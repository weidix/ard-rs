use flate2::{Decompress, FlushDecompress, Status};

use crate::mvs::MvsState;
use crate::wire::Cursor;
use crate::{
    ArdEncryptionControl, Encoding, Error, Framebuffer, PixelFormat, Rectangle, Result,
    media_stream::MediaStreamServerReply, parse_ard_encryption_control,
};

const MAX_REUSABLE_ZRLE_SCRATCH: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_rectangles: usize,
    pub max_compressed_bytes: usize,
    pub max_decompressed_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_rectangles: 4096,
            max_compressed_bytes: 64 * 1024 * 1024,
            max_decompressed_bytes: 256 * 1024 * 1024,
        }
    }
}

pub struct Decoder {
    pixel_format: PixelFormat,
    limits: DecodeLimits,
    streams: [Decompress; 5],
    zlib_scratch: [Vec<u8>; 5],
    zrle_palette_scratch: Vec<[u8; 4]>,
    zrle_pixels_scratch: Vec<[u8; 4]>,
    mvs: MvsState,
    pending_encryption_control: Option<ArdEncryptionControl>,
    pending_media_stream_replies: Vec<MediaStreamServerReply>,
    gpu_mvs_output: bool,
    pending_gpu_mvs_frames: Vec<crate::MvsGpuFrame>,
}

impl Decoder {
    pub fn new(pixel_format: PixelFormat) -> Result<Self> {
        Self::with_limits(pixel_format, DecodeLimits::default())
    }

    pub fn with_limits(pixel_format: PixelFormat, limits: DecodeLimits) -> Result<Self> {
        let _ = pixel_format.bytes_per_pixel()?;
        Ok(Self {
            pixel_format,
            limits,
            // Apple keeps independent persistent streams for halftone,
            // grayscale, thousands, full-colour zlib, and ZRLE.
            streams: core::array::from_fn(|_| Decompress::new(true)),
            zlib_scratch: core::array::from_fn(|_| Vec::new()),
            zrle_palette_scratch: Vec::new(),
            zrle_pixels_scratch: Vec::new(),
            mvs: MvsState::default(),
            pending_encryption_control: None,
            pending_media_stream_replies: Vec::new(),
            gpu_mvs_output: false,
            pending_gpu_mvs_frames: Vec::new(),
        })
    }

    /// Enables GPU-native MVS output. Protocol parsing, entropy decoding and
    /// state reconstruction remain on the CPU, but inverse DCT and full-frame
    /// pixel expansion are omitted. Call [`Self::take_gpu_mvs_frames`] after
    /// dispatching server messages.
    pub fn new_gpu_mvs(pixel_format: PixelFormat) -> Result<Self> {
        let mut decoder = Self::new(pixel_format)?;
        decoder.gpu_mvs_output = true;
        Ok(decoder)
    }

    pub fn take_gpu_mvs_frames(&mut self) -> Vec<crate::MvsGpuFrame> {
        core::mem::take(&mut self.pending_gpu_mvs_frames)
    }

    /// Delivers pending GPU frames while retaining the decoder's outer frame
    /// vector allocation for the next update.
    pub fn drain_gpu_mvs_frames(&mut self, mut visit: impl FnMut(crate::MvsGpuFrame)) {
        for frame in self.pending_gpu_mvs_frames.drain(..) {
            visit(frame);
        }
    }

    pub fn limits(&self) -> DecodeLimits {
        self.limits
    }

    pub fn decode_rectangle(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
    ) -> Result<usize> {
        self.decode_rectangle_impl(rect, payload, framebuffer, true)
    }

    /// Decodes a rectangle whose complete wire payload has already been
    /// framed by the message dispatcher. A malformed live stream terminates
    /// the session, so this path can update the large persistent MVS state in
    /// place instead of cloning every framebuffer tile and cache entry first.
    pub(crate) fn decode_complete_rectangle(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
    ) -> Result<usize> {
        self.decode_rectangle_impl(rect, payload, framebuffer, false)
    }

