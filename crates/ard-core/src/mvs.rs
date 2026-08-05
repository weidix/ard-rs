use std::sync::Arc;

use crate::wire::Cursor;
use crate::{Error, Framebuffer, Rectangle, Result};

pub(crate) const QUANTIZATION_UPDATE_LEN: usize = 129;
const DCT_CACHE_LAST_INDEX: u16 = 64_999;

const DEFAULT_LUMINANCE_QUANTIZATION: [u16; 64] = [
    0x10, 0x0b, 0x0b, 0x0e, 0x18, 0x16, 0x18, 0x21, 0x0b, 0x0c, 0x0b, 0x0b, 0x1a, 0x12, 0x13, 0x17,
    0x0d, 0x0d, 0x0e, 0x18, 0x14, 0x15, 0x16, 0x1c, 0x0d, 0x11, 0x0e, 0x15, 0x17, 0x19, 0x22, 0x22,
    0x11, 0x16, 0x12, 0x19, 0x1f, 0x20, 0x22, 0x2c, 0x18, 0x14, 0x11, 0x1b, 0x23, 0x28, 0x2c, 0x36,
    0x1e, 0x23, 0x1c, 0x1e, 0x22, 0x2c, 0x36, 0x40, 0x23, 0x1e, 0x21, 0x26, 0x2d, 0x37, 0x41, 0x4b,
];

const DEFAULT_CHROMINANCE_QUANTIZATION: [u16; 64] = [
    0x13, 0x13, 0x18, 0x2f, 0x4c, 0x63, 0x63, 0x63, 0x13, 0x15, 0x1a, 0x42, 0x63, 0x63, 0x63, 0x63,
    0x18, 0x1a, 0x38, 0x63, 0x63, 0x63, 0x63, 0x63, 0x2f, 0x42, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
    0x4c, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
    0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63, 0x63,
];

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const IDCT_BASIS: [[i32; 8]; 8] = [
    [11585, 11585, 11585, 11585, 11585, 11585, 11585, 11585],
    [16069, 13623, 9102, 3196, -3196, -9102, -13623, -16069],
    [15137, 6270, -6270, -15137, -15137, -6270, 6270, 15137],
    [13623, -3196, -16069, -9102, 9102, 16069, 3196, -13623],
    [11585, -11585, -11585, 11585, 11585, -11585, -11585, 11585],
    [9102, -16069, 3196, 13623, -13623, -3196, 16069, -9102],
    [6270, -15137, 15137, -6270, -6270, 15137, -15137, 6270],
    [3196, -9102, 13623, -16069, 16069, -13623, 9102, -3196],
];

// The native full-update decoder initializes one derived AC table from
// std_huff_tables.bits_ac_luminance/val_ac_luminance and uses it for both
// chroma components. It does not select the JPEG chrominance AC table here.
const LUMINANCE_AC_BITS: [u8; 17] = [
    0x00, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01,
    0x7d,
];

const LUMINANCE_AC_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

#[derive(Debug, Clone)]
struct DctTile {
    coefficients: Arc<[[i16; 64]; 3]>,
}

/// GPU-facing content of one decoded 8×8 MVS tile. DCT tiles retain the
/// codec coefficients and are not inverse-transformed on the CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MvsGpuTile {
    SolidYcbcr([u8; 3]),
    SolidRgba([u8; 4]),
    PixelsYcbcr(Box<[[u8; 3]; 64]>),
    PixelsRgba(Box<[[u8; 4]; 64]>),
    /// Partial-update Rice tile: luminance uses IDCT while chroma contains
    /// DC-only samples expanded with the codec's dedicated rounding rule.
    // These coefficient blocks are shared between the decoder's tile cache,
    // MVS state, and the latest frame mailbox. Cloning an Arc keeps the hot
    // copy/cache paths from duplicating 384 coefficients per tile.
    RiceDct(Arc<[[i16; 64]; 3]>),
    Dct(Arc<[[i16; 64]; 3]>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvsGpuTileUpdate {
    pub x: u16,
    pub y: u16,
    pub width: u8,
    pub height: u8,
    pub tile: MvsGpuTile,
}

/// One MVS update ready for GPU tile expansion and presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvsGpuFrame {
    pub framebuffer_width: u16,
    pub framebuffer_height: u16,
    pub luminance_quantization: [u16; 64],
    pub chrominance_quantization: [u16; 64],
    pub tiles: Vec<MvsGpuTileUpdate>,
}

/// A literal color carried by a non-DCT MVS tile. This is tile command data,
/// not an intermediate full-frame image.
#[derive(Debug, Clone, Copy)]
enum MvsPixel {
    Rgba([u8; 4]),
    Ycbcr([u8; 3]),
}

impl Default for DctTile {
    fn default() -> Self {
        Self {
            coefficients: Arc::new([[0; 64]; 3]),
        }
    }
}

#[derive(Debug, Clone)]
struct TileState {
    generation: u32,
    copy_source: Option<usize>,
    dct: DctTile,
    luma_count: u8,
    gpu_tile: MvsGpuTile,
}

