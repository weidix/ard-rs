//! Windows Media Foundation Transform decoder for the AVC media stream.
//!
//! The decoder is deliberately synchronous and thread-confined: the owning
//! media thread initializes COM/MF, feeds whole Annex-B access units to the
//! inbox MFT, and copies its NV12 output before releasing the COM sample.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::mem::ManuallyDrop;

use ard_rs::media_stream::{AccessUnit, MediaStreamCodec};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};

use super::{DecodedOutput, DecodedSlice, YuvMatrix, YuvRange};

#[derive(Debug, Clone, Copy)]
struct Submission {
    stream_index: usize,
    timestamp: u32,
    submission: u64,
    encoded_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct OutputFormat {
    width: u32,
    height: u32,
    stride: usize,
    range: YuvRange,
    matrix: YuvMatrix,
}

/// Owns the thread's COM and Media Foundation initialization.
struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    fn new() -> Result<Self, String> {
        // SAFETY: this object is constructed and dropped on the dedicated
        // decoder thread. No COM interface escapes that thread.
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
        // SAFETY: paired with successful initialization above and dropped on
        // the same thread after every MFT COM pointer has been released.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

struct NativeDecoder {
    transform: IMFTransform,
    output_format: Option<OutputFormat>,
    pending: BTreeMap<i64, Submission>,
    // Keep this field last so the transform is released before MFShutdown.
    _runtime: MediaFoundationRuntime,
}

impl NativeDecoder {
    fn new(codec: MediaStreamCodec) -> Result<Self, String> {
        let runtime = MediaFoundationRuntime::new()?;
        let clsid = match codec {
            MediaStreamCodec::H264 => CLSID_MSH264DecoderMFT,
            MediaStreamCodec::Hevc => CLSID_MSH265DecoderMFT,
        };
        let subtype = match codec {
            MediaStreamCodec::H264 => MFVideoFormat_H264,
            MediaStreamCodec::Hevc => MFVideoFormat_HEVC,
        };
        // SAFETY: the CLSIDs name Windows' inbox video decoder MFTs. COM and
        // Media Foundation are initialized on this thread and the returned
        // smart pointer owns its reference count.
        let transform: IMFTransform = unsafe {
            CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| format!("创建系统视频解码 MFT 失败：{error}"))?
        };
        let input_type = unsafe { MFCreateMediaType() }
            .map_err(|error| format!("创建 MFT 输入格式失败：{error}"))?;
        unsafe {
            // Screen sharing is an all-interactive, no-B-frame workload. The
            // inbox decoders accept this standard attribute and avoid holding
            // a completed band for presentation reordering.
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| input_type.SetGUID(&MF_MT_SUBTYPE, &subtype))
                .and_then(|_| {
                    input_type
                        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                })
                .and_then(|_| transform.SetInputType(0, &input_type, 0))
                .and_then(|_| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0))
                .and_then(|_| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(|error| format!("配置系统视频解码 MFT 失败：{error}"))?;
        }
        Ok(Self {
            transform,
            output_format: None,
            pending: BTreeMap::new(),
            _runtime: runtime,
        })
    }

    fn submit(
        &mut self,
        codec: MediaStreamCodec,
        unit: &AccessUnit,
        metadata: Submission,
    ) -> Result<Vec<DecodedOutput>, String> {
        let bytes = unit.to_annex_b();
        let sample_time = i64::try_from(metadata.submission)
            .map_err(|_| "MFT 提交序号超出时间戳范围".to_owned())?
            .saturating_add(1);
        let sample = create_input_sample(&bytes, sample_time, is_sync_unit(codec, unit))?;
        let mut outputs = self.drain_available()?;
        // A synchronous decoder can refuse input until all pending output has
        // been collected. Drain once more and retry exactly once in that case.
        let result = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        if result
            .as_ref()
            .is_err_and(|error| error.code() == MF_E_NOTACCEPTING)
        {
            outputs.extend(self.drain_available()?);
            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .map_err(|error| format!("MFT 拒绝压缩帧输入：{error}"))?;
        } else {
            result.map_err(|error| format!("MFT 压缩帧输入失败：{error}"))?;
        }
        self.pending.insert(sample_time, metadata);
        outputs.extend(self.drain_available()?);
        Ok(outputs)
    }