    fn decode_rectangle_impl(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
        transactional_mvs: bool,
    ) -> Result<usize> {
        let encoding =
            Encoding::from_i32(rect.encoding).ok_or(Error::UnsupportedEncoding(rect.encoding))?;
        framebuffer.validate_pixel_format(self.pixel_format)?;
        if encoding == Encoding::DesktopSize {
            framebuffer.resize(rect.width, rect.height)?;
            return Ok(0);
        }
        if encoding == Encoding::ArdEncryption {
            if rect.x != 0 || rect.y != 0 || rect.width != 0 || rect.height != 0 {
                return Err(Error::Invalid(
                    "ARD encryption control rectangle is not zero-sized",
                ));
            }
            let (control, consumed) = parse_ard_encryption_control(payload)?;
            self.pending_encryption_control = Some(control);
            return Ok(consumed);
        }
        if encoding == Encoding::ArdAvcMediaStream {
            if rect.x != 0 || rect.y != 0 {
                return Err(Error::Invalid(
                    "ARD AVC media-stream rectangle is not at the origin",
                ));
            }
            let (message, consumed) =
                MediaStreamServerReply::parse_rectangle_payload_with_len(payload)?;
            self.pending_media_stream_replies.push(message);
            return Ok(consumed);
        }
        if encoding == Encoding::CursorPosition {
            // Screen Sharing's 1100 rectangle carries the pointer hotspot in
            // the rectangle header and has no payload. The viewer draws its
            // own cursor overlay from this position; the core only needs to
            // tolerate the rectangle so it does not abort the whole update.
            return Ok(0);
        }
        framebuffer.validate_rect(&rect)?;
        if rect.width == 0 || rect.height == 0 {
            if encoding == Encoding::ArdMvs {
                return self.decode_mvs(rect, payload, framebuffer, transactional_mvs);
            }
            return Ok(0);
        }
        match encoding {
            Encoding::Raw => self.decode_raw(rect, payload, framebuffer),
            Encoding::CopyRect => self.decode_copy_rect(rect, payload, framebuffer),
            Encoding::Zlib => self.decode_apple_zlib(rect, payload, framebuffer, 3),
            Encoding::Zrle => self.decode_zrle(rect, payload, framebuffer),
            Encoding::ArdHalftone => self.decode_apple_zlib(rect, payload, framebuffer, 0),
            Encoding::ArdGrayscale => self.decode_apple_zlib(rect, payload, framebuffer, 1),
            Encoding::ArdThousands => self.decode_apple_zlib(rect, payload, framebuffer, 2),
            Encoding::ArdMvs => self.decode_mvs(rect, payload, framebuffer, transactional_mvs),
            Encoding::DesktopSize | Encoding::ArdEncryption | Encoding::CursorPosition => {
                unreachable!("handled before rectangle validation")
            }
            Encoding::ArdAvcMediaStream => unreachable!("handled before rectangle validation"),
        }
    }

    /// Returns the exact encoded payload length without mutating decoder
    /// state. This lets the streaming dispatcher wait for a whole update
    /// before entering the expensive stateful decoder.
    pub(crate) fn complete_rectangle_payload_len(
        &self,
        rect: Rectangle,
        payload: &[u8],
    ) -> Result<usize> {
        let encoding =
            Encoding::from_i32(rect.encoding).ok_or(Error::UnsupportedEncoding(rect.encoding))?;
        if (rect.width == 0 || rect.height == 0)
            && !matches!(
                encoding,
                Encoding::ArdMvs | Encoding::ArdEncryption | Encoding::ArdAvcMediaStream
            )
        {
            return Ok(0);
        }
        match encoding {
            Encoding::DesktopSize | Encoding::CursorPosition => Ok(0),
            Encoding::ArdEncryption => {
                if payload.len() < ArdEncryptionControl::WIRE_LEN {
                    Err(Error::NeedMore {
                        needed: ArdEncryptionControl::WIRE_LEN,
                        available: payload.len(),
                    })
                } else {
                    Ok(ArdEncryptionControl::WIRE_LEN)
                }
            }
            Encoding::ArdAvcMediaStream => {
                let (_, consumed) =
                    MediaStreamServerReply::parse_rectangle_payload_with_len(payload)?;
                Ok(consumed)
            }
            Encoding::CopyRect => fixed_payload_len(payload, 4),
            Encoding::Raw => {
                let len = pixel_count(rect)?
                    .checked_mul(self.pixel_format.bytes_per_pixel()?)
                    .ok_or(Error::LimitExceeded("raw rectangle"))?;
                if len > self.limits.max_decompressed_bytes {
                    return Err(Error::LimitExceeded("raw rectangle"));
                }
                fixed_payload_len(payload, len)
            }
            Encoding::Zlib
            | Encoding::Zrle
            | Encoding::ArdHalftone
            | Encoding::ArdGrayscale
            | Encoding::ArdThousands
            | Encoding::ArdMvs => {
                if payload.len() < 4 {
                    return Err(Error::NeedMore {
                        needed: 4,
                        available: payload.len(),
                    });
                }
                let encoded = usize::try_from(u32::from_be_bytes(
                    payload[..4].try_into().expect("length prefix checked"),
                ))
                .map_err(|_| Error::LimitExceeded("compressed rectangle"))?;
                if encoded > self.limits.max_compressed_bytes {
                    return Err(Error::LimitExceeded("compressed rectangle"));
                }
                let len = encoded
                    .checked_add(4)
                    .ok_or(Error::LimitExceeded("compressed rectangle"))?;
                fixed_payload_len(payload, len)
            }
        }
    }

