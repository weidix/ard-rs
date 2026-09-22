//! VideoToolbox H.264 encoder for session recording.
//!
//! The backend is deliberately synchronous from the caller's point of view: the
//! recording thread hands it one presented frame at a time and drains finished
//! access units from a queue that VideoToolbox fills from its own callback
//! thread. Frames are copied into IOSurface-backed pixel buffers, so the system
//! encoder can hand them to the media engine without another copy.
//!
//! `media::vt` is the decode half of the same framework and is kept separate on
//! purpose: decoding is driven by network access units, encoding by presented
//! frames, and the two share nothing but the framework's naming.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::sync::Mutex;
use std::time::Duration;

use super::{EncodedSample, EncoderSettings, FrameTimeline, ParameterSets, SourceFrame};
use crate::media::{YuvMatrix, YuvRange};
use crate::recording::{TIMESCALE, timeline_position};

type OSStatus = i32;
type CFIndex = isize;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFBooleanRef = *const c_void;
type CFNumberRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CVImageBufferRef = *const c_void;
type CVPixelBufferRef = *const c_void;
type VTCompressionSessionRef = *const c_void;
type VTEncodeInfoFlags = u32;

/// `avc1`, the H.264 codec type identifier.
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x6176_6331;
/// `BGRA`, packed 8-bit blue/green/red/alpha.
const K_CV_PIXEL_FORMAT_32_BGRA: u32 = 0x4247_5241;
/// `420v`, biplanar 4:2:0 with video-range luma.
const K_CV_PIXEL_FORMAT_420V: u32 = 0x3432_3076;
/// `420f`, biplanar 4:2:0 with full-range luma.
const K_CV_PIXEL_FORMAT_420F: u32 = 0x3432_3066;
const K_CF_NUMBER_SINT32: i32 = 3;
const K_CF_NUMBER_FLOAT64: i32 = 13;
/// `kCVAttachmentMode_ShouldPropagate`.
const K_CV_ATTACHMENT_MODE_PROPAGATE: u32 = 1;
/// Valid `CMTime` flags value; an all-zero `CMTime` means "invalid".
const K_CM_TIME_FLAGS_VALID: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

impl CMTime {
    fn invalid() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }

    fn units(value: u64) -> Self {
        Self {
            value: value as i64,
            timescale: TIMESCALE as i32,
            flags: K_CM_TIME_FLAGS_VALID,
            epoch: 0,
        }
    }
}

type VTCompressionOutputCallback = unsafe extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTEncodeInfoFlags,
    sample_buffer: CMSampleBufferRef,
);

#[allow(clippy::duplicated_attributes)]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    /// Standard CoreFoundation collection callbacks: they retain, release and
    /// compare keys and values as CF types. Passing NULL instead makes a
    /// non-retaining collection, which CoreVideo crashes on when one is nested
    /// inside another (measured: an IOSurface-properties dictionary).
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(the_array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(the_array: CFArrayRef, index: CFIndex) -> *const c_void;
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(the_dict: CFDictionaryRef, key: *const c_void, value: *const c_void);
    fn CFDictionaryContainsKey(the_dict: CFDictionaryRef, key: *const c_void) -> u8;
    fn CFEqual(first: CFTypeRef, second: CFTypeRef) -> u8;
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFNumberRef;

    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    fn CVPixelBufferCreate(
        allocator: CFAllocatorRef,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> OSStatus;
    fn CVPixelBufferRelease(pixel_buffer: CVPixelBufferRef);
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u32) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u32) -> OSStatus;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVBufferSetAttachment(
        buffer: CVImageBufferRef,
        key: CFStringRef,
        value: CFTypeRef,
        attachment_mode: u32,
    );
    static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_2020: CFStringRef;
    static kCVImageBufferColorPrimariesKey: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunctionKey: CFStringRef;
    static kCVImageBufferTransferFunction_sRGB: CFStringRef;

    fn CMSampleBufferGetDataBuffer(sample_buffer: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(
        sample_buffer: CMSampleBufferRef,
    ) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample_buffer: CMSampleBufferRef,
        create_if_necessary: bool,
    ) -> CFArrayRef;
    fn CMBlockBufferGetDataLength(the_buffer: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        the_source_buffer: CMBlockBufferRef,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        video_desc: CMFormatDescriptionRef,
        parameter_set_index: usize,
        parameter_set_pointer_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut c_int,
    ) -> OSStatus;

    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;
    fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: Option<VTCompressionOutputCallback>,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTEncodeInfoFlags,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    fn VTSessionCopyProperty(
        session: VTCompressionSessionRef,
        property_key: CFStringRef,
        allocator: CFAllocatorRef,
        property_value_out: *mut CFTypeRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(
        session: VTCompressionSessionRef,
        property_key: CFStringRef,
        property_value: CFTypeRef,
    ) -> OSStatus;
}

