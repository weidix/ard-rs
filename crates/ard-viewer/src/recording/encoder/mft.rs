//! Media Foundation H.264 encoder for session recording.
//!
//! Windows exposes H.264 encoding as a Media Foundation Transform: the recorder
//! hands it one input sample per presented frame and pulls one compressed sample
//! back. The transform is synchronous, so unlike the VideoToolbox backend this
//! one never needs a callback queue.
//!
//! The transform is asked for a hardware encoder first and falls back to the
//! inbox `CLSID_MSH264EncoderMFT`. Input is ARGB when the transform accepts it —
//! which keeps an RGB recording free of any colour conversion — and NV12
//! otherwise; an NV12 source is always passed through untouched. Output samples
//! are an Annex-B byte stream with the parameter sets published separately as
//! `MF_MT_MPEG_SEQUENCE_HEADER`, so they are converted to the length-prefixed
//! form an MP4 track stores.
//!
//! This backend compiles for `x86_64-pc-windows-gnu` in this repository but has
//! not been run against a live Windows session yet; the VideoToolbox backend is
//! the one covered by the recording verification.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};

use super::{EncodedSample, EncoderSettings, FrameTimeline, ParameterSets, SourceFrame};
use crate::media::{YuvMatrix, YuvRange};
use crate::recording::nal;
use crate::recording::{TIMESCALE, timeline_position};

/// Media Foundation expresses sample times in 100-nanosecond units.
const HNS_PER_SECOND: i64 = 10_000_000;

/// Owns the thread's COM and Media Foundation initialization.
struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    fn new() -> Result<Self, String> {
        // SAFETY: this object is constructed and dropped on the recording
        // thread, and no COM interface escapes it.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|error| format!("COM 初始化失败：{error}"))?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(format!("Media Foundation 初始化失败：{error}"));
            }
        }
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        // SAFETY: paired with a successful initialization, on the same thread,
        // after every transform interface has been released.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

/// How one frame's bytes are laid out for the transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputLayout {
    /// Packed 32-bit BGRA: `stride` bytes per row, RGBA order in memory is what
    /// the capture readback produced and what MFVideoFormat_ARGB32 expects.
    Bgra { stride: usize },
    /// Contiguous NV12: luma plane then interleaved chroma, both `stride` wide.
    Nv12 { stride: usize },
}

pub(crate) struct MediaFoundationEncoder {
    transform: IMFTransform,
    settings: EncoderSettings,
    layout: InputLayout,
    /// Colour description of the frames being recorded, applied to the input
    /// type when the transform takes NV12 directly.
    source_color: Option<(YuvRange, YuvMatrix)>,
    timeline: FrameTimeline,
    held: Option<(IMFSample, Duration)>,
    samples: VecDeque<EncodedSample>,
    parameter_sets: Option<ParameterSets>,
    frames_submitted: u64,
    finished: bool,
    // Kept last so the transform is released before Media Foundation shuts down.
    _runtime: MediaFoundationRuntime,
}