    pub fn take_ard_encryption_control(&mut self) -> Option<ArdEncryptionControl> {
        self.pending_encryption_control.take()
    }

    /// Takes AVC media-stream control messages recovered from 1010
    /// bootstrap rectangles. Encoded video is delivered over UDP.
    pub fn take_media_stream_replies(&mut self) -> Vec<MediaStreamServerReply> {
        core::mem::take(&mut self.pending_media_stream_replies)
    }

    /// Returns the two 8x8 quantization tables most recently supplied by an
    /// MVS type-2 control update.
    pub fn mvs_quantization_tables(&self) -> (&[u16; 64], &[u16; 64]) {
        self.mvs.quantization_tables()
    }

    fn decode_mvs(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
        transactional: bool,
    ) -> Result<usize> {
        let mut cursor = Cursor::new(payload);
        let update_len =
            usize::try_from(cursor.u32()?).map_err(|_| Error::LimitExceeded("ARD MVS update"))?;
        if update_len > self.limits.max_compressed_bytes {
            return Err(Error::LimitExceeded("ARD MVS update"));
        }
        let update = cursor.take(update_len)?;
        if transactional {
            let mut next_mvs = self.mvs.clone();
            let frame = Self::decode_mvs_update(
                &mut next_mvs,
                rect,
                update,
                framebuffer,
                self.limits.max_decompressed_bytes,
                self.gpu_mvs_output,
                transactional,
            )?;
            self.mvs = next_mvs;
            if let Some(frame) = frame {
                self.pending_gpu_mvs_frames.push(frame);
            }
        } else {
            let frame = Self::decode_mvs_update(
                &mut self.mvs,
                rect,
                update,
                framebuffer,
                self.limits.max_decompressed_bytes,
                self.gpu_mvs_output,
                transactional,
            )?;
            if let Some(frame) = frame {
                self.pending_gpu_mvs_frames.push(frame);
            }
        }
        Ok(cursor.position())
    }