    fn drain_available(&mut self) -> Result<Vec<DecodedOutput>, String> {
        let mut decoded = Vec::new();
        loop {
            let sample = self.output_sample()?;
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };
            // ProcessOutput transfers ordinary COM references through these
            // fields, but the generated FFI structure uses ManuallyDrop.
            let returned_sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
            drop(events);

            match result {
                Ok(()) => {
                    let sample = returned_sample
                        .ok_or_else(|| "MFT 成功返回但没有输出 sample".to_owned())?;
                    decoded.push(self.convert_output(sample, output.dwStatus)?);
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.select_nv12_output_type()?;
                }
                Err(error) => return Err(format!("MFT 获取解码输出失败：{error}")),
            }
        }
        Ok(decoded)
    }

    fn output_sample(&self) -> Result<Option<IMFSample>, String> {
        let Some(format) = self.output_format else {
            return Ok(None);
        };
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|error| format!("读取 MFT 输出流信息失败：{error}"))?;
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
            return Ok(None);
        }
        let minimum = format
            .stride
            .checked_mul(format.height as usize)
            .and_then(|y| {
                y.checked_add(
                    format
                        .stride
                        .checked_mul(format.height.div_ceil(2) as usize)?,
                )
            })
            .ok_or_else(|| "MFT NV12 输出大小溢出".to_owned())?;
        let capacity = usize::max(minimum, info.cbSize as usize);
        let capacity = u32::try_from(capacity).map_err(|_| "MFT 输出 sample 过大".to_owned())?;
        let buffer = unsafe { MFCreateMemoryBuffer(capacity) }
            .map_err(|error| format!("创建 MFT 输出 buffer 失败：{error}"))?;
        let sample = unsafe { MFCreateSample() }
            .map_err(|error| format!("创建 MFT 输出 sample 失败：{error}"))?;
        unsafe { sample.AddBuffer(&buffer) }
            .map_err(|error| format!("绑定 MFT 输出 buffer 失败：{error}"))?;
        Ok(Some(sample))
    }

    fn select_nv12_output_type(&mut self) -> Result<(), String> {
        for index in 0..256 {
            let media_type = match unsafe { self.transform.GetOutputAvailableType(0, index) } {
                Ok(media_type) => media_type,
                Err(_) => break,
            };
            let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) };
            if subtype != Ok(MFVideoFormat_NV12) {
                continue;
            }
            unsafe { self.transform.SetOutputType(0, &media_type, 0) }
                .map_err(|error| format!("选择 MFT NV12 输出失败：{error}"))?;
            let packed_size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|error| format!("MFT NV12 输出缺少帧尺寸：{error}"))?;
            let width = (packed_size >> 32) as u32;
            let height = packed_size as u32;
            if width == 0 || height == 0 {
                return Err("MFT 返回了零尺寸 NV12 输出".into());
            }
            let stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
                .ok()
                .map(|value| (value as i32).unsigned_abs() as usize)
                .unwrap_or(width as usize);
            if stride < width as usize {
                return Err(format!(
                    "MFT NV12 stride 小于宽度：stride={stride} width={width}"
                ));
            }
            let range = match unsafe { media_type.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
                Ok(value) if value == MFNominalRange_0_255.0 as u32 => YuvRange::Full,
                _ => YuvRange::Video,
            };
            let matrix = match unsafe { media_type.GetUINT32(&MF_MT_YUV_MATRIX) } {
                Ok(value) if value == MFVideoTransferMatrix_BT601.0 as u32 => YuvMatrix::Bt601,
                Ok(value)
                    if value == MFVideoTransferMatrix_BT2020_10.0 as u32
                        || value == MFVideoTransferMatrix_BT2020_12.0 as u32 =>
                {
                    YuvMatrix::Bt2020
                }
                _ => YuvMatrix::Bt709,
            };
            self.output_format = Some(OutputFormat {
                width,
                height,
                stride,
                range,
                matrix,
            });
            return Ok(());
        }
        Err("系统视频解码 MFT 不支持 NV12 输出".into())
    }

    fn convert_output(
        &mut self,
        sample: IMFSample,
        info_flags: u32,
    ) -> Result<DecodedOutput, String> {
        let format = self
            .output_format
            .ok_or_else(|| "MFT 在输出格式确定前返回了 sample".to_owned())?;
        let sample_time = unsafe { sample.GetSampleTime() }.ok();
        let metadata = sample_time
            .and_then(|time| self.pending.remove(&time))
            .or_else(|| {
                let first = self.pending.first_key_value().map(|(&time, _)| time)?;
                self.pending.remove(&first)
            })
            .ok_or_else(|| "MFT 输出无法对应到已提交的 access unit".to_owned())?;
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|error| format!("合并 MFT NV12 buffer 失败：{error}"))?;
        let frame = copy_nv12(&buffer, format)?;
        Ok(DecodedOutput {
            stream_index: metadata.stream_index,
            timestamp: metadata.timestamp,
            submission: metadata.submission,
            encoded_bytes: metadata.encoded_bytes,
            status: 0,
            info_flags,
            conversion_error: None,
            frame: Some(frame),
        })
    }

    fn flush(&mut self) -> Result<Vec<DecodedOutput>, String> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|error| format!("MFT drain 失败：{error}"))?;
        let outputs = self.drain_available()?;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(|error| format!("MFT flush 失败：{error}"))?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|error| format!("重新启动 MFT 输入流失败：{error}"))?;
        }
        self.pending.clear();
        Ok(outputs)
    }

    fn discard_prediction_chain(&mut self) {
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        };
        self.pending.clear();
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
        };
    }
}