impl MediaFoundationEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, String> {
        // The recorder passes NV12 planes through without a colour conversion,
        // so a stream from a P3-tagged display really is P3. Media Foundation's
        // H.264 output type has no `MF_MT_VIDEO_PRIMARIES` value for Display P3,
        // so this build leaves the recording untagged rather than mislabelling
        // it BT.709 the way the macOS path used to.
        let _ = settings.primaries;
        if settings.width == 0 || settings.height == 0 {
            return Err("录制尺寸无效".into());
        }
        let runtime = MediaFoundationRuntime::new()?;
        let transform = create_h264_transform()?;
        let mut encoder = Self {
            transform,
            settings,
            layout: InputLayout::Nv12 {
                stride: settings.width as usize,
            },
            source_color: None,
            timeline: FrameTimeline::default(),
            held: None,
            samples: VecDeque::new(),
            parameter_sets: None,
            frames_submitted: 0,
            finished: false,
            _runtime: runtime,
        };
        encoder.configure_output()?;
        encoder.configure_input()?;
        // SAFETY: the transform is fully configured by this point.
        unsafe {
            encoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|_| {
                    encoder
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                })
                .map_err(|error| format!("无法启动 H.264 编码 MFT：{error}"))?;
        }
        encoder.read_sequence_header();
        Ok(encoder)
    }

    fn output_type(&self) -> Result<IMFMediaType, String> {
        // SAFETY: media type construction only touches local interfaces.
        unsafe {
            let media_type =
                MFCreateMediaType().map_err(|error| format!("无法创建编码输出格式：{error}"))?;
            let frame_size =
                (u64::from(self.settings.width) << 32) | u64::from(self.settings.height);
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|_| media_type.SetUINT32(&MF_MT_AVG_BITRATE, self.settings.bitrate))
                .and_then(|_| media_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size))
                .and_then(|_| {
                    media_type.SetUINT64(
                        &MF_MT_FRAME_RATE,
                        (u64::from(self.settings.frame_rate_hint.max(1)) << 32) | 1,
                    )
                })
                .and_then(|_| {
                    media_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .and_then(|_| {
                    media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                })
                .map_err(|error| format!("无法配置编码输出格式：{error}"))?;
            Ok(media_type)
        }
    }

    fn configure_output(&mut self) -> Result<(), String> {
        // The inbox encoder accepts the requested type only after a matching
        // input type exists on some systems, so both orders are attempted.
        let media_type = self.output_type()?;
        // SAFETY: the transform is live and the media type is local.
        let first = unsafe { self.transform.SetOutputType(0, &media_type, 0) };
        if first.is_err() {
            self.configure_input()?;
            // SAFETY: as above.
            unsafe { self.transform.SetOutputType(0, &media_type, 0) }
                .map_err(|error| format!("系统 H.264 编码器拒绝输出格式：{error}"))?;
        }
        Ok(())
    }

    /// Negotiate the input format: RGB when the transform accepts it, NV12
    /// otherwise, since a colour conversion would subsample chroma.
    fn configure_input(&mut self) -> Result<(), String> {
        let stride = Some(self.settings.width as usize);
        let color = self.source_color;
        if let Ok(Some(layout)) =
            self.try_input(MFVideoFormat_ARGB32, stride.map(|stride| stride * 4), None)
        {
            self.layout = layout;
            return Ok(());
        }
        let layout = self
            .try_input(MFVideoFormat_NV12, stride, color)?
            .ok_or_else(|| "系统 H.264 编码器既不接受 ARGB32 也不接受 NV12 输入".to_owned())?;
        self.layout = layout;
        Ok(())
    }

    fn try_input(
        &self,
        subtype: windows::core::GUID,
        stride: Option<usize>,
        color: Option<(YuvRange, YuvMatrix)>,
    ) -> Result<Option<InputLayout>, String> {
        // SAFETY: the transform is live; the media type is local.
        unsafe {
            let media_type =
                MFCreateMediaType().map_err(|error| format!("无法创建编码输入格式：{error}"))?;
            let frame_size =
                (u64::from(self.settings.width) << 32) | u64::from(self.settings.height);
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| media_type.SetGUID(&MF_MT_SUBTYPE, &subtype))
                .and_then(|_| media_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size))
                .and_then(|_| {
                    media_type.SetUINT64(
                        &MF_MT_FRAME_RATE,
                        (u64::from(self.settings.frame_rate_hint.max(1)) << 32) | 1,
                    )
                })
                .and_then(|_| {
                    media_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .map_err(|error| format!("无法配置编码输入格式：{error}"))?;
            if let Some(stride) = stride {
                media_type
                    .SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32)
                    .map_err(|error| format!("无法配置编码输入行距：{error}"))?;
            }
            if let Some((range, matrix)) = color {
                // Tell the encoder how the decoded planes are encoded, so the
                // recorded stream carries the same colour description the
                // viewer converted with.
                media_type
                    .SetUINT32(
                        &MF_MT_YUV_MATRIX,
                        match matrix {
                            YuvMatrix::Bt601 => MFVideoTransferMatrix_BT601.0 as u32,
                            YuvMatrix::Bt709 => MFVideoTransferMatrix_BT709.0 as u32,
                            YuvMatrix::Bt2020 => MFVideoTransferMatrix_BT2020_10.0 as u32,
                        },
                    )
                    .and_then(|_| {
                        media_type.SetUINT32(
                            &MF_MT_VIDEO_NOMINAL_RANGE,
                            match range {
                                YuvRange::Video => MFNominalRange_16_235.0 as u32,
                                YuvRange::Full => MFNominalRange_0_255.0 as u32,
                            },
                        )
                    })
                    .map_err(|error| format!("无法配置编码输入色彩范围：{error}"))?;
            }
            if self.transform.SetInputType(0, &media_type, 0).is_err() {
                return Ok(None);
            }
            let layout = if subtype == MFVideoFormat_ARGB32 {
                InputLayout::Bgra {
                    stride: stride.unwrap_or(self.settings.width as usize * 4),
                }
            } else {
                InputLayout::Nv12 {
                    stride: stride.unwrap_or(self.settings.width as usize),
                }
            };
            Ok(Some(layout))
        }
    }

    /// Read the SPS/PPS the transform publishes for the current output type.
    fn read_sequence_header(&mut self) {
        // SAFETY: the output type is queried on a configured transform.
        let sequence = unsafe {
            let media_type = match self.transform.GetOutputCurrentType(0) {
                Ok(media_type) => media_type,
                Err(_) => return,
            };
            let Ok(size) = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return;
            };
            if size == 0 {
                return;
            }
            let mut blob = vec![0_u8; size as usize];
            match media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None) {
                Ok(()) => blob,
                Err(_) => return,
            }
        };
        let (sps, pps) = nal::parameter_sets(&sequence, false);
        if let (Some(sps), Some(pps)) = (sps, pps) {
            self.parameter_sets = Some(ParameterSets { sps, pps });
        }
    }

    pub fn push(&mut self, frame: SourceFrame<'_>, pts: Duration) -> Result<(), String> {
        if self.finished {
            return Err("编码器已经结束".into());
        }
        if self.source_color.is_none()
            && let SourceFrame::Nv12 { range, matrix, .. } = frame
        {
            self.source_color = Some((range, matrix));
        }
        let sample = self.create_sample(&frame)?;
        let pts_hns = position_hns(pts);
        // SAFETY: the sample was created above and the counter is local.
        unsafe {
            sample
                .SetSampleTime(pts_hns)
                .map_err(|error| format!("无法设置编码输入时间戳：{error}"))?;
        }
        if let Some(duration) = self.timeline.advance(pts) {
            let (previous, _) = self.held.take().expect("a held sample with a previous pts");
            self.submit(previous, duration)?;
        }
        self.held = Some((sample, pts));
        Ok(())
    }

    pub fn finish(&mut self, now: Duration) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if let Some(duration) = self.timeline.finish(now)
            && let Some((sample, _)) = self.held.take()
        {
            self.submit(sample, duration)?;
        }
        // SAFETY: draining a configured transform; every remaining output sample
        // is pulled before the encoder is released.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(|error| format!("无法结束 H.264 编码：{error}"))?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .map_err(|error| format!("无法结束 H.264 编码流：{error}"))?;
        }
        self.drain()
    }

    pub fn take_samples(&mut self, out: &mut Vec<EncodedSample>) {
        out.extend(self.samples.drain(..));
    }

    pub fn parameter_sets(&self) -> Option<ParameterSets> {
        self.parameter_sets.clone()
    }

    fn submit(&mut self, sample: IMFSample, duration: u32) -> Result<(), String> {
        let duration_hns = i64::from(duration) * HNS_PER_SECOND / i64::from(TIMESCALE);
        // SAFETY: the transform is live and the sample is owned here.
        unsafe {
            sample
                .SetSampleDuration(duration_hns.max(1))
                .map_err(|error| format!("无法设置编码输入时长：{error}"))?;
            if self.frames_submitted == 0 {
                // Force an IDR first sample so the track is seekable.
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(|error| format!("无法标记首个关键帧：{error}"))?;
            }
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|error| format!("系统 H.264 编码器拒绝输入帧：{error}"))?;
        }
        self.frames_submitted += 1;
        self.drain()
    }

    /// Pull every compressed sample the transform has ready.
    fn drain(&mut self) -> Result<(), String> {
        loop {
            // SAFETY: an output sample with its own buffer is supplied for every
            // call, and both are released here.
            let (result, returned) = unsafe {
                let buffer = MFCreateMemoryBuffer(output_capacity(
                    &self.transform,
                    self.settings.width,
                    self.settings.height,
                )?)
                .map_err(|error| format!("无法创建编码输出缓冲：{error}"))?;
                let sample =
                    MFCreateSample().map_err(|error| format!("无法创建编码输出样本：{error}"))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|error| format!("无法绑定编码输出缓冲：{error}"))?;
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status = 0_u32;
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                // ProcessOutput transfers COM references through ManuallyDrop.
                let returned = ManuallyDrop::take(&mut output.pSample);
                let events = ManuallyDrop::take(&mut output.pEvents);
                drop(events);
                (result, returned)
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = returned {
                        self.convert_output(sample)?;
                    }
                    // The transform may have more samples queued.
                    continue;
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The real output type (and its sequence header) is only
                    // known once the first frame has been accepted.
                    self.read_sequence_header();
                    let media_type = self.output_type()?;
                    // SAFETY: selecting the stream-changed output type.
                    unsafe { self.transform.SetOutputType(0, &media_type, 0) }
                        .map_err(|error| format!("无法更新编码输出格式：{error}"))?;
                    continue;
                }
                Err(error) => return Err(format!("无法读取 H.264 编码输出：{error}")),
            }
        }
    }

    fn convert_output(&mut self, sample: IMFSample) -> Result<(), String> {
        // SAFETY: the sample was produced by the transform above.
        let bytes = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| format!("无法合并编码输出：{error}"))?;
            let mut pointer: *mut u8 = std::ptr::null_mut();
            let mut length = 0_u32;
            buffer
                .Lock(&mut pointer, None, Some(&mut length))
                .map_err(|error| format!("无法锁定编码输出：{error}"))?;
            let bytes = if pointer.is_null() || length == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(pointer, length as usize).to_vec()
            };
            buffer
                .Unlock()
                .map_err(|error| format!("无法解锁编码输出：{error}"))?;
            bytes
        };
        if bytes.is_empty() {
            return Ok(());
        }
        // A sync sample is one the transform flagged as a clean point; when the
        // attribute is missing, the bitstream itself says whether it holds an IDR.
        // SAFETY: attribute lookup on a live sample.
        let is_sync = unsafe {
            match sample.GetUINT32(&MFSampleExtension_CleanPoint) {
                Ok(value) => value != 0,
                Err(_) => nal::annex_b_is_sync(&bytes),
            }
        };
        // Parameter sets belong in the configuration box; keeping a copy inside
        // the sample as well would duplicate them. When the transform never
        // published a sequence header the in-band copies are what makes the
        // stream decodable, so they are kept.
        let avcc = if self.parameter_sets.is_some() {
            nal::to_avcc_skipping(&bytes, |unit_type| unit_type == 7 || unit_type == 8)
        } else {
            nal::to_avcc(&bytes)
        };
        let Some(avcc) = avcc else {
            return Ok(());
        };
        let (Some(pts_hns), Some(duration_hns)) = (
            unsafe { sample.GetSampleTime() }.ok(),
            unsafe { sample.GetSampleDuration() }.ok(),
        ) else {
            return Err("编码输出缺少时间戳".into());
        };
        let pts = u64::try_from(pts_hns.max(0)).unwrap_or(0);
        let duration = u32::try_from(duration_hns.max(1)).unwrap_or(u32::MAX);
        self.samples.push_back(EncodedSample {
            bytes: avcc,
            is_sync,
            pts: pts.saturating_mul(u64::from(TIMESCALE)) / HNS_PER_SECOND as u64,
            duration: duration.saturating_mul(TIMESCALE) / HNS_PER_SECOND as u32,
        });
        Ok(())
    }

    fn create_sample(&self, frame: &SourceFrame<'_>) -> Result<IMFSample, String> {
        let (width, height) = (self.settings.width as usize, self.settings.height as usize);
        let (bytes, capacity) = match (&self.layout, frame) {
            (
                InputLayout::Bgra { stride },
                SourceFrame::Bgra {
                    stride: source_stride,
                    bytes,
                },
            ) => {
                let required = source_stride.saturating_mul(height);
                if *stride < width * 4 || bytes.len() < required {
                    return Err("录制的 BGRA 帧与编码尺寸不匹配".into());
                }
                // The capture readback is already BGRA, which is the byte order
                // MFVideoFormat_ARGB32 describes.
                let mut packed = Vec::with_capacity(stride * height);
                for row in 0..height {
                    packed.extend_from_slice(
                        &bytes[row * source_stride..row * source_stride + width * 4],
                    );
                }
                (packed, stride * height)
            }
            (
                InputLayout::Nv12 { stride },
                SourceFrame::Nv12 {
                    y_stride,
                    y,
                    uv_stride,
                    uv,
                    ..
                },
            ) => {
                let luma_required = y_stride.saturating_mul(height);
                let chroma_height = height.div_ceil(2);
                let chroma_required = uv_stride.saturating_mul(chroma_height);
                let chroma_width = width.div_ceil(2) * 2;
                if *stride < width || y.len() < luma_required || uv.len() < chroma_required {
                    return Err("录制的 NV12 帧与编码尺寸不匹配".into());
                }
                let mut packed = Vec::with_capacity(stride * height + stride * chroma_height);
                for row in 0..height {
                    packed.extend_from_slice(&y[row * y_stride..row * y_stride + width]);
                    if *stride > width {
                        packed.resize(packed.len() + (stride - width), 0);
                    }
                }
                for row in 0..chroma_height {
                    packed.extend_from_slice(&uv[row * uv_stride..row * uv_stride + chroma_width]);
                    if *stride > chroma_width {
                        packed.resize(packed.len() + (stride - chroma_width), 0);
                    }
                }
                (packed, stride.saturating_mul(height + height.div_ceil(2)))
            }
            // A frame whose format does not match the negotiated input type
            // cannot be encoded without a conversion this backend refuses to
            // perform silently.
            _ => return Err("录制帧格式与编码器输入格式不一致".into()),
        };
        let length = u32::try_from(bytes.len()).map_err(|_| "录制帧过大".to_owned())?;
        let capacity =
            u32::try_from(capacity.max(bytes.len())).map_err(|_| "录制帧过大".to_owned())?;
        // SAFETY: the buffer and sample are created and immediately populated;
        // both are owned by the returned sample.
        unsafe {
            let buffer = MFCreateMemoryBuffer(capacity)
                .map_err(|error| format!("无法创建编码输入缓冲：{error}"))?;
            let mut pointer: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(|error| format!("无法锁定编码输入缓冲：{error}"))?;
            if pointer.is_null() {
                let _ = buffer.Unlock();
                return Err("编码输入缓冲不可写".into());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(length)
                .map_err(|error| format!("无法设置编码输入长度：{error}"))?;
            let sample =
                MFCreateSample().map_err(|error| format!("无法创建编码输入样本：{error}"))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|error| format!("无法绑定编码输入缓冲：{error}"))?;
            Ok(sample)
        }
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish(Duration::ZERO);
        }
    }
}