    fn decode_mvs_update(
        mvs: &mut MvsState,
        rect: Rectangle,
        update: &[u8],
        framebuffer: &mut Framebuffer,
        max_decompressed_bytes: usize,
        gpu_mvs_output: bool,
        transactional: bool,
    ) -> Result<Option<crate::MvsGpuFrame>> {
        let update_type = *update
            .first()
            .ok_or(Error::Invalid("empty ARD MVS update"))?;
        match update_type {
            2 => mvs.decode_control_update(update)?,
            0 => {
                if rect.width == 0 || rect.height == 0 {
                    return Err(Error::Invalid("zero-sized ARD MVS image update"));
                }
                let tiles = mvs.decode_partial_update(
                    rect,
                    update,
                    framebuffer,
                    max_decompressed_bytes,
                    !gpu_mvs_output,
                )?;
                if gpu_mvs_output {
                    if tiles.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(crate::MvsGpuFrame {
                        framebuffer_width: framebuffer.width(),
                        framebuffer_height: framebuffer.height(),
                        luminance_quantization: *mvs.quantization_tables().0,
                        chrominance_quantization: *mvs.quantization_tables().1,
                        tiles,
                    }));
                }
            }
            1 => {
                if rect.width == 0 || rect.height == 0 {
                    return Err(Error::Invalid("zero-sized ARD MVS image update"));
                }
                let tiles = if transactional && !gpu_mvs_output {
                    let mut next_framebuffer = framebuffer.clone();
                    let tiles = mvs.decode_full_update(
                        rect,
                        update,
                        &mut next_framebuffer,
                        max_decompressed_bytes,
                        true,
                    )?;
                    *framebuffer = next_framebuffer;
                    tiles
                } else {
                    // Complete live messages are already fully framed. The
                    // non-transactional path can decode directly into the
                    // framebuffer; a failed live decode terminates the
                    // session, so retaining a full-frame rollback copy only
                    // wastes CPU and memory. GPU MVS also has no CPU pixels
                    // to clone in the first place.
                    mvs.decode_full_update(
                        rect,
                        update,
                        framebuffer,
                        max_decompressed_bytes,
                        !gpu_mvs_output,
                    )?
                };
                if gpu_mvs_output {
                    if tiles.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(crate::MvsGpuFrame {
                        framebuffer_width: framebuffer.width(),
                        framebuffer_height: framebuffer.height(),
                        luminance_quantization: *mvs.quantization_tables().0,
                        chrominance_quantization: *mvs.quantization_tables().1,
                        tiles,
                    }));
                }
            }
            _ => return Err(Error::Invalid("invalid ARD MVS update type")),
        }
        Ok(None)
    }

    fn decode_copy_rect(
        &self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
    ) -> Result<usize> {
        let mut cursor = Cursor::new(payload);
        let src_x = cursor.u16()?;
        let src_y = cursor.u16()?;
        framebuffer.copy_rect(&rect, src_x, src_y)?;
        Ok(cursor.position())
    }

    fn decode_raw(
        &self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
    ) -> Result<usize> {
        let bpp = self.pixel_format.bytes_per_pixel()?;
        let expected = pixel_count(rect)?
            .checked_mul(bpp)
            .ok_or(Error::LimitExceeded("raw rectangle"))?;
        if expected > self.limits.max_decompressed_bytes {
            return Err(Error::LimitExceeded("raw rectangle"));
        }
        if payload.len() < expected {
            return Err(Error::NeedMore {
                needed: expected,
                available: payload.len(),
            });
        }
        blit_pixels(
            self.pixel_format,
            rect,
            &payload[..expected],
            bpp,
            framebuffer,
        )?;
        Ok(expected)
    }

    fn decode_apple_zlib(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
        stream: usize,
    ) -> Result<usize> {
        let mut cursor = Cursor::new(payload);
        let compressed_len = usize::try_from(cursor.u32()?)
            .map_err(|_| Error::LimitExceeded("compressed rectangle"))?;
        if compressed_len > self.limits.max_compressed_bytes {
            return Err(Error::LimitExceeded("compressed rectangle"));
        }
        let compressed = cursor.take(compressed_len)?;
        let row_bytes = match stream {
            0 => usize::from(rect.width).div_ceil(8),
            1 => usize::from(rect.width).div_ceil(2),
            2 => usize::from(rect.width)
                .checked_mul(2)
                .ok_or(Error::LimitExceeded("ARD thousands row"))?,
            3 => usize::from(rect.width)
                .checked_mul(self.pixel_format.bytes_per_pixel()?)
                .ok_or(Error::LimitExceeded("zlib row"))?,
            _ => unreachable!(),
        };
        let expected = row_bytes
            .checked_mul(usize::from(rect.height))
            .ok_or(Error::LimitExceeded("decompressed rectangle"))?;
        let pixel_format = self.pixel_format;
        let decoded = decompress_exact(
            &mut self.streams[stream],
            compressed,
            expected,
            self.limits.max_decompressed_bytes,
            &mut self.zlib_scratch[stream],
        )?;
        match stream {
            0 => blit_halftone(rect, decoded, row_bytes, framebuffer),
            1 => blit_grayscale(rect, decoded, row_bytes, framebuffer),
            2 => blit_rgb555(rect, decoded, framebuffer),
            3 => blit_pixels(
                pixel_format,
                rect,
                decoded,
                pixel_format.bytes_per_pixel()?,
                framebuffer,
            ),
            _ => unreachable!(),
        }?;
        Ok(cursor.position())
    }

    fn decode_zrle(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
    ) -> Result<usize> {
        let mut cursor = Cursor::new(payload);
        let compressed_len = usize::try_from(cursor.u32()?)
            .map_err(|_| Error::LimitExceeded("ZRLE compressed data"))?;
        if compressed_len > self.limits.max_compressed_bytes {
            return Err(Error::LimitExceeded("ZRLE compressed data"));
        }
        let compressed = cursor.take(compressed_len)?;
        let pixel_format = self.pixel_format;
        let decoded = decompress_available(
            &mut self.streams[4],
            compressed,
            self.limits.max_decompressed_bytes,
            &mut self.zlib_scratch[4],
        )?;
        let result = Self::blit_zrle_tiles(
            pixel_format,
            rect,
            decoded,
            framebuffer,
            &mut self.zrle_palette_scratch,
            &mut self.zrle_pixels_scratch,
        );
        if self.zlib_scratch[4].capacity() > MAX_REUSABLE_ZRLE_SCRATCH {
            // Very large one-off frames should not permanently pin a full
            // decompressed framebuffer-sized buffer in the decoder.
            self.zlib_scratch[4] = Vec::new();
        }
        result?;
        Ok(cursor.position())
    }

    fn blit_zrle_tiles(
        pixel_format: PixelFormat,
        rect: Rectangle,
        bytes: &[u8],
        framebuffer: &mut Framebuffer,
        palette: &mut Vec<[u8; 4]>,
        pixels: &mut Vec<[u8; 4]>,
    ) -> Result<()> {
        let mut cursor = Cursor::new(bytes);
        let cpixel = compact_pixel_bytes(pixel_format)?;
        for tile_y in (0..rect.height).step_by(64) {
            let tile_height = (rect.height - tile_y).min(64);
            for tile_x in (0..rect.width).step_by(64) {
                let tile_width = (rect.width - tile_x).min(64);
                let count = usize::from(tile_width) * usize::from(tile_height);
                let mode = cursor.u8()?;
                let rle = mode & 0x80 != 0;
                let palette_size = usize::from(mode & 0x7f);
                if palette_size > 127 {
                    return Err(Error::Invalid("invalid ZRLE palette size"));
                }
                palette.clear();
                for _ in 0..palette_size {
                    palette.push(decode_compact_pixel(pixel_format, cursor.take(cpixel)?)?);
                }
                pixels.clear();
                match (rle, palette_size) {
                    (false, 0) => {
                        for _ in 0..count {
                            pixels.push(decode_compact_pixel(pixel_format, cursor.take(cpixel)?)?);
                        }
                    }
                    (false, 1) => pixels.resize(count, palette[0]),
                    (false, 2..=16) => {
                        let bits = if palette_size <= 2 {
                            1
                        } else if palette_size <= 4 {
                            2
                        } else {
                            4
                        };
                        unpack_palette_indices(
                            &mut cursor,
                            tile_width,
                            tile_height,
                            bits,
                            palette,
                            pixels,
                        )?;
                    }
                    (true, 0) => {
                        while pixels.len() < count {
                            let pixel = decode_compact_pixel(pixel_format, cursor.take(cpixel)?)?;
                            let run = read_run_length(&mut cursor)?;
                            append_run(pixels, pixel, run, count)?;
                        }
                    }
                    (true, 1..=127) => {
                        while pixels.len() < count {
                            let index = cursor.u8()?;
                            let has_run = index & 0x80 != 0;
                            let palette_index = usize::from(index & 0x7f);
                            let pixel = *palette
                                .get(palette_index)
                                .ok_or(Error::Invalid("ZRLE palette index out of range"))?;
                            let run = if has_run {
                                read_run_length(&mut cursor)?
                            } else {
                                1
                            };
                            append_run(pixels, pixel, run, count)?;
                        }
                    }
                    _ => return Err(Error::Invalid("unknown ZRLE subencoding")),
                }
                for (index, &pixel) in pixels.iter().enumerate() {
                    let x = index % usize::from(tile_width);
                    let y = index / usize::from(tile_width);
                    framebuffer.set_pixel(
                        rect.x + tile_x + x as u16,
                        rect.y + tile_y + y as u16,
                        pixel,
                    );
                }
            }
        }
        if cursor.remaining() != 0 {
            return Err(Error::Invalid("trailing ZRLE tile data"));
        }
        Ok(())
    }
}