/// Per-frame metadata handed to VideoToolbox and returned to the callback, which
/// is the only way an asynchronously encoded sample learns its own timeline.
#[derive(Debug, Clone, Copy)]
struct FrameMeta {
    pts: u64,
    duration: u32,
}

#[derive(Debug, Default)]
struct CallbackContext {
    samples: Mutex<VecDeque<EncodedSample>>,
    parameter_sets: Mutex<Option<ParameterSets>>,
    error: Mutex<Option<String>>,
}

impl CallbackContext {
    fn push_error(&self, message: String) {
        if let Ok(mut error) = self.error.lock()
            && error.is_none()
        {
            *error = Some(message);
        }
    }
}

/// Frames arrive from VideoToolbox's own callback thread, so the queue is shared.
unsafe extern "C" fn compression_output(
    ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: CMSampleBufferRef,
) {
    // SAFETY: `ref_con` is the context pointer passed to
    // `VTCompressionSessionCreate`, which outlives every callback because the
    // session is invalidated before the context is dropped. `source_frame_ref_con`
    // is a `FrameMeta` box this module allocated for exactly one callback.
    unsafe {
        if ref_con.is_null() {
            return;
        }
        let context = &*(ref_con as *const CallbackContext);
        let meta = if source_frame_ref_con.is_null() {
            None
        } else {
            Some(*Box::from_raw(source_frame_ref_con as *mut FrameMeta))
        };
        if status != 0 {
            context.push_error(format!("VideoToolbox 编码帧失败（状态 {status}）"));
            return;
        }
        let (Some(meta), false) = (meta, sample_buffer.is_null()) else {
            return;
        };
        match extract_sample(context, sample_buffer, meta) {
            Ok(sample) => {
                if let Ok(mut samples) = context.samples.lock() {
                    samples.push_back(sample);
                }
            }
            Err(error) => context.push_error(error),
        }
    }
}

/// Copy one compressed access unit out of the framework's sample buffer.
///
/// # Safety
/// `sample_buffer` must be a valid compressed sample buffer produced by the
/// session that owns `context`.
unsafe fn extract_sample(
    context: &CallbackContext,
    sample_buffer: CMSampleBufferRef,
    meta: FrameMeta,
) -> Result<EncodedSample, String> {
    // SAFETY: the caller guarantees a live sample buffer; every function below
    // is a reading accessor on it.
    unsafe {
        let description = CMSampleBufferGetFormatDescription(sample_buffer);
        if description.is_null() {
            return Err("VideoToolbox 输出缺少格式描述".into());
        }
        if context
            .parameter_sets
            .lock()
            .map(|sets| sets.is_none())
            .unwrap_or(false)
        {
            let sets = h264_parameter_sets(description)?;
            if let Ok(mut stored) = context.parameter_sets.lock() {
                *stored = Some(sets);
            }
        }
        let block = CMSampleBufferGetDataBuffer(sample_buffer);
        if block.is_null() {
            return Err("VideoToolbox 输出缺少压缩数据".into());
        }
        let length = CMBlockBufferGetDataLength(block);
        if length == 0 {
            return Err("VideoToolbox 输出为空".into());
        }
        let mut bytes = vec![0_u8; length];
        let status = CMBlockBufferCopyDataBytes(block, 0, length, bytes.as_mut_ptr().cast());
        if status != 0 {
            return Err(format!("无法读取 VideoToolbox 输出（状态 {status}）"));
        }
        Ok(EncodedSample {
            bytes,
            is_sync: sample_is_sync(sample_buffer),
            pts: meta.pts,
            duration: meta.duration,
        })
    }
}