pub struct MftDecoder {
    codec: MediaStreamCodec,
    native: Option<NativeDecoder>,
    next_submission: u64,
    frames_decoded: u64,
    needs_sync: bool,
    errors: VecDeque<String>,
}

impl MftDecoder {
    pub fn new(codec: MediaStreamCodec) -> Self {
        let mut errors = VecDeque::new();
        let native = NativeDecoder::new(codec)
            .map_err(|error| errors.push_back(error))
            .ok();
        Self {
            codec,
            native,
            next_submission: 0,
            frames_decoded: 0,
            needs_sync: false,
            errors,
        }
    }

    pub(crate) fn require_sync(&mut self) {
        if let Some(native) = &mut self.native {
            native.discard_prediction_chain();
        }
        self.needs_sync = true;
    }

    pub(crate) fn decode(&mut self, stream_index: usize, unit: &AccessUnit) -> Vec<DecodedOutput> {
        let sync = is_sync_unit(self.codec, unit);
        if self.needs_sync && !sync {
            return Vec::new();
        }
        if sync {
            self.needs_sync = false;
        }
        let submission = self.next_submission;
        self.next_submission = self.next_submission.wrapping_add(1);
        let metadata = Submission {
            stream_index,
            timestamp: unit.timestamp,
            submission,
            encoded_bytes: unit.avcc_len(),
        };
        let Some(native) = &mut self.native else {
            return Vec::new();
        };
        match native.submit(self.codec, unit, metadata) {
            Ok(outputs) => self.order_outputs(outputs),
            Err(error) => {
                self.errors.push_back(format!(
                    "MFT decode failed: stream={stream_index} timestamp={} submission={submission}: {error}",
                    unit.timestamp
                ));
                Vec::new()
            }
        }
    }

    pub(crate) fn finish_frame(&mut self) -> Vec<DecodedOutput> {
        let Some(native) = &mut self.native else {
            return Vec::new();
        };
        match native.drain_available() {
            Ok(outputs) => self.order_outputs(outputs),
            Err(error) => {
                self.errors.push_back(error);
                Vec::new()
            }
        }
    }

    pub(crate) fn flush(&mut self) -> Vec<DecodedOutput> {
        let Some(native) = &mut self.native else {
            return Vec::new();
        };
        match native.flush() {
            Ok(outputs) => self.order_outputs(outputs),
            Err(error) => {
                self.errors.push_back(error);
                Vec::new()
            }
        }
    }

    pub(crate) fn take_errors(&mut self) -> Vec<String> {
        self.errors.drain(..).collect()
    }

    fn order_outputs(&mut self, mut outputs: Vec<DecodedOutput>) -> Vec<DecodedOutput> {
        outputs.sort_by_key(|output| output.submission);
        self.frames_decoded = self.frames_decoded.saturating_add(outputs.len() as u64);
        outputs
    }
}

fn is_sync_unit(codec: MediaStreamCodec, unit: &AccessUnit) -> bool {
    unit.nal_units.iter().any(|nal| match codec {
        MediaStreamCodec::H264 => matches!(nal.first().map(|byte| byte & 0x1f), Some(5)),
        MediaStreamCodec::Hevc => {
            matches!(nal.first().map(|byte| (byte >> 1) & 0x3f), Some(16..=23))
        }
    })
}