fn decode_compact_pixel(pixel_format: PixelFormat, bytes: &[u8]) -> Result<[u8; 4]> {
    let bpp = pixel_format.bytes_per_pixel()?;
    if bpp != 4 || compact_pixel_bytes(pixel_format)? == 4 {
        return pixel_format.decode_pixel(bytes);
    }
    let omitted_numeric_lane = compact_omitted_lane(pixel_format)?;
    let mut expanded = [0; 4];
    let mut compact_index = 0;
    for (wire_index, slot) in expanded.iter_mut().enumerate() {
        let numeric_lane = if pixel_format.big_endian {
            3 - wire_index
        } else {
            wire_index
        };
        if numeric_lane != omitted_numeric_lane {
            *slot = bytes[compact_index];
            compact_index += 1;
        }
    }
    pixel_format.decode_pixel(&expanded)
}

fn pixel_count(rect: Rectangle) -> Result<usize> {
    usize::from(rect.width)
        .checked_mul(usize::from(rect.height))
        .ok_or(Error::LimitExceeded("rectangle pixel count"))
}

fn fixed_payload_len(payload: &[u8], needed: usize) -> Result<usize> {
    if payload.len() < needed {
        Err(Error::NeedMore {
            needed,
            available: payload.len(),
        })
    } else {
        Ok(needed)
    }
}