/// Whether the sample buffer carries no `NotSync` attachment.
///
/// A compressed sample with no attachments at all is a keyframe; VideoToolbox
/// only marks frames that cannot be seeked to.
///
/// # Safety
/// `sample_buffer` must be a live compressed sample buffer.
unsafe fn sample_is_sync(sample_buffer: CMSampleBufferRef) -> bool {
    // SAFETY: reading accessors on a live sample buffer.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, false);
        if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
            return true;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0);
        if first.is_null() {
            return true;
        }
        CFDictionaryContainsKey(first, kCMSampleAttachmentKey_NotSync) == 0
    }
}

/// SPS and PPS of the encoded stream, without start codes.
///
/// # Safety
/// `description` must be a live H.264 format description.
unsafe fn h264_parameter_sets(
    description: CMFormatDescriptionRef,
) -> Result<ParameterSets, String> {
    let mut sets = Vec::with_capacity(2);
    for index in 0..2_usize {
        let mut pointer: *const u8 = std::ptr::null();
        let mut size = 0_usize;
        let mut count = 0_usize;
        let mut header_length: c_int = 0;
        // SAFETY: the caller guarantees a live format description, and the out
        // pointers are local to this frame.
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                index,
                &mut pointer,
                &mut size,
                &mut count,
                &mut header_length,
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            return Err(format!("无法读取 H.264 参数集（状态 {status}）"));
        }
        if header_length != 4 {
            return Err(format!(
                "VideoToolbox 输出的 NAL 长度前缀为 {header_length} 字节，MP4 只支持 4 字节"
            ));
        }
        // SAFETY: the framework owns `pointer` for `size` bytes for as long as
        // the format description lives.
        sets.push(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec());
    }
    let pps = sets.pop().expect("two parameter sets were pushed");
    let sps = sets.pop().expect("two parameter sets were pushed");
    Ok(ParameterSets { sps, pps })
}

struct HeldImage {
    image: CVPixelBufferRef,
    pts: Duration,
}

pub(crate) struct VideoToolboxEncoder {
    session: VTCompressionSessionRef,
    settings: EncoderSettings,
    timeline: FrameTimeline,
    held: Option<HeldImage>,
    frames_submitted: u64,
    finished: bool,
    /// Kept alive for as long as the session can call back into it.
    context: Box<CallbackContext>,
}