impl Default for TileState {
    fn default() -> Self {
        Self {
            generation: 0,
            copy_source: None,
            dct: DctTile::default(),
            // DecodeMVSUpdate allocates the native 0x54-byte tile-state
            // array with calloc. A tile can therefore be used as a
            // differential baseline before any Rice/DCT block is received.
            luma_count: 0,
            gpu_tile: MvsGpuTile::SolidRgba([0, 0, 0, 255]),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MvsState {
    luminance_quantization: [u16; 64],
    chrominance_quantization: [u16; 64],
    framebuffer_size: (u16, u16),
    tiles_wide: usize,
    tiles: Vec<TileState>,
    generation: u32,
    // MVS cache indexes are a bounded dense ring. Keeping the slots directly
    // avoids hash metadata and makes every cache recall a single indexed
    // lookup. The vector stays empty until the first DCT tile is cached, so
    // streams that never use the cache pay no allocation cost.
    cache: Vec<Option<DctTile>>,
    cache_write_index: u16,
    last_cache_reference: u16,
    cache_insertions: u32,
}

impl Default for MvsState {
    fn default() -> Self {
        Self {
            luminance_quantization: DEFAULT_LUMINANCE_QUANTIZATION,
            chrominance_quantization: DEFAULT_CHROMINANCE_QUANTIZATION,
            framebuffer_size: (0, 0),
            tiles_wide: 0,
            tiles: Vec::new(),
            generation: 0,
            cache: Vec::new(),
            cache_write_index: 0,
            last_cache_reference: 0,
            cache_insertions: 0,
        }
    }
}

impl MvsState {
    fn ensure_framebuffer_state(&mut self, framebuffer: &Framebuffer) -> Result<()> {
        let size = (framebuffer.width(), framebuffer.height());
        if size == self.framebuffer_size {
            return Ok(());
        }
        let tiles_wide = usize::from(size.0).div_ceil(8);
        let tile_count = tiles_wide
            .checked_mul(usize::from(size.1).div_ceil(8))
            .ok_or(Error::LimitExceeded("ARD MVS framebuffer tile count"))?;
        self.framebuffer_size = size;
        self.tiles_wide = tiles_wide;
        if self.tiles.capacity() > tile_count {
            // A DesktopSize reduction should release the old large tile
            // state instead of retaining its peak allocation indefinitely.
            self.tiles = Vec::new();
        } else {
            self.tiles.clear();
        }
        self.tiles
            .try_reserve_exact(tile_count)
            .map_err(|_| Error::LimitExceeded("ARD MVS framebuffer tile count"))?;
        self.tiles.resize(tile_count, TileState::default());
        self.generation = 0;
        Ok(())
    }

    fn insert_cache_tile(&mut self, tile: DctTile) {
        let index = next_cache_index(self.cache_write_index);
        self.cache_write_index = index;
        self.cache_insertions = self.cache_insertions.saturating_add(1);
        let slot = usize::from(index);
        if self.cache.len() <= slot {
            self.cache.resize_with(slot + 1, || None);
        }
        self.cache[slot] = Some(tile);
    }

    fn cached_tile(&mut self, index: u16) -> Option<DctTile> {
        if index == 0 || index > DCT_CACHE_LAST_INDEX || u32::from(index) > self.cache_insertions {
            return None;
        }
        let tile = self.cache.get(usize::from(index))?.as_ref()?.clone();
        self.last_cache_reference = index;
        Some(tile)
    }

    pub(crate) fn quantization_tables(&self) -> (&[u16; 64], &[u16; 64]) {
        (&self.luminance_quantization, &self.chrominance_quantization)
    }

    pub(crate) fn decode_control_update(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() != QUANTIZATION_UPDATE_LEN {
            return Err(Error::Invalid(
                "ARD MVS quantization update must contain 129 bytes",
            ));
        }

        let mut cursor = Cursor::new(payload);
        if cursor.u8()? != 2 {
            return Err(Error::Invalid("not an ARD MVS quantization update"));
        }
        for value in &mut self.luminance_quantization {
            *value = u16::from(cursor.u8()?);
        }
        for value in &mut self.chrominance_quantization {
            *value = u16::from(cursor.u8()?);
        }
        Ok(())
    }

    pub(crate) fn decode_partial_update(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
        max_output_bytes: usize,
        render_cpu: bool,
    ) -> Result<Vec<MvsGpuTileUpdate>> {
        self.ensure_framebuffer_state(framebuffer)?;
        if payload.len() < 6 {
            return Err(Error::NeedMore {
                needed: 6,
                available: payload.len(),
            });
        }
        if payload[0] != 0 {
            return Err(Error::Invalid("not an ARD MVS partial update"));
        }
        let secondary_offset = (usize::from(payload[3]) << 16)
            | (usize::from(payload[4]) << 8)
            | usize::from(payload[5]);
        if !(6..=payload.len()).contains(&secondary_offset) {
            return Err(Error::Invalid("invalid ARD MVS secondary bitstream offset"));
        }

        let pixel_count = usize::from(rect.width)
            .checked_mul(usize::from(rect.height))
            .ok_or(Error::LimitExceeded("ARD MVS rectangle"))?;
        let output_bytes = pixel_count
            .checked_mul(4)
            .ok_or(Error::LimitExceeded("ARD MVS rectangle"))?;
        if output_bytes > max_output_bytes {
            return Err(Error::LimitExceeded("ARD MVS rectangle"));
        }
        let mut primary = BitReader::new(&payload[6..secondary_offset]);
        let mut secondary = BitReader::new(&payload[secondary_offset..]);
        let _initial_state = primary.read(1)?;
        let tiles_wide = usize::from(rect.width).div_ceil(8);
        let tiles_high = usize::from(rect.height).div_ceil(8);
        let total_tiles = tiles_wide
            .checked_mul(tiles_high)
            .ok_or(Error::LimitExceeded("ARD MVS tile count"))?;
        let mut tile_index = 0_usize;
        let mut first_color = MvsPixel::Rgba([255, 255, 255, 255]);
        let mut second_color = MvsPixel::Rgba([254, 213, 181, 255]);
        let mut solid_color = MvsPixel::Rgba([0, 0, 0, 255]);
        // ExpandBlockRice's previous-block pointer is initialized to null for
        // each partial update and advances only within this rectangle.
        let mut previous_coefficients: Option<(Arc<[[i16; 64]; 3]>, u8)> = None;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let luminance_quantization = self.luminance_quantization;
        let chrominance_quantization = self.chrominance_quantization;
        let mut updates = Vec::with_capacity(total_tiles);

        while tile_index < total_tiles {
            let update_type = primary.read(3)?;
            let repeat = usize::try_from(primary.repeat_count()?)
                .map_err(|_| Error::LimitExceeded("ARD MVS repeat count"))?;
            let run = repeat
                .checked_add(1)
                .ok_or(Error::LimitExceeded("ARD MVS repeat count"))?;
            let run_end = tile_index
                .checked_add(run)
                .ok_or(Error::LimitExceeded("ARD MVS tile count"))?;
            if run_end > total_tiles {
                return Err(Error::Invalid("ARD MVS repeat exceeds rectangle"));
            }

            for _ in 0..run {
                let tile_x = (tile_index % tiles_wide) * 8;
                let tile_y = (tile_index / tiles_wide) * 8;
                let tile_width = (usize::from(rect.width) - tile_x).min(8);
                let tile_height = (usize::from(rect.height) - tile_y).min(8);
                let global_tile_x = (usize::from(rect.x) + tile_x) / 8;
                let global_tile_y = (usize::from(rect.y) + tile_y) / 8;
                let global_tile = global_tile_y
                    .checked_mul(self.tiles_wide)
                    .and_then(|row| row.checked_add(global_tile_x))
                    .ok_or(Error::LimitExceeded("ARD MVS framebuffer tile index"))?;
                if global_tile >= self.tiles.len() {
                    return Err(Error::Invalid("ARD MVS tile is outside framebuffer state"));
                }
                let mut decoded_dct = None;
                let mut copy_source = None;
                let mut emit_update = true;
                let gpu_tile = match update_type {
                    0 => MvsGpuTile::SolidRgba([255, 255, 255, 255]),
                    1 => {
                        // Apple indexes copy sources against the complete
                        // framebuffer, not against this rectangle. A
                        // rectangle can therefore begin at the second tile
                        // of a row and copy from the tile immediately to its
                        // left.
                        copy_source = global_tile.checked_sub(1);
                        match copy_source.and_then(|source| self.tiles.get(source)) {
                            Some(source_state) => source_state.gpu_tile.clone(),
                            None => {
                                // The native decoder records an invalid
                                // source and leaves the destination as-is.
                                // It does not reject the whole framebuffer
                                // update.
                                emit_update = false;
                                self.tiles[global_tile].gpu_tile.clone()
                            }
                        }
                    }
                    2 => {
                        // As with left-copy, this is a global framebuffer
                        // lookup. The first local row may copy from the
                        // preceding global row.
                        copy_source = global_tile.checked_sub(self.tiles_wide);
                        match copy_source.and_then(|source| self.tiles.get(source)) {
                            Some(source_state) => source_state.gpu_tile.clone(),
                            None => {
                                emit_update = false;
                                self.tiles[global_tile].gpu_tile.clone()
                            }
                        }
                    }
                    3 => decode_bilevel_gpu_tile(
                        &mut secondary,
                        MvsPixel::Rgba([255, 255, 255, 255]),
                        MvsPixel::Rgba([0, 0, 0, 255]),
                        MvsPixel::Rgba([255, 255, 255, 255]),
                    )?,
                    4 => {
                        if secondary.read(1)? != 0 {
                            if secondary.read(1)? == 0 {
                                first_color = read_ycbcr20(&mut secondary)?;
                                second_color = read_ycbcr20(&mut secondary)?;
                            }
                            decode_bilevel_gpu_tile(
                                &mut secondary,
                                first_color,
                                second_color,
                                first_color,
                            )?
                        } else {
                            if secondary.read(1)? == 0 {
                                solid_color = read_ycbcr20(&mut secondary)?;
                            }
                            gpu_solid(solid_color)
                        }
                    }
                    5 => {
                        let (coefficients, luma_count) = decode_rice_dct_coefficients(
                            &mut secondary,
                            &mut previous_coefficients,
                            payload[1],
                            payload[2],
                        )?;
                        decoded_dct = Some((Arc::clone(&coefficients), luma_count));
                        MvsGpuTile::RiceDct(coefficients)
                    }
                    6 | 7 => {
                        let cache_index = if update_type == 6 {
                            let high = secondary.read(8)? as u16;
                            let low = secondary.read(8)? as u16;
                            (high << 8) | low
                        } else {
                            next_cache_index(self.last_cache_reference)
                        };
                        match self.cached_tile(cache_index) {
                            Some(tile) => MvsGpuTile::Dct(Arc::clone(&tile.coefficients)),
                            None => {
                                // Cache_UpdateTile only logs an unavailable
                                // index and returns on the native decoder.
                                // Keep the tile metadata update, but do not
                                // emit a replacement for the previous tile.
                                emit_update = false;
                                self.tiles[global_tile].gpu_tile.clone()
                            }
                        }
                    }
                    _ => unreachable!("three-bit MVS update type"),
                };
                let state = &mut self.tiles[global_tile];
                state.generation = generation;
                state.copy_source = copy_source;
                state.gpu_tile = gpu_tile.clone();
                if let Some((coefficients, luma_count)) = decoded_dct {
                    state.dct = DctTile { coefficients };
                    state.luma_count = luma_count;
                }
                if emit_update {
                    updates.push(MvsGpuTileUpdate {
                        x: rect.x + tile_x as u16,
                        y: rect.y + tile_y as u16,
                        width: tile_width as u8,
                        height: tile_height as u8,
                        tile: gpu_tile,
                    });
                }
                tile_index += 1;
            }
        }

        if primary.read(8)? != 0x6d || secondary.read(8)? != 0x6d {
            return Err(Error::Invalid("invalid ARD MVS partial-update marker"));
        }
        if render_cpu {
            for update in &updates {
                render_gpu_tile_to_framebuffer(
                    update,
                    framebuffer,
                    &luminance_quantization,
                    &chrominance_quantization,
                );
            }
        }
        Ok(updates)
    }

    pub(crate) fn decode_full_update(
        &mut self,
        rect: Rectangle,
        payload: &[u8],
        framebuffer: &mut Framebuffer,
        max_output_bytes: usize,
        render_cpu: bool,
    ) -> Result<Vec<MvsGpuTileUpdate>> {
        self.ensure_framebuffer_state(framebuffer)?;
        if payload.len() < 3 {
            return Err(Error::NeedMore {
                needed: 3,
                available: payload.len(),
            });
        }
        if payload[0] != 1 {
            return Err(Error::Invalid("not an ARD MVS full update"));
        }
        let pixel_count = usize::from(rect.width)
            .checked_mul(usize::from(rect.height))
            .ok_or(Error::LimitExceeded("ARD MVS rectangle"))?;
        if pixel_count
            .checked_mul(4)
            .ok_or(Error::LimitExceeded("ARD MVS rectangle"))?
            > max_output_bytes
        {
            return Err(Error::LimitExceeded("ARD MVS rectangle"));
        }

        let tiles_wide = usize::from(rect.width).div_ceil(8);
        let tiles_high = usize::from(rect.height).div_ceil(8);
        let tiles = tiles_wide
            .checked_mul(tiles_high)
            .ok_or(Error::LimitExceeded("ARD MVS tile count"))?;
        let mut bits = BitReader::new(&payload[3..]);
        let luminance_limit = payload[1].min(64);
        let chrominance_limit = payload[2].min(64);
        let luminance_quantization = self.luminance_quantization;
        let chrominance_quantization = self.chrominance_quantization;
        let mut updates = Vec::with_capacity(tiles);
        for tile_index in 0..tiles {
            let local_tile_x = (tile_index % tiles_wide) * 8;
            let local_tile_y = (tile_index / tiles_wide) * 8;
            let tile_width = (usize::from(rect.width) - local_tile_x).min(8);
            let tile_height = (usize::from(rect.height) - local_tile_y).min(8);
            let x = usize::from(rect.x) + local_tile_x;
            let y = usize::from(rect.y) + local_tile_y;
            let global_tile = (y / 8)
                .checked_mul(self.tiles_wide)
                .and_then(|row| row.checked_add(x / 8))
                .ok_or(Error::LimitExceeded("ARD MVS framebuffer tile index"))?;
            if global_tile >= self.tiles.len() {
                return Err(Error::Invalid("ARD MVS tile is outside framebuffer state"));
            }

            match bits.read(2)? {
                0 => {}
                1 => {
                    let baseline = &self.tiles[global_tile];
                    let (coefficients, _luma_count) = decode_full_differential_tile(
                        &mut bits,
                        baseline,
                        luminance_limit,
                        chrominance_limit,
                    )?;
                    if render_cpu {
                        blit_dct_to_framebuffer(
                            &coefficients,
                            framebuffer,
                            x,
                            y,
                            tile_width,
                            tile_height,
                            &luminance_quantization,
                            &chrominance_quantization,
                        );
                    }
                    let coefficients = Arc::new(coefficients);
                    self.insert_cache_tile(DctTile {
                        coefficients: Arc::clone(&coefficients),
                    });
                    let state = &mut self.tiles[global_tile];
                    // A full update is a refinement of the most recent
                    // partial/Rice-DCT baseline. Screen Sharing caches the
                    // refined result, but deliberately leaves that baseline
                    // untouched for later full updates.
                    state.gpu_tile = MvsGpuTile::Dct(coefficients);
                    updates.push(MvsGpuTileUpdate {
                        x: x as u16,
                        y: y as u16,
                        width: tile_width as u8,
                        height: tile_height as u8,
                        tile: state.gpu_tile.clone(),
                    });
                }
                2 => {
                    // DecodeMVSUpdate treats an absent or stale copy source
                    // as a no-op. This is common when an automatic update
                    // contains a full selector for a tile whose partial
                    // copy metadata was not established in the current
                    // MVS generation; it is not a malformed wire packet.
                    let Some(source) = self.tiles[global_tile].copy_source else {
                        continue;
                    };
                    let Some(source_state) = self.tiles.get(source) else {
                        continue;
                    };
                    if source_state.generation != self.tiles[global_tile].generation {
                        continue;
                    }
                    let source_x = (source % self.tiles_wide) * 8;
                    let source_y = (source / self.tiles_wide) * 8;
                    if render_cpu {
                        framebuffer.copy_rect(
                            &Rectangle {
                                x: u16::try_from(x)
                                    .map_err(|_| Error::LimitExceeded("ARD MVS tile x"))?,
                                y: u16::try_from(y)
                                    .map_err(|_| Error::LimitExceeded("ARD MVS tile y"))?,
                                width: tile_width as u16,
                                height: tile_height as u16,
                                encoding: rect.encoding,
                            },
                            u16::try_from(source_x)
                                .map_err(|_| Error::LimitExceeded("ARD MVS source x"))?,
                            u16::try_from(source_y)
                                .map_err(|_| Error::LimitExceeded("ARD MVS source y"))?,
                        )?;
                    }
                    let gpu_tile = source_state.gpu_tile.clone();
                    self.tiles[global_tile].gpu_tile = gpu_tile.clone();
                    updates.push(MvsGpuTileUpdate {
                        x: x as u16,
                        y: y as u16,
                        width: tile_width as u8,
                        height: tile_height as u8,
                        tile: gpu_tile,
                    });
                }
                3 => {
                    let cache_index = if bits.read(1)? != 0 {
                        next_cache_index(self.last_cache_reference)
                    } else {
                        let high = bits.read(8)? as u16;
                        let low = bits.read(8)? as u16;
                        (high << 8) | low
                    };
                    let Some(tile) = self.cached_tile(cache_index) else {
                        // Native _Cache_UpdateTile logs and returns when the
                        // cache entry is not available. The rest of the MVS
                        // record remains valid and must still be consumed.
                        continue;
                    };
                    if render_cpu {
                        blit_dct_to_framebuffer(
                            &tile.coefficients,
                            framebuffer,
                            x,
                            y,
                            tile_width,
                            tile_height,
                            &luminance_quantization,
                            &chrominance_quantization,
                        );
                    }
                    let gpu_tile = MvsGpuTile::Dct(Arc::clone(&tile.coefficients));
                    self.tiles[global_tile].gpu_tile = gpu_tile.clone();
                    updates.push(MvsGpuTileUpdate {
                        x: x as u16,
                        y: y as u16,
                        width: tile_width as u8,
                        height: tile_height as u8,
                        tile: gpu_tile,
                    });
                }
                _ => unreachable!("two-bit MVS selector"),
            }
        }
        if bits.read(8)? != 0x6d || bits.read(8)? != 0x76 || bits.read(8)? != 0x73 {
            return Err(Error::Invalid("invalid ARD MVS full-update marker"));
        }
        Ok(updates)
    }
}

fn next_cache_index(index: u16) -> u16 {
    if index == DCT_CACHE_LAST_INDEX {
        1
    } else {
        index + 1
    }
}

fn decode_full_differential_tile(
    bits: &mut BitReader<'_>,
    baseline: &TileState,
    luminance_limit: u8,
    chrominance_limit: u8,
) -> Result<([[i16; 64]; 3], u8)> {
    let old_count = usize::from(baseline.luma_count);
    if old_count > 64 {
        return Err(Error::Invalid("invalid ARD MVS luma baseline length"));
    }
    let new_count = usize::try_from(bits.read(6)? + 1)
        .map_err(|_| Error::LimitExceeded("ARD MVS luma coefficient count"))?;
    let mut scan_values = [0_i8; 64];
    let mut old_scan_values = [0_i8; 64];
    for scan in 0..64 {
        old_scan_values[scan] = baseline.dct.coefficients[0][ZIGZAG[scan]] as i8;
    }
    scan_values[0] = old_scan_values[0];

    let shared_end = old_count.min(new_count);
    if old_count < 15 {
        for scan in 1..shared_end {
            let old = old_scan_values[scan];
            scan_values[scan] = if old == 0 {
                decode_dc_rice(bits)? as i8
            } else {
                signed_delta(old, bits.read(3)? as u8)
            };
        }
        // Scan 0 is the luma DC value copied above. The native decoder's
        // coefficient expansion starts its differential AC loop at scan 1,
        // even when the previous luma block has zero active coefficients.
        for scan in old_count.max(1)..new_count {
            let old = old_scan_values[scan.max(1)];
            scan_values[scan] = if old == 0 {
                decode_dc_rice(bits)? as i8
            } else {
                signed_delta(old, bits.read(4)? as u8)
            };
        }
    } else {
        for scan in 1..shared_end {
            let old = old_scan_values[scan];
            scan_values[scan] = if old == 0 {
                if bits.read(1)? == 0 {
                    0
                } else if bits.read(1)? == 0 {
                    1
                } else {
                    -1
                }
            } else {
                signed_delta(old, bits.read(1)? as u8)
            };
        }
        for scan in old_count..new_count {
            let old = old_scan_values[scan];
            scan_values[scan] = if old == 0 {
                decode_dc_rice(bits)? as i8
            } else {
                signed_delta(old, bits.read(3)? as u8)
            };
        }
    }

    let mut coefficients = [[0_i16; 64]; 3];
    for scan in 0..new_count {
        coefficients[0][ZIGZAG[scan]] = i16::from(scan_values[scan]);
    }
    // Screen Sharing stores the full-update chroma records in Cr, Cb order,
    // even though ExpandBlockRice's predictor arrays are Cb, Cr.
    coefficients[2][0] = i16::from(decode_one_step_dc(
        bits,
        baseline.dct.coefficients[2][0] as i8,
    )?);
    decode_luminance_huffman_ac(bits, &mut coefficients[2], luminance_limit)?;
    coefficients[1][0] = i16::from(decode_one_step_dc(
        bits,
        baseline.dct.coefficients[1][0] as i8,
    )?);
    decode_luminance_huffman_ac(bits, &mut coefficients[1], chrominance_limit)?;
    Ok((coefficients, new_count as u8))
}

fn signed_delta(old: i8, magnitude: u8) -> i8 {
    if old.is_negative() {
        old.wrapping_sub(magnitude as i8)
    } else {
        old.wrapping_add(magnitude as i8)
    }
}

fn decode_one_step_dc(bits: &mut BitReader<'_>, old: i8) -> Result<i8> {
    let changes = bits.read(1)? != 0;
    if old == 0 {
        if !changes {
            Ok(0)
        } else if bits.read(1)? == 0 {
            Ok(1)
        } else {
            Ok(-1)
        }
    } else if !changes {
        Ok(old)
    } else if old.is_negative() {
        Ok(old.wrapping_sub(1))
    } else {
        Ok(old.wrapping_add(1))
    }
}

fn decode_luminance_huffman_ac(
    bits: &mut BitReader<'_>,
    coefficients: &mut [i16; 64],
    limit: u8,
) -> Result<()> {
    let mut scan = 1_usize;
    let limit = usize::from(limit);
    // The native loop's counter starts at zero (before the first AC
    // coefficient) and continues while that counter is below `limit`. Rust
    // stores the natural coefficient position in `scan`, which starts at
    // one, so the equivalent condition is inclusive here.
    while scan <= limit {
        let symbol = decode_luminance_ac_symbol(bits)?;
        let run = usize::from(symbol >> 4);
        let size = symbol & 0x0f;
        if size == 0 {
            if run != 15 {
                break;
            }
            scan = scan
                .checked_add(16)
                .ok_or(Error::LimitExceeded("ARD MVS Huffman run"))?;
            continue;
        }
        scan = scan
            .checked_add(run)
            .ok_or(Error::LimitExceeded("ARD MVS Huffman run"))?;
        if scan >= 80 {
            return Err(Error::Invalid("ARD MVS Huffman run exceeds block"));
        }
        let encoded = bits.read(size)?;
        let threshold = 1_u32 << (size - 1);
        let value = if encoded < threshold {
            i32::try_from(encoded)
                .map_err(|_| Error::LimitExceeded("ARD MVS Huffman coefficient"))?
                + 1
                - (1_i32 << size)
        } else {
            i32::try_from(encoded)
                .map_err(|_| Error::LimitExceeded("ARD MVS Huffman coefficient"))?
        };
        let natural_index = if scan < 64 { ZIGZAG[scan] } else { 63 };
        coefficients[natural_index] = i16::try_from(value)
            .map_err(|_| Error::LimitExceeded("ARD MVS Huffman coefficient"))?;
        scan += 1;
    }
    Ok(())
}

fn decode_luminance_ac_symbol(bits: &mut BitReader<'_>) -> Result<u8> {
    let mut code = 0_u32;
    let mut first_code = 0_u32;
    let mut value_index = 0_usize;
    for length in 1..=16_u8 {
        code = (code << 1) | bits.read(1)?;
        let count = u32::from(LUMINANCE_AC_BITS[usize::from(length)]);
        if code >= first_code && code < first_code + count {
            let offset = usize::try_from(code - first_code)
                .map_err(|_| Error::LimitExceeded("ARD MVS Huffman table"))?;
            return LUMINANCE_AC_VALUES
                .get(value_index + offset)
                .copied()
                .ok_or(Error::Invalid("invalid ARD MVS Huffman table index"));
        }
        value_index = value_index
            .checked_add(
                usize::try_from(count)
                    .map_err(|_| Error::LimitExceeded("ARD MVS Huffman table"))?,
            )
            .ok_or(Error::LimitExceeded("ARD MVS Huffman table"))?;
        first_code = (first_code + count) << 1;
    }
    Err(Error::Invalid("invalid ARD MVS Huffman code"))
}

#[allow(clippy::too_many_arguments)]
fn blit_dct_to_framebuffer(
    coefficients: &[[i16; 64]; 3],
    framebuffer: &mut Framebuffer,
    tile_x: usize,
    tile_y: usize,
    width: usize,
    height: usize,
    luminance_quantization: &[u16; 64],
    chrominance_quantization: &[u16; 64],
) {
    let luminance = inverse_dct(&coefficients[0], luminance_quantization);
    let cb = inverse_dct(&coefficients[1], chrominance_quantization);
    let cr = inverse_dct(&coefficients[2], chrominance_quantization);
    for y in 0..height {
        for x in 0..width {
            let index = y * 8 + x;
            framebuffer.set_ycbcr(
                (tile_x + x) as u16,
                (tile_y + y) as u16,
                [luminance[index] as u8, cb[index] as u8, cr[index] as u8],
            );
        }
    }
}

fn decode_bilevel_gpu_tile(
    bits: &mut BitReader<'_>,
    uniform_color: MvsPixel,
    zero_color: MvsPixel,
    one_color: MvsPixel,
) -> Result<MvsGpuTile> {
    let mut pixels = [uniform_color; 64];
    let uniform_rows = bits.read(8)? as u8;
    for y in 0..8 {
        let row_is_uniform = uniform_rows & (0x80 >> y) != 0;
        let row_bits = if row_is_uniform {
            0
        } else {
            bits.read(8)? as u8
        };
        for x in 0..8 {
            let color = if row_is_uniform {
                uniform_color
            } else if row_bits & (0x80 >> x) == 0 {
                zero_color
            } else {
                one_color
            };
            pixels[y * 8 + x] = color;
        }
    }
    Ok(gpu_pixels(pixels))
}

fn read_ycbcr20(bits: &mut BitReader<'_>) -> Result<MvsPixel> {
    let y = bits.read(8)? as u8;
    let cb = (bits.read(6)? * 4) as u8;
    let cr = (bits.read(6)? * 4) as u8;
    Ok(MvsPixel::Ycbcr([y, cb, cr]))
}

fn decode_rice_dct_coefficients(
    bits: &mut BitReader<'_>,
    previous_coefficients: &mut Option<(Arc<[[i16; 64]; 3]>, u8)>,
    luminance_limit: u8,
    chrominance_limit: u8,
) -> Result<(Arc<[[i16; 64]; 3]>, u8)> {
    let (coefficients, coefficient_limit) = if bits.read(1)? != 0 {
        let Some((coefficients, coefficient_limit)) = previous_coefficients.as_ref() else {
            return Err(Error::Invalid(
                "ARD MVS Rice/DCT reuse has no previous block",
            ));
        };
        (Arc::clone(coefficients), *coefficient_limit)
    } else {
        let mut current = [[0_i16; 64]; 3];
        let predictor_mode = bits.read(2)?;
        let (previous_luma_dc, previous_cb_dc, previous_cr_dc) = previous_coefficients
            .as_ref()
            .map_or((0, 0, 0), |(coefficients, _)| {
                (coefficients[0][0], coefficients[1][0], coefficients[2][0])
            });
        if predictor_mode & 2 != 0 {
            current[1][0] = previous_cb_dc;
            current[2][0] = previous_cr_dc;
        } else {
            for (slot, predictor) in current[1..=2]
                .iter_mut()
                .zip([previous_cb_dc, previous_cr_dc])
            {
                let delta = decode_dc_rice(bits)?;
                slot[0] = predict_chroma_dc(predictor, delta);
            }
        }
        current[0][0] = previous_luma_dc.wrapping_sub(decode_dc_rice(bits)?);
        let coefficient_limit = if predictor_mode & 1 != 0 {
            chrominance_limit
        } else {
            luminance_limit
        };
        decode_ac_rice(bits, &mut current[0], coefficient_limit)?;
        (Arc::new(current), coefficient_limit)
    };

    *previous_coefficients = Some((Arc::clone(&coefficients), coefficient_limit));
    Ok((coefficients, coefficient_limit))
}

fn gpu_solid(pixel: MvsPixel) -> MvsGpuTile {
    match pixel {
        MvsPixel::Rgba(rgba) => MvsGpuTile::SolidRgba(rgba),
        MvsPixel::Ycbcr(sample) => MvsGpuTile::SolidYcbcr(sample),
    }
}

fn gpu_pixels(pixels: [MvsPixel; 64]) -> MvsGpuTile {
    if pixels
        .iter()
        .all(|pixel| matches!(pixel, MvsPixel::Ycbcr(_)))
    {
        MvsGpuTile::PixelsYcbcr(Box::new(pixels.map(|pixel| match pixel {
            MvsPixel::Ycbcr(sample) => sample,
            MvsPixel::Rgba(_) => unreachable!("pixel kind checked"),
        })))
    } else {
        MvsGpuTile::PixelsRgba(Box::new(pixels.map(mvs_pixel_to_rgba)))
    }
}

fn mvs_pixel_to_rgba(pixel: MvsPixel) -> [u8; 4] {
    match pixel {
        MvsPixel::Rgba(rgba) => rgba,
        MvsPixel::Ycbcr([y, cb, cr]) => {
            let y = i32::from(y);
            let cb = i32::from(cb) - 128;
            let cr = i32::from(cr) - 128;
            let red = y + ((91_881 * cr + 32_768) >> 16);
            let green = y + ((32_768 - 22_554 * cb - 46_802 * cr) >> 16);
            let blue = y + ((116_130 * cb + 32_768) >> 16);
            [
                red.clamp(0, 255) as u8,
                green.clamp(0, 255) as u8,
                blue.clamp(0, 255) as u8,
                255,
            ]
        }
    }
}

fn render_gpu_tile_to_framebuffer(
    update: &MvsGpuTileUpdate,
    framebuffer: &mut Framebuffer,
    luminance_quantization: &[u16; 64],
    chrominance_quantization: &[u16; 64],
) {
    match &update.tile {
        MvsGpuTile::SolidYcbcr(sample) => {
            for y in 0..u16::from(update.height) {
                for x in 0..u16::from(update.width) {
                    framebuffer.set_ycbcr(update.x + x, update.y + y, *sample);
                }
            }
        }
        MvsGpuTile::SolidRgba(rgba) => {
            for y in 0..u16::from(update.height) {
                for x in 0..u16::from(update.width) {
                    framebuffer.set_pixel(update.x + x, update.y + y, *rgba);
                }
            }
        }
        MvsGpuTile::PixelsYcbcr(pixels) => {
            for y in 0..usize::from(update.height) {
                for x in 0..usize::from(update.width) {
                    framebuffer.set_ycbcr(
                        update.x + x as u16,
                        update.y + y as u16,
                        pixels[y * 8 + x],
                    );
                }
            }
        }
        MvsGpuTile::PixelsRgba(pixels) => {
            for y in 0..usize::from(update.height) {
                for x in 0..usize::from(update.width) {
                    framebuffer.set_pixel(
                        update.x + x as u16,
                        update.y + y as u16,
                        pixels[y * 8 + x],
                    );
                }
            }
        }
        MvsGpuTile::RiceDct(coefficients) => {
            let luminance = inverse_dct(&coefficients[0], luminance_quantization);
            let cb = dc_sample_value(coefficients[1][0], chrominance_quantization[0]);
            let cr = dc_sample_value(coefficients[2][0], chrominance_quantization[0]);
            for y in 0..usize::from(update.height) {
                for x in 0..usize::from(update.width) {
                    framebuffer.set_ycbcr(
                        update.x + x as u16,
                        update.y + y as u16,
                        [luminance[y * 8 + x] as u8, cb as u8, cr as u8],
                    );
                }
            }
        }
        MvsGpuTile::Dct(coefficients) => blit_dct_to_framebuffer(
            coefficients,
            framebuffer,
            usize::from(update.x),
            usize::from(update.y),
            usize::from(update.width),
            usize::from(update.height),
            luminance_quantization,
            chrominance_quantization,
        ),
    }
}

fn predict_chroma_dc(predictor: i16, delta: i16) -> i16 {
    // ExpandBlockRice halves the signed predictor with truncation toward zero,
    // applies the Rice delta, then doubles it again. Rust's signed division
    // already has the required rounding. Applying an additional sign
    // correction makes every negative even predictor drift by +2 on each new
    // block, which accumulates into visible magenta 8x8 tiles.
    (predictor / 2).wrapping_sub(delta).wrapping_mul(2)
}

fn decode_ac_rice(bits: &mut BitReader<'_>, coefficients: &mut [i16; 64], limit: u8) -> Result<()> {
    let mut scan = 1_usize;
    while scan < 64 {
        if bits.read(1)? != 0 {
            let compact_phase = scan < 6;
            let maximum_prefix = if compact_phase { 33 } else { 19 };
            let mut prefix = 0_u32;
            while bits.read(1)? != 0 {
                prefix += 1;
                if prefix > maximum_prefix {
                    return Err(Error::Invalid("ARD MVS AC Rice prefix is too long"));
                }
            }

            let (magnitude, sign) = if compact_phase {
                if prefix < 4 {
                    let suffix = bits.read(3)?;
                    (prefix * 4 + 2 + (suffix >> 1), suffix & 1)
                } else {
                    let suffix = bits.read(4)?;
                    (prefix * 8 - 14 + (suffix >> 1), suffix & 1)
                }
            } else {
                match prefix {
                    0 => {
                        let suffix = bits.read(2)?;
                        (2 + (suffix >> 1), suffix & 1)
                    }
                    1 => {
                        let suffix = bits.read(3)?;
                        (4 + (suffix >> 1), suffix & 1)
                    }
                    _ => {
                        let suffix = bits.read(4)?;
                        (prefix * 8 - 8 + (suffix >> 1), suffix & 1)
                    }
                }
            };
            let magnitude = magnitude
                .checked_shl(coefficient_shift(scan, limit))
                .ok_or(Error::LimitExceeded("ARD MVS AC Rice coefficient"))?;
            let magnitude = i16::try_from(magnitude)
                .map_err(|_| Error::LimitExceeded("ARD MVS AC Rice coefficient"))?;
            coefficients[ZIGZAG[scan]] = if sign == 0 { magnitude } else { -magnitude };
            scan += 1;
            continue;
        }

        let selector = bits.read(2)?;
        match selector {
            0 => scan += 1,
            2 | 3 => {
                let magnitude = 1_i16
                    .checked_shl(coefficient_shift(scan, limit))
                    .ok_or(Error::LimitExceeded("ARD MVS AC Rice coefficient"))?;
                coefficients[ZIGZAG[scan]] = if selector == 2 { magnitude } else { -magnitude };
                scan += 1;
            }
            1 => {
                if bits.read(1)? == 0 {
                    break;
                }
                let first = bits.read(2)?;
                let mut run = first + 3;
                if first == 3 {
                    loop {
                        let extension = bits.read(3)?;
                        run = run
                            .checked_add(extension)
                            .ok_or(Error::LimitExceeded("ARD MVS AC Rice run"))?;
                        if extension != 7 {
                            break;
                        }
                    }
                }
                scan = scan
                    .checked_add(
                        usize::try_from(run)
                            .map_err(|_| Error::LimitExceeded("ARD MVS AC Rice run"))?,
                    )
                    .ok_or(Error::LimitExceeded("ARD MVS AC Rice run"))?;
                if scan > 64 {
                    return Err(Error::Invalid("ARD MVS AC Rice run exceeds block"));
                }
            }
            _ => unreachable!("two-bit AC Rice selector"),
        }
    }
    Ok(())
}

fn coefficient_shift(scan: usize, limit: u8) -> u32 {
    if scan < usize::from(limit) {
        if limit >= 15 { 1 } else { 3 }
    } else {
        4
    }
}

fn inverse_dct(coefficients: &[i16; 64], quantization: &[u16; 64]) -> [i32; 64] {
    // The transform is separable. Keeping the intermediate values in the
    // same exact integer domain as the original two-dimensional sum reduces
    // the arithmetic from 64 products per output sample to 16, without
    // changing the final rounding or clamping.
    let mut horizontal = [0_i64; 64];
    for v in 0..8 {
        for x in 0..8 {
            let mut sum = 0_i64;
            for (u, basis_row) in IDCT_BASIS.iter().enumerate() {
                let index = v * 8 + u;
                sum += i64::from(coefficients[index])
                    * i64::from(quantization[index])
                    * i64::from(basis_row[x]);
            }
            horizontal[v * 8 + x] = sum;
        }
    }

    let mut output = [0_i32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0_i64;
            for v in 0..8 {
                sum += horizontal[v * 8 + x] * i64::from(IDCT_BASIS[v][y]);
            }
            output[y * 8 + x] = (((sum + (1_i64 << 29)) >> 30) as i32 + 128).clamp(0, 255);
        }
    }
    output
}

#[cfg(test)]
fn dc_sample(coefficient: i16, quantization: u16) -> i32 {
    dc_sample_value(coefficient, quantization)
}

fn dc_sample_value(coefficient: i16, quantization: u16) -> i32 {
    let dequantized = i32::from(coefficient) * i32::from(quantization);
    (((dequantized + 4) >> 3) + 128).clamp(0, 255)
}

fn decode_dc_rice(bits: &mut BitReader<'_>) -> Result<i16> {
    let mut prefix = 0_u32;
    while bits.read(1)? != 0 {
        prefix += 1;
        if prefix == 40 {
            return Err(Error::Invalid("ARD MVS DC Rice prefix exceeds 39 bits"));
        }
    }

    let (magnitude, sign) = match prefix {
        0 => {
            if bits.read(1)? == 0 {
                return Ok(0);
            }
            (1, bits.read(1)?)
        }
        1 => {
            let suffix = bits.read(2)?;
            (2 + (suffix >> 1), suffix & 1)
        }
        2 => {
            let suffix = bits.read(3)?;
            (4 + (suffix >> 1), suffix & 1)
        }
        _ => {
            let suffix = bits.read(4)?;
            (
                prefix
                    .checked_mul(8)
                    .and_then(|value| value.checked_sub(16))
                    .and_then(|value| value.checked_add(suffix >> 1))
                    .ok_or(Error::LimitExceeded("ARD MVS DC Rice value"))?,
                suffix & 1,
            )
        }
    };
    let magnitude =
        i16::try_from(magnitude).map_err(|_| Error::LimitExceeded("ARD MVS DC Rice value"))?;
    Ok(if sign == 0 { magnitude } else { -magnitude })
}

#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    bytes: &'a [u8],
    bit_position: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            bit_position: 0,
        }
    }

    #[inline]
    pub(crate) fn read(&mut self, count: u8) -> Result<u32> {
        if count > 32 {
            return Err(Error::Invalid("ARD MVS bit read exceeds 32 bits"));
        }
        let count = usize::from(count);
        let end = self
            .bit_position
            .checked_add(count)
            .ok_or(Error::LimitExceeded("ARD MVS bit position"))?;
        let available_bits = self
            .bytes
            .len()
            .checked_mul(8)
            .ok_or(Error::LimitExceeded("ARD MVS bitstream"))?;
        if end > available_bits {
            return Err(Error::NeedMore {
                needed: end.div_ceil(8),
                available: self.bytes.len(),
            });
        }

        if count == 0 {
            return Ok(0);
        }

        let byte_index = self.bit_position / 8;
        let bit_offset = self.bit_position % 8;
        if bit_offset + count <= 8 {
            let shift = 8 - bit_offset - count;
            let mask = (1_u16 << count) - 1;
            let value = (u16::from(self.bytes[byte_index]) >> shift) & mask;
            self.bit_position = end;
            return Ok(u32::from(value));
        }

        let mut value = 0_u32;
        let mut remaining = count;
        let mut index = byte_index;
        let mut offset = bit_offset;
        while remaining != 0 {
            let take = (8 - offset).min(remaining);
            let shift = 8 - offset - take;
            let mask = (1_u16 << take) - 1;
            let chunk = (u16::from(self.bytes[index]) >> shift) & mask;
            value = (value << take) | u32::from(chunk);
            remaining -= take;
            index += 1;
            offset = 0;
        }
        self.bit_position = end;
        Ok(value)
    }

    /// Mirrors Screen Sharing's `GetRepeatCount`.
    ///
    /// A zero leading bit means no repetition. A set bit is followed by a
    /// four-bit value for counts 1 through 15. Value 15 switches to up to
    /// three little-endian 7-bit groups with the high bit as continuation.
    pub(crate) fn repeat_count(&mut self) -> Result<u32> {
        if self.read(1)? == 0 {
            return Ok(0);
        }
        let short = self.read(4)?;
        if short != 15 {
            return Ok(short + 1);
        }

        let mut value = 0_u32;
        for shift in [0, 7, 14] {
            let group = self.read(8)?;
            value |= (group & 0x7f) << shift;
            if group & 0x80 == 0 {
                return value
                    .checked_add(16)
                    .ok_or(Error::LimitExceeded("ARD MVS repeat count"));
            }
        }
        value
            .checked_add(16)
            .ok_or(Error::LimitExceeded("ARD MVS repeat count"))
    }
}