fn decompress_exact<'a>(
    stream: &mut Decompress,
    input: &[u8],
    expected: usize,
    max: usize,
    output: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    if expected > max {
        return Err(Error::LimitExceeded("decompressed rectangle"));
    }
    let output_len = expected
        .checked_add(1)
        .ok_or(Error::LimitExceeded("decompressed rectangle"))?;
    output.resize(output_len, 0);
    let before_in = stream.total_in();
    let before_out = stream.total_out();
    let status = stream
        .decompress(input, output, FlushDecompress::Sync)
        .map_err(|_| Error::Decompression)?;
    let consumed =
        usize::try_from(stream.total_in() - before_in).map_err(|_| Error::Decompression)?;
    let produced =
        usize::try_from(stream.total_out() - before_out).map_err(|_| Error::Decompression)?;
    if consumed != input.len() || produced != expected || status == Status::BufError {
        return Err(Error::Decompression);
    }
    output.truncate(expected);
    Ok(output.as_slice())
}

fn decompress_available<'a>(
    stream: &mut Decompress,
    input: &[u8],
    max: usize,
    output: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    output.clear();
    let mut consumed = 0;
    while consumed < input.len() {
        if output.len() == max {
            return Err(Error::LimitExceeded("decompressed ZRLE data"));
        }
        let old_len = output.len();
        let grow = (max - old_len).min(64 * 1024);
        output.resize(old_len + grow, 0);
        let before_in = stream.total_in();
        let before_out = stream.total_out();
        let status = stream
            .decompress(
                &input[consumed..],
                &mut output[old_len..],
                FlushDecompress::Sync,
            )
            .map_err(|_| Error::Decompression)?;
        let used =
            usize::try_from(stream.total_in() - before_in).map_err(|_| Error::Decompression)?;
        let made =
            usize::try_from(stream.total_out() - before_out).map_err(|_| Error::Decompression)?;
        consumed += used;
        output.truncate(old_len + made);
        if used == 0 && made == 0 {
            if status == Status::BufError {
                break;
            }
            return Err(Error::Decompression);
        }
    }
    if consumed != input.len() {
        return Err(Error::Decompression);
    }
    Ok(output.as_slice())
}

fn blit_pixels(
    pixel_format: PixelFormat,
    rect: Rectangle,
    bytes: &[u8],
    bpp: usize,
    framebuffer: &mut Framebuffer,
) -> Result<()> {
    for row in 0..rect.height {
        for column in 0..rect.width {
            let index = (usize::from(row) * usize::from(rect.width) + usize::from(column)) * bpp;
            framebuffer.set_pixel_bytes(
                rect.x + column,
                rect.y + row,
                pixel_format,
                &bytes[index..index + bpp],
            )?;
        }
    }
    Ok(())
}

fn blit_halftone(
    rect: Rectangle,
    bytes: &[u8],
    row_bytes: usize,
    framebuffer: &mut Framebuffer,
) -> Result<()> {
    for row in 0..rect.height {
        for column in 0..rect.width {
            let byte = bytes[usize::from(row) * row_bytes + usize::from(column / 8)];
            let white = byte & (0x80 >> (column % 8)) != 0;
            let value = if white { 255 } else { 0 };
            framebuffer.set_pixel(rect.x + column, rect.y + row, [value, value, value, 255]);
        }
    }
    Ok(())
}