impl VideoToolboxEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, String> {
        if settings.width == 0 || settings.height == 0 {
            return Err("录制尺寸无效".into());
        }
        let mut context = Box::new(CallbackContext::default());
        let context_pointer: *mut c_void = (&mut *context as *mut CallbackContext).cast();
        let width = i32::try_from(settings.width).map_err(|_| "录制宽度超出编码器范围")?;
        let height = i32::try_from(settings.height).map_err(|_| "录制高度超出编码器范围")?;
        // SAFETY: every framework object created here is released in `Drop`, and
        // the callback context outlives the session because the session is
        // invalidated before the field is dropped.
        let session = unsafe {
            let specification = dictionary(&[(
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                kCFBooleanTrue as CFTypeRef,
            )]);
            let mut session: VTCompressionSessionRef = std::ptr::null();
            let status = VTCompressionSessionCreate(
                std::ptr::null(),
                width,
                height,
                K_CM_VIDEO_CODEC_TYPE_H264,
                specification,
                std::ptr::null(),
                std::ptr::null(),
                Some(compression_output),
                context_pointer,
                &mut session,
            );
            CFRelease(specification.cast());
            if status != 0 || session.is_null() {
                return Err(format!("无法创建 VideoToolbox 编码会话（状态 {status}）"));
            }
            session
        };
        let encoder = Self {
            session,
            settings,
            timeline: FrameTimeline::default(),
            held: None,
            frames_submitted: 0,
            finished: false,
            context,
        };
        encoder.configure();
        // SAFETY: the session was created above and is live.
        unsafe {
            let _ = VTCompressionSessionPrepareToEncodeFrames(encoder.session);
        }
        Ok(encoder)
    }

    /// Best-effort rate control and latency configuration.
    ///
    /// A screen recording is live content: frames are captured as they are
    /// presented, so the encoder must not queue work it cannot keep up with.
    /// Real-time mode plus a bitrate target keeps latency bounded, while the
    /// average bitrate is what actually decides how much text detail survives.
    fn configure(&self) {
        let fps = self.settings.frame_rate_hint.max(1) as f64;
        // SAFETY: property keys are framework-provided strings and the values
        // are either framework booleans or CFNumbers created here and released
        // after the call that copies them.
        unsafe {
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_RealTime,
                kCFBooleanTrue,
            );
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            );
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
            );
            let bitrate = number_i32(self.settings.bitrate.min(i32::MAX as u32) as i32);
            if !bitrate.is_null() {
                let _ = VTSessionSetProperty(
                    self.session,
                    kVTCompressionPropertyKey_AverageBitRate,
                    bitrate,
                );
                CFRelease(bitrate.cast());
            }
            let frame_rate = number_f64(fps);
            if !frame_rate.is_null() {
                let _ = VTSessionSetProperty(
                    self.session,
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    frame_rate,
                );
                CFRelease(frame_rate.cast());
            }
            // Describe the colour space in the bitstream itself. Without this
            // the player has to guess, and a guess of BT.601 for a frame the
            // encoder converted with BT.709 shifts every colour — measured as a
            // mean channel error of 45 on a smooth gradient.
            //
            // The tags describe what the presenter puts on screen, not what the
            // stream declared: the presenter shows the decoded planes' numbers
            // as sRGB, so the recording is sRGB too. Tagging the stream's
            // Display P3 instead makes a player show the take's flat background
            // 39 levels of red away from the viewer it was recorded from
            // (measured through CoreImage: red 13 against the viewer's 52).
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            );
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_sRGB,
            );
            let _ = VTSessionSetProperty(
                self.session,
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            );
            // Two-second keyframe spacing keeps the file seekable without
            // inflating a mostly static desktop recording.
            let keyframe_interval = number_f64(2.0);
            if !keyframe_interval.is_null() {
                let _ = VTSessionSetProperty(
                    self.session,
                    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                    keyframe_interval,
                );
                CFRelease(keyframe_interval.cast());
            }
        }
    }

    /// Encode the frame held since the previous call, then hold this one.
    pub fn push(&mut self, frame: SourceFrame<'_>, pts: Duration) -> Result<(), String> {
        if self.finished {
            return Err("编码器已经结束".into());
        }
        let image = self.create_image(&frame)?;
        if let Some(duration) = self.timeline.advance(pts) {
            let held = self.held.take().expect("a held frame with a previous pts");
            self.submit(held.image, held.pts, duration)?;
        }
        self.held = Some(HeldImage { image, pts });
        Ok(())
    }

    /// Encode the held frame with the duration measured up to `now` and flush.
    pub fn finish(&mut self, now: Duration) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if let Some(duration) = self.timeline.finish(now)
            && let Some(held) = self.held.take()
        {
            self.submit(held.image, held.pts, duration)?;
        }
        // SAFETY: the session is live and every callback has fired once this
        // returns, which is what makes the drained queue complete.
        let status = unsafe { VTCompressionSessionCompleteFrames(self.session, CMTime::invalid()) };
        if status != 0 {
            return Err(format!("无法结束 VideoToolbox 编码（状态 {status}）"));
        }
        self.take_error()
    }

    pub fn take_samples(&mut self, out: &mut Vec<EncodedSample>) {
        if let Ok(mut samples) = self.context.samples.lock() {
            out.extend(samples.drain(..));
        }
    }

    /// Whether VideoToolbox chose a hardware encoder for this session.
    pub fn hardware_accelerated(&self) -> Option<bool> {
        let mut value: CFTypeRef = std::ptr::null();
        // SAFETY: the session is live and the out pointer is local; the returned
        // object is released here.
        let status = unsafe {
            VTSessionCopyProperty(
                self.session,
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
                std::ptr::null(),
                &mut value,
            )
        };
        if status != 0 || value.is_null() {
            return None;
        }
        // SAFETY: a CFBoolean is compared by identity with the framework's
        // singletons.
        let accelerated = unsafe {
            let result = CFEqual(value, kCFBooleanTrue) != 0;
            CFRelease(value);
            result
        };
        Some(accelerated)
    }

    pub fn parameter_sets(&self) -> Option<ParameterSets> {
        self.context
            .parameter_sets
            .lock()
            .ok()
            .and_then(|sets| sets.clone())
    }

    fn take_error(&self) -> Result<(), String> {
        match self.context.error.lock() {
            Ok(mut error) => match error.take() {
                Some(message) => Err(message),
                None => Ok(()),
            },
            Err(_) => Ok(()),
        }
    }

    fn submit(
        &mut self,
        image: CVPixelBufferRef,
        pts: Duration,
        duration: u32,
    ) -> Result<(), String> {
        let meta = FrameMeta {
            pts: timeline_position(pts),
            duration,
        };
        let meta_pointer = Box::into_raw(Box::new(meta));
        let frame_properties = if self.frames_submitted == 0 {
            // Force an IDR first sample: a track whose first frame is not a
            // sync sample cannot be seeked to and some players refuse it.
            // SAFETY: the dictionary owns its framework-provided key.
            unsafe {
                dictionary(&[(
                    kVTEncodeFrameOptionKey_ForceKeyFrame,
                    kCFBooleanTrue as CFTypeRef,
                )])
            }
        } else {
            std::ptr::null()
        };
        // SAFETY: the session is live; `meta_pointer` is released in the
        // callback when the frame is encoded, or here when submission fails.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                image,
                CMTime::units(meta.pts),
                CMTime::units(u64::from(duration)),
                frame_properties,
                meta_pointer.cast(),
                std::ptr::null_mut(),
            )
        };
        if !frame_properties.is_null() {
            // SAFETY: created just above and only used by the call.
            unsafe { CFRelease(frame_properties.cast()) };
        }
        if status != 0 {
            // SAFETY: the callback will not run for a frame the session refused.
            unsafe {
                drop(Box::from_raw(meta_pointer));
                CVPixelBufferRelease(image);
            }
            return Err(format!("VideoToolbox 拒绝编码帧（状态 {status}）"));
        }
        self.frames_submitted += 1;
        // The session retains the buffer until the frame is encoded, so the
        // recording thread's reference is no longer needed.
        // SAFETY: this releases the reference taken by `CVPixelBufferCreate`.
        unsafe { CVPixelBufferRelease(image) };
        self.take_error()
    }

    fn create_image(&self, frame: &SourceFrame<'_>) -> Result<CVPixelBufferRef, String> {
        match frame {
            SourceFrame::Bgra { stride, bytes } => {
                let buffer = self.create_pixel_buffer(K_CV_PIXEL_FORMAT_32_BGRA)?;
                // SAFETY: the buffer was just created with the recording size,
                // and the source slice is validated against it below.
                unsafe {
                    if CVPixelBufferLockBaseAddress(buffer, 0) != 0 {
                        CVPixelBufferRelease(buffer);
                        return Err("无法锁定录制像素缓冲".into());
                    }
                    let destination_stride = CVPixelBufferGetBytesPerRow(buffer);
                    let destination = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
                    let row_bytes = self.settings.width as usize * 4;
                    let required = stride.saturating_mul(self.settings.height as usize);
                    if destination.is_null() || bytes.len() < required || *stride < row_bytes {
                        CVPixelBufferUnlockBaseAddress(buffer, 0);
                        CVPixelBufferRelease(buffer);
                        return Err("录制的 BGRA 帧与编码尺寸不匹配".into());
                    }
                    for row in 0..self.settings.height as usize {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr().add(row * stride),
                            destination.add(row * destination_stride),
                            row_bytes,
                        );
                    }
                    CVPixelBufferUnlockBaseAddress(buffer, 0);
                    attach_color_tags(buffer, YuvMatrix::Bt709);
                }
                Ok(buffer)
            }
            SourceFrame::Nv12 {
                y_stride,
                y,
                uv_stride,
                uv,
                range,
                matrix,
            } => {
                let format = match range {
                    YuvRange::Video => K_CV_PIXEL_FORMAT_420V,
                    YuvRange::Full => K_CV_PIXEL_FORMAT_420F,
                };
                let buffer = self.create_pixel_buffer(format)?;
                let width = self.settings.width as usize;
                let height = self.settings.height as usize;
                let uv_width = width.div_ceil(2) * 2;
                let uv_height = height.div_ceil(2);
                // SAFETY: plane accessors are used inside the lock, and every
                // copy is bounded by the plane geometry validated here.
                unsafe {
                    if CVPixelBufferLockBaseAddress(buffer, 0) != 0 {
                        CVPixelBufferRelease(buffer);
                        return Err("无法锁定录制像素缓冲".into());
                    }
                    let invalid = y.len() < y_stride.saturating_mul(height)
                        || *y_stride < width
                        || uv.len() < uv_stride.saturating_mul(uv_height)
                        || *uv_stride < uv_width;
                    let luma_destination =
                        CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
                    let chroma_destination =
                        CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
                    if invalid || luma_destination.is_null() || chroma_destination.is_null() {
                        CVPixelBufferUnlockBaseAddress(buffer, 0);
                        CVPixelBufferRelease(buffer);
                        return Err("录制的 NV12 帧与编码尺寸不匹配".into());
                    }
                    let luma_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
                    for row in 0..height {
                        std::ptr::copy_nonoverlapping(
                            y.as_ptr().add(row * y_stride),
                            luma_destination.add(row * luma_stride),
                            width,
                        );
                    }
                    let chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
                    for row in 0..uv_height {
                        std::ptr::copy_nonoverlapping(
                            uv.as_ptr().add(row * uv_stride),
                            chroma_destination.add(row * chroma_stride),
                            uv_width,
                        );
                    }
                    CVPixelBufferUnlockBaseAddress(buffer, 0);
                    attach_color_tags(buffer, *matrix);
                }
                Ok(buffer)
            }
        }
    }

    fn create_pixel_buffer(&self, format: u32) -> Result<CVPixelBufferRef, String> {
        // SAFETY: the attributes dictionary is created and released here; an
        // empty IOSurface dictionary asks CoreVideo for an IOSurface backing so
        // the system encoder can use the frame without another copy.
        unsafe {
            let surface = mutable_dictionary(0);
            let attributes = mutable_dictionary(0);
            if surface.is_null() || attributes.is_null() {
                if !surface.is_null() {
                    CFRelease(surface.cast());
                }
                if !attributes.is_null() {
                    CFRelease(attributes.cast());
                }
                return Err("无法创建录制像素缓冲属性".into());
            }
            CFDictionarySetValue(
                attributes,
                kCVPixelBufferIOSurfacePropertiesKey,
                surface.cast(),
            );
            let pixel_format = number_i32(format as i32);
            let width = number_i32(self.settings.width as i32);
            let height = number_i32(self.settings.height as i32);
            if !pixel_format.is_null() {
                CFDictionarySetValue(
                    attributes,
                    kCVPixelBufferPixelFormatTypeKey,
                    pixel_format.cast(),
                );
            }
            if !width.is_null() {
                CFDictionarySetValue(attributes, kCVPixelBufferWidthKey, width.cast());
            }
            if !height.is_null() {
                CFDictionarySetValue(attributes, kCVPixelBufferHeightKey, height.cast());
            }
            let mut buffer: CVPixelBufferRef = std::ptr::null();
            let status = CVPixelBufferCreate(
                std::ptr::null(),
                self.settings.width as usize,
                self.settings.height as usize,
                format,
                attributes,
                &mut buffer,
            );
            CFRelease(attributes.cast());
            CFRelease(surface.cast());
            if !pixel_format.is_null() {
                CFRelease(pixel_format.cast());
            }
            if !width.is_null() {
                CFRelease(width.cast());
            }
            if !height.is_null() {
                CFRelease(height.cast());
            }
            if status != 0 || buffer.is_null() {
                return Err(format!("无法创建录制像素缓冲（状态 {status}）"));
            }
            Ok(buffer)
        }
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // SAFETY: invalidating the session guarantees no further callbacks
        // before the callback context field is dropped, and the session
        // reference taken by `VTCompressionSessionCreate` is released once.
        unsafe {
            if !self.finished {
                let _ = VTCompressionSessionCompleteFrames(self.session, CMTime::invalid());
            }
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session.cast());
            if let Some(held) = self.held.take() {
                CVPixelBufferRelease(held.image);
            }
        }
    }
}