#[cfg(test)]
mod tests {
    use super::{BitReader, IDCT_BASIS, MvsState, dc_sample, inverse_dct, predict_chroma_dc};
    use crate::Error;

    fn bits(bit_string: &str) -> Vec<u8> {
        let significant = bit_string.bytes().filter(u8::is_ascii_digit).count();
        let mut output = Vec::with_capacity(significant.div_ceil(8));
        let mut byte = 0_u8;
        for (index, bit) in bit_string.bytes().filter(u8::is_ascii_digit).enumerate() {
            byte = (byte << 1) | u8::from(bit == b'1');
            if index % 8 == 7 {
                output.push(byte);
                byte = 0;
            }
        }
        let remainder = significant % 8;
        if remainder != 0 {
            output.push(byte << (8 - remainder));
        }
        output
    }

    #[test]
    fn bit_reader_is_msb_first_across_byte_boundaries() {
        let bytes = [0b1011_0010, 0b0110_0000];
        let mut reader = BitReader::new(&bytes);
        assert_eq!(reader.read(3).unwrap(), 0b101);
        assert_eq!(reader.read(7).unwrap(), 0b1001001);
        assert_eq!(reader.bit_position, 10);
    }

    #[test]
    fn repeat_count_decodes_all_screen_sharing_forms() {
        let short_bits = bits("0 1 0000 1 1110");
        let mut short = BitReader::new(&short_bits);
        assert_eq!(short.repeat_count().unwrap(), 0);
        assert_eq!(short.repeat_count().unwrap(), 1);
        assert_eq!(short.repeat_count().unwrap(), 15);

        let extended_bits = bits("1 1111 00000101 1 1111 10000001 00000010");
        let mut extended = BitReader::new(&extended_bits);
        assert_eq!(extended.repeat_count().unwrap(), 21);
        assert_eq!(extended.repeat_count().unwrap(), 273);
    }