/// Size of an output sample buffer: the transform's own estimate when it gives
/// one, and a generous multiple of the raw frame size otherwise.
fn output_capacity(transform: &IMFTransform, width: u32, height: u32) -> Result<u32, String> {
    // SAFETY: stream information is queried on a configured transform.
    let info = unsafe { transform.GetOutputStreamInfo(0) }
        .map_err(|error| format!("无法读取编码输出流信息：{error}"))?;
    if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
        // The transform allocates its own samples; the buffer is unused but must
        // still exist for the call.
        return Ok(1024);
    }
    let minimum = info.cbSize;
    let estimate = width
        .saturating_mul(height)
        .saturating_mul(2)
        .saturating_add(1 << 16);
    Ok(minimum.max(estimate))
}

/// Create the system H.264 encoder transform, preferring hardware.
fn create_h264_transform() -> Result<IMFTransform, String> {
    // SAFETY: Media Foundation is initialized on this thread. Every COM pointer
    // returned by the enumeration is released here, and the transform is owned
    // by the caller.
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0_u32;
        let output_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let enumerated = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        );
        if enumerated.is_ok() && count > 0 && !activates.is_null() {
            // The flags sort hardware encoders first, so the first entry is the
            // best available transform.
            let first = (*activates).clone();
            let hardware =
                first.and_then(|activate| activate.ActivateObject::<IMFTransform>().ok());
            for index in 0..count as usize {
                drop((*activates.add(index)).take());
            }
            CoTaskMemFree(Some(activates as *const core::ffi::c_void));
            if let Some(transform) = hardware {
                return Ok(transform);
            }
        } else if !activates.is_null() {
            CoTaskMemFree(Some(activates as *const core::ffi::c_void));
        }
        CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("系统没有可用的 H.264 编码器：{error}"))
    }
}

/// Position on the recording timeline in Media Foundation's 100 ns units.
fn position_hns(pts: Duration) -> i64 {
    let units = timeline_position(pts);
    let hns = units.saturating_mul(HNS_PER_SECOND as u64) / u64::from(TIMESCALE);
    i64::try_from(hns).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{HNS_PER_SECOND, position_hns};
    use crate::recording::TIMESCALE;
    use std::time::Duration;

    #[test]
    fn positions_convert_to_media_foundation_units() {
        assert_eq!(position_hns(Duration::ZERO), 0);
        assert_eq!(position_hns(Duration::from_secs(1)), HNS_PER_SECOND);
        // One 60 Hz frame in 100 ns units.
        assert_eq!(position_hns(Duration::from_nanos(16_666_667)), 166_667);
    }

    #[test]
    fn positions_are_monotonic_at_the_track_timescale() {
        let step = Duration::from_nanos(1_000_000_000 / TIMESCALE as u64);
        assert!(position_hns(step) > 0);
        assert!(position_hns(step * 2) > position_hns(step));
    }
}