/// Describe the colour space of a frame the recorder is about to encode.
///
/// Without these tags the encoder would guess, and a player would then convert
/// the recorded stream with a different matrix than the viewer used on screen.
///
/// # Safety
/// `buffer` must be a live pixel buffer.
unsafe fn attach_color_tags(buffer: CVPixelBufferRef, matrix: YuvMatrix) {
    // SAFETY: framework-provided keys and values, live pixel buffer.
    unsafe {
        let matrix = match matrix {
            YuvMatrix::Bt601 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            YuvMatrix::Bt709 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            YuvMatrix::Bt2020 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
        };
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferYCbCrMatrixKey,
            matrix,
            K_CV_ATTACHMENT_MODE_PROPAGATE,
        );
        // These describe the presenter, not the stream: the viewer shows the
        // decoded planes' numbers as sRGB, so the recording is tagged sRGB
        // (ITU-R BT.709 primaries are sRGB's primaries, and the transfer is the
        // sRGB curve the presenter's numbers are encoded with).
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
            K_CV_ATTACHMENT_MODE_PROPAGATE,
        );
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_sRGB,
            K_CV_ATTACHMENT_MODE_PROPAGATE,
        );
    }
}

/// Create an empty mutable dictionary with the standard CF collection callbacks.
///
/// # Safety
/// The returned dictionary owns a reference the caller must release.
unsafe fn mutable_dictionary(capacity: CFIndex) -> CFMutableDictionaryRef {
    // SAFETY: the callback tables are framework-provided constants.
    unsafe {
        CFDictionaryCreateMutable(
            std::ptr::null(),
            capacity,
            std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        )
    }
}