    #[test]
    fn bit_reader_rejects_truncation() {
        let bytes = [0xff];
        let mut reader = BitReader::new(&bytes);
        assert_eq!(reader.read(8).unwrap(), 255);
        assert_eq!(
            reader.read(1).unwrap_err(),
            Error::NeedMore {
                needed: 2,
                available: 1
            }
        );
    }

    #[test]
    fn default_quantization_matches_screen_sharing() {
        let state = MvsState::default();
        let (luminance, chrominance) = state.quantization_tables();
        assert_eq!(&luminance[..8], &[16, 11, 11, 14, 24, 22, 24, 33]);
        assert_eq!(&luminance[56..], &[35, 30, 33, 38, 45, 55, 65, 75]);
        assert_eq!(&chrominance[..8], &[19, 19, 24, 47, 76, 99, 99, 99]);
        assert_eq!(chrominance[32], 76);
        assert!(chrominance[33..].iter().all(|&value| value == 99));
    }

    #[test]
    fn inverse_dct_clamps_component_samples_before_color_conversion() {
        let quantization = [16; 64];
        let mut coefficients = [0; 64];
        coefficients[0] = 100;
        assert!(
            inverse_dct(&coefficients, &quantization)
                .into_iter()
                .all(|sample| sample == 255)
        );

        coefficients[0] = -100;
        assert!(
            inverse_dct(&coefficients, &quantization)
                .into_iter()
                .all(|sample| sample == 0)
        );

        assert_eq!(dc_sample(100, 16), 255);
        assert_eq!(dc_sample(-100, 16), 0);
    }