fn blit_grayscale(
    rect: Rectangle,
    bytes: &[u8],
    row_bytes: usize,
    framebuffer: &mut Framebuffer,
) -> Result<()> {
    for row in 0..rect.height {
        for column in 0..rect.width {
            let byte = bytes[usize::from(row) * row_bytes + usize::from(column / 2)];
            let nibble = if column % 2 == 0 {
                byte >> 4
            } else {
                byte & 0xf
            };
            // This exactly matches Apple's tables: n * 0x10 for 0..14,
            // with 15 promoted to full white.
            let value = if nibble == 15 { 255 } else { nibble * 16 };
            framebuffer.set_pixel(rect.x + column, rect.y + row, [value, value, value, 255]);
        }
    }
    Ok(())
}

fn blit_rgb555(rect: Rectangle, bytes: &[u8], framebuffer: &mut Framebuffer) -> Result<()> {
    for row in 0..rect.height {
        for column in 0..rect.width {
            let index = (usize::from(row) * usize::from(rect.width) + usize::from(column)) * 2;
            let value = u16::from_be_bytes([bytes[index], bytes[index + 1]]);
            let r = ((value >> 10) & 0x1f) as u8;
            let g = ((value >> 5) & 0x1f) as u8;
            let b = (value & 0x1f) as u8;
            framebuffer.set_pixel(
                rect.x + column,
                rect.y + row,
                [expand_five(r), expand_five(g), expand_five(b), 255],
            );
        }
    }
    Ok(())
}

fn expand_five(value: u8) -> u8 {
    (u16::from(value) * 255 / 31) as u8
}

fn compact_pixel_bytes(format: PixelFormat) -> Result<usize> {
    let bpp = format.bytes_per_pixel()?;
    if bpp == 4 && format.depth <= 24 && compact_omitted_lane(format).is_ok() {
        Ok(3)
    } else {
        Ok(bpp)
    }
}

fn compact_omitted_lane(format: PixelFormat) -> Result<usize> {
    let channel_bits = |max: u16| u32::from(max).ilog2() + 1;
    let low = [format.red_shift, format.green_shift, format.blue_shift]
        .into_iter()
        .min()
        .expect("three channels");
    let high = [
        u32::from(format.red_shift) + channel_bits(format.red_max),
        u32::from(format.green_shift) + channel_bits(format.green_max),
        u32::from(format.blue_shift) + channel_bits(format.blue_max),
    ]
    .into_iter()
    .max()
    .expect("three channels");
    if high <= 24 {
        Ok(3)
    } else if low >= 8 && high <= 32 {
        Ok(0)
    } else {
        Err(Error::Invalid(
            "pixel channels cannot use compact ZRLE form",
        ))
    }
}

fn read_run_length(cursor: &mut Cursor<'_>) -> Result<usize> {
    let mut length = 1_usize;
    loop {
        let byte = cursor.u8()?;
        length = length
            .checked_add(usize::from(byte))
            .ok_or(Error::LimitExceeded("ZRLE run length"))?;
        if byte != 255 {
            return Ok(length);
        }
    }
}

fn append_run(pixels: &mut Vec<[u8; 4]>, pixel: [u8; 4], run: usize, limit: usize) -> Result<()> {
    let end = pixels
        .len()
        .checked_add(run)
        .ok_or(Error::LimitExceeded("ZRLE run"))?;
    if end > limit {
        return Err(Error::Invalid("ZRLE run exceeds tile"));
    }
    pixels.resize(end, pixel);
    Ok(())
}

fn unpack_palette_indices(
    cursor: &mut Cursor<'_>,
    width: u16,
    height: u16,
    bits: u8,
    palette: &[[u8; 4]],
    pixels: &mut Vec<[u8; 4]>,
) -> Result<()> {
    let mask = (1_u8 << bits) - 1;
    let per_byte = 8 / bits;
    for _ in 0..height {
        let mut column = 0_u16;
        while column < width {
            let byte = cursor.u8()?;
            for slot in 0..per_byte {
                if column == width {
                    break;
                }
                let shift = 8 - bits * (slot + 1);
                let index = usize::from((byte >> shift) & mask);
                pixels.push(
                    *palette
                        .get(index)
                        .ok_or(Error::Invalid("ZRLE palette index out of range"))?,
                );
                column += 1;
            }
        }
    }
    Ok(())
}