/// Create a mutable CoreFoundation dictionary from key/value pairs.
///
/// # Safety
/// The returned dictionary owns a reference the caller must release.
unsafe fn dictionary(pairs: &[(CFStringRef, CFTypeRef)]) -> CFDictionaryRef {
    // SAFETY: keys and values are framework-provided or created by the caller.
    unsafe {
        let dictionary = CFDictionaryCreateMutable(
            std::ptr::null(),
            pairs.len() as CFIndex,
            std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        );
        for (key, value) in pairs {
            CFDictionarySetValue(dictionary, *key, *value);
        }
        dictionary
    }
}

/// Create a CFNumber holding a 32-bit integer.
///
/// # Safety
/// The returned number owns a reference the caller must release.
unsafe fn number_i32(value: i32) -> CFNumberRef {
    // SAFETY: the pointer is valid for the duration of the call.
    unsafe {
        CFNumberCreate(
            std::ptr::null(),
            K_CF_NUMBER_SINT32,
            (&value as *const i32).cast(),
        )
    }
}

/// Create a CFNumber holding a double.
///
/// # Safety
/// The returned number owns a reference the caller must release.
unsafe fn number_f64(value: f64) -> CFNumberRef {
    // SAFETY: the pointer is valid for the duration of the call.
    unsafe {
        CFNumberCreate(
            std::ptr::null(),
            K_CF_NUMBER_FLOAT64,
            (&value as *const f64).cast(),
        )
    }
}