fn create_input_sample(
    bytes: &[u8],
    sample_time: i64,
    clean_point: bool,
) -> Result<IMFSample, String> {
    let length = u32::try_from(bytes.len()).map_err(|_| "压缩视频 access unit 过大".to_owned())?;
    let buffer = unsafe { MFCreateMemoryBuffer(length) }
        .map_err(|error| format!("创建 MFT 输入 buffer 失败：{error}"))?;
    let mut destination = std::ptr::null_mut();
    unsafe { buffer.Lock(&mut destination, None, None) }
        .map_err(|error| format!("锁定 MFT 输入 buffer 失败：{error}"))?;
    let lock = BufferLock::new(&buffer, destination);
    // SAFETY: MFCreateMemoryBuffer allocated at least `length` bytes and the
    // lock remains held for the duration of this copy.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), lock.pointer, bytes.len()) };
    drop(lock);
    unsafe { buffer.SetCurrentLength(length) }
        .map_err(|error| format!("设置 MFT 输入长度失败：{error}"))?;
    let sample = unsafe { MFCreateSample() }
        .map_err(|error| format!("创建 MFT 输入 sample 失败：{error}"))?;
    unsafe { sample.AddBuffer(&buffer) }
        .map_err(|error| format!("绑定 MFT 输入 buffer 失败：{error}"))?;
    unsafe { sample.SetSampleTime(sample_time) }
        .map_err(|error| format!("设置 MFT sample 时间失败：{error}"))?;
    unsafe { sample.SetSampleDuration(1) }
        .map_err(|error| format!("设置 MFT sample 时长失败：{error}"))?;
    if clean_point {
        unsafe { sample.SetUINT32(&MFSampleExtension_CleanPoint, 1) }
            .map_err(|error| format!("标记 MFT 关键帧失败：{error}"))?;
    }
    Ok(sample)
}

struct BufferLock<'a> {
    buffer: &'a IMFMediaBuffer,
    pointer: *mut u8,
}

impl<'a> BufferLock<'a> {
    fn new(buffer: &'a IMFMediaBuffer, pointer: *mut u8) -> Self {
        Self { buffer, pointer }
    }
}

impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.buffer.Unlock() };
    }
}

fn copy_nv12(buffer: &IMFMediaBuffer, format: OutputFormat) -> Result<DecodedSlice, String> {
    let mut pointer = std::ptr::null_mut();
    let mut current_length = 0;
    unsafe { buffer.Lock(&mut pointer, None, Some(&mut current_length)) }
        .map_err(|error| format!("锁定 MFT NV12 buffer 失败：{error}"))?;
    let lock = BufferLock::new(buffer, pointer);
    let y_storage = format
        .stride
        .checked_mul(format.height as usize)
        .ok_or_else(|| "MFT NV12 亮度平面大小溢出".to_owned())?;
    let uv_rows = format.height.div_ceil(2) as usize;
    let required = y_storage
        .checked_add(
            format
                .stride
                .checked_mul(uv_rows)
                .ok_or_else(|| "MFT NV12 色度平面大小溢出".to_owned())?,
        )
        .ok_or_else(|| "MFT NV12 buffer 大小溢出".to_owned())?;
    if (current_length as usize) < required {
        return Err(format!(
            "MFT NV12 buffer 被截断：required={required} actual={current_length}"
        ));
    }
    let width = format.width as usize;
    let height = format.height as usize;
    let mut y_plane = vec![0; width * height];
    let mut uv_plane = vec![0; width * uv_rows];
    for row in 0..height {
        // SAFETY: validated the complete padded source size above; both
        // destination slices are tightly allocated for width*height bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                lock.pointer.add(row * format.stride),
                y_plane.as_mut_ptr().add(row * width),
                width,
            );
        }
    }
    for row in 0..uv_rows {
        unsafe {
            std::ptr::copy_nonoverlapping(
                lock.pointer.add(y_storage + row * format.stride),
                uv_plane.as_mut_ptr().add(row * width),
                width,
            );
        }
    }
    drop(lock);
    Ok(DecodedSlice {
        width: format.width,
        height: format.height,
        y_plane,
        uv_plane,
        range: format.range,
        matrix: format.matrix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_detection_is_codec_specific() {
        let h264_idr = AccessUnit {
            timestamp: 1,
            nal_units: vec![vec![0x65]],
        };
        let h264_predictive = AccessUnit {
            timestamp: 2,
            nal_units: vec![vec![0x41]],
        };
        let hevc_irap = AccessUnit {
            timestamp: 3,
            nal_units: vec![vec![19 << 1]],
        };
        assert!(is_sync_unit(MediaStreamCodec::H264, &h264_idr));
        assert!(!is_sync_unit(MediaStreamCodec::H264, &h264_predictive));
        assert!(is_sync_unit(MediaStreamCodec::Hevc, &hevc_irap));
        assert!(!is_sync_unit(MediaStreamCodec::Hevc, &h264_idr));
    }
}