    #[test]
    fn separable_inverse_dct_matches_direct_integer_reference() {
        let coefficients = core::array::from_fn(|index| ((index * 37 % 65) as i16) - 32);
        let quantization = core::array::from_fn(|index| (index * 19 % 99 + 1) as u16);
        let mut expected = [0_i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut sum = 0_i64;
                for (v, vertical_basis) in IDCT_BASIS.iter().enumerate() {
                    for (u, horizontal_basis) in IDCT_BASIS.iter().enumerate() {
                        let index = v * 8 + u;
                        sum += i64::from(coefficients[index])
                            * i64::from(quantization[index])
                            * i64::from(horizontal_basis[x])
                            * i64::from(vertical_basis[y]);
                    }
                }
                expected[y * 8 + x] = (((sum + (1_i64 << 29)) >> 30) as i32 + 128).clamp(0, 255);
            }
        }
        assert_eq!(inverse_dct(&coefficients, &quantization), expected);
    }

    #[test]
    fn chroma_dc_prediction_matches_expand_block_rice_signed_rounding() {
        assert_eq!(predict_chroma_dc(4, 0), 4);
        assert_eq!(predict_chroma_dc(3, 0), 2);
        assert_eq!(predict_chroma_dc(-4, 0), -4);
        assert_eq!(predict_chroma_dc(-3, 0), -2);
        assert_eq!(predict_chroma_dc(-4, 1), -6);
        assert_eq!(predict_chroma_dc(-4, -1), -2);
    }
}
