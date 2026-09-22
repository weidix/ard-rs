//! macOS VideoProcessing decoder for the AVC media stream.
//!
//! Consumes access units (H.264 or HEVC NAL units) from `ard_rs::media_stream`, builds
//! a `CMVideoFormatDescription` from the parameter sets, decodes with the
//! private VCP session used by AVConference, and returns the decoder's native
//! NV12 planes for direct GPU presentation.

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex, OnceLock};

use ard_rs::media_stream::{AccessUnit, MediaStreamCodec};

use super::{DecodedOutput, DecodedSlice, YuvMatrix, YuvPrimaries, YuvRange};

type OSStatus = i32;
type CFIndex = isize;
type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNumberRef = *const c_void;
type CFStringRef = *const c_void;
type CFBooleanRef = *const c_void;
type CMVideoFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CVPixelBufferRef = *const c_void;
type VTDecompressionSessionRef = *const c_void;
type VTDecodeFrameFlags = u32;
type VTDecodeInfoFlags = u32;
type CFTypeID = usize;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

#[allow(clippy::duplicated_attributes)]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: CFBooleanRef;
    fn CFGetTypeID(cf: *const c_void) -> CFTypeID;
    fn CFEqual(cf1: *const c_void, cf2: *const c_void) -> u8;
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetValueAtIndex(the_array: CFArrayRef, index: CFIndex) -> *const c_void;
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFDictionarySetValue(the_dict: CFDictionaryRef, key: *const c_void, value: *const c_void);
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        extensions: CFDictionaryRef,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetDimensions(
        video_desc: CMVideoFormatDescriptionRef,
    ) -> CMVideoDimensions;
    fn CMVideoFormatDescriptionGetPresentationDimensions(
        video_desc: CMVideoFormatDescriptionRef,
        use_pixel_aspect_ratio: u8,
        use_clean_aperture: u8,
    ) -> CGSize;
    fn CMVideoFormatDescriptionGetCleanAperture(
        video_desc: CMVideoFormatDescriptionRef,
        origin_is_at_top_left: u8,
    ) -> CGRect;
    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        target_buffer: CMBlockBufferRef,
        offset_into_data: usize,
        data_length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMVideoFormatDescriptionRef,
        sample_count: isize,
        sample_timing_entry_count: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        sample_size_entry_count: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
    fn CMSampleBufferGetSampleAttachmentsArray(
        sample_buffer: CMSampleBufferRef,
        create_if_necessary: bool,
    ) -> CFArrayRef;
    static kCMSampleAttachmentKey_DisplayImmediately: CFStringRef;
    static kCMSampleAttachmentKey_NotSync: CFStringRef;

    fn CVPixelBufferGetTypeID() -> CFTypeID;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> u32;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetPlaneCount(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut c_void;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u32) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: CVPixelBufferRef,
        unlock_flags: u32,
    ) -> OSStatus;
    fn CVBufferGetAttachment(
        buffer: *const c_void,
        key: CFStringRef,
        attachment_mode: *mut u32,
    ) -> *const c_void;
    static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_2020: CFStringRef;
    static kCVImageBufferColorPrimariesKey: CFStringRef;
    static kCVImageBufferColorPrimaries_P3_D65: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_2020: CFStringRef;

    fn dlopen(path: *const u8, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const u8) -> *mut c_void;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMVideoDimensions {
    width: c_int,
    height: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[allow(non_upper_case_globals)]
const kCFNumberSInt32Type: i32 = 3;
#[allow(non_upper_case_globals)]
const kCFStringEncodingUTF8: u32 = 0x08000100;
/// The decoder's destination format, named by the same typed set the offer's
/// `pixelFormats` menu is built from: this client *claims* every format in that
/// menu but currently asks VideoProcessing for 8-bit 4:2:0 here, which is where
/// the stream's chroma is dropped.
const PIXEL_FORMAT_NV12_VIDEO_RANGE: u32 =
    ard_rs::media_stream::MediaPixelFormat::Nv12Video.fourcc();
const PIXEL_FORMAT_NV12_FULL_RANGE: u32 = ard_rs::media_stream::MediaPixelFormat::Nv12Full.fourcc();
#[allow(non_upper_case_globals)]
const kCVPixelBufferLock_ReadOnly: u32 = 1;
const VCP_FRAMEWORK: &[u8] =
    b"/System/Library/PrivateFrameworks/VideoProcessing.framework/Versions/A/VideoProcessing\0";
const RTLD_LAZY: c_int = 0x1;
const RTLD_LOCAL: c_int = 0x4;

type VcpCreate = unsafe extern "C" fn(
    CFAllocatorRef,
    CMVideoFormatDescriptionRef,
    CFDictionaryRef,
    CFDictionaryRef,
    *const VTDecompressionOutputCallbackRecord,
    *mut VTDecompressionSessionRef,
) -> OSStatus;
type VcpDecode = unsafe extern "C" fn(
    VTDecompressionSessionRef,
    CMSampleBufferRef,
    VTDecodeFrameFlags,
    *mut c_void,
    *mut VTDecodeInfoFlags,
) -> OSStatus;
type VcpWait = unsafe extern "C" fn(VTDecompressionSessionRef) -> OSStatus;
type VcpInvalidate = unsafe extern "C" fn(VTDecompressionSessionRef);

struct VcpApi {
    _handle: usize,
    create: VcpCreate,
    decode: VcpDecode,
    wait: VcpWait,
    invalidate: VcpInvalidate,
}

fn vcp_api() -> Option<&'static VcpApi> {
    static API: OnceLock<Option<VcpApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let handle = dlopen(VCP_FRAMEWORK.as_ptr(), RTLD_LAZY | RTLD_LOCAL);
        if handle.is_null() {
            return None;
        }
        unsafe fn load<T: Copy>(handle: *mut c_void, name: &[u8]) -> Option<T> {
            let pointer = dlsym(handle, name.as_ptr());
            (!pointer.is_null()).then(|| std::mem::transmute_copy(&pointer))
        }
        Some(VcpApi {
            _handle: handle as usize,
            create: load(handle, b"VCPDecompressionSessionCreate\0")?,
            decode: load(handle, b"VCPDecompressionSessionDecodeFrame\0")?,
            wait: load(
                handle,
                b"VCPDecompressionSessionWaitForAsynchronousFrames\0",
            )?,
            invalidate: load(handle, b"VCPDecompressionSessionInvalidate\0")?,
        })
    })
    .as_ref()
}

#[derive(Default)]
struct CallbackState {
    outputs: Mutex<VecDeque<DecodedOutput>>,
}

struct SourceFrameContext {
    stream_index: usize,
    timestamp: u32,
    submission: u64,
    encoded_bytes: usize,
    visible_rect: PixelRect,
    output_state: Arc<CallbackState>,
}

#[allow(clippy::missing_safety_doc)]
#[allow(private_interfaces)]
pub unsafe extern "C" fn decompression_output_callback(
    _decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    image_buffer: CVPixelBufferRef,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if source_frame_ref_con.is_null() {
        if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
            eprintln!("VCP callback without source-frame context");
        }
        return;
    }
    // Every accepted decode owns one unique Box. The callback consumes it
    // even when decoding fails or intentionally produces no image.
    let context = Box::from_raw(source_frame_ref_con as *mut SourceFrameContext);
    let (frame, conversion_error) = if status != 0 || image_buffer.is_null() {
        (None, None)
    } else if CFGetTypeID(image_buffer) != CVPixelBufferGetTypeID() {
        (
            None,
            Some("VCP returned an object that is not a CVPixelBuffer".to_owned()),
        )
    } else {
        match pixel_buffer_to_nv12(image_buffer, context.visible_rect) {
            Ok(frame) => (Some(frame), None),
            Err(error) => (None, Some(error)),
        }
    };
    if std::env::var_os("ARD_MEDIA_TRACE").is_some() && frame.is_none() {
        eprintln!(
            "VCP callback without conventional image: stream={} timestamp={} submission={} status={status} info_flags={info_flags:#x} null_image={}",
            context.stream_index,
            context.timestamp,
            context.submission,
            image_buffer.is_null(),
        );
    }
    if let Ok(mut outputs) = context.output_state.outputs.lock() {
        outputs.push_back(DecodedOutput {
            stream_index: context.stream_index,
            timestamp: context.timestamp,
            submission: context.submission,
            encoded_bytes: context.encoded_bytes,
            status,
            info_flags,
            conversion_error,
            frame,
        });
    }
}

type VTDecompressionOutputCallback = unsafe extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: VTDecodeInfoFlags,
    image_buffer: CVPixelBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: VTDecompressionOutputCallback,
    refcon: *mut c_void,
}

struct NativeDecoder {
    format_description: CMVideoFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    api: &'static VcpApi,
    output_state: Arc<CallbackState>,
    visible_rect: PixelRect,
    /// Destination image buffer attributes handed to
    /// `VCPDecompressionSessionCreate`.
    ///
    /// The private entry point publishes no retention contract. Keeping the
    /// dictionary alive for the session's lifetime is what a client may rely on
    /// with the public `VTDecompressionSessionCreate`, and it costs nothing
    /// here, so the session can never read a freed dictionary.
    _destination_attributes: DestinationAttributes,
}

impl NativeDecoder {
    fn wait(&self) {
        unsafe {
            let _ = (self.api.wait)(self.session);
        }
    }

    fn take_outputs(&self) -> Vec<DecodedOutput> {
        self.output_state
            .outputs
            .lock()
            .map(|mut outputs| outputs.drain(..).collect())
            .unwrap_or_default()
    }
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.api.wait)(self.session);
            (self.api.invalidate)(self.session);
            CFRelease(self.session as *const c_void);
            CFRelease(self.format_description as *const c_void);
        }
    }
}

/// Safe wrapper around an AVConference-style VCP decompression session.
pub struct VideoToolboxDecoder {
    codec: MediaStreamCodec,
    native: Option<NativeDecoder>,
    parameter_sets: Vec<Vec<u8>>,
    next_submission: u64,
    needs_sync: bool,
    errors: VecDeque<String>,
}

impl VideoToolboxDecoder {
    pub fn new(codec: MediaStreamCodec) -> Self {
        Self {
            codec,
            native: None,
            parameter_sets: Vec::new(),
            next_submission: 0,
            needs_sync: false,
            errors: VecDeque::new(),
        }
    }

    /// Resolution configured on the active VideoToolbox session. When the
    /// server applies an explicitly requested display mode and sends updated
    /// parameter sets, the decoder recreates the immutable native session.
    pub fn configured_dimensions(&self) -> Option<(u32, u32)> {
        self.native.as_ref().map(|native| {
            (
                native.visible_rect.width as u32,
                native.visible_rect.height as u32,
            )
        })
    }

    /// Stop decoding predictive access units after RTP loss. The next sync
    /// unit recreates the VCP session so no damaged reference picture can be
    /// reused by either H.264 or HEVC.
    pub(crate) fn require_sync(&mut self) {
        if !self.needs_sync {
            // Quiesce callbacks from the old prediction chain now. Letting an
            // already queued P/B frame arrive after the loss boundary would
            // repopulate the compositor with stale reference content.
            let _ = self.finish_session();
        }
        self.needs_sync = true;
    }

    pub(crate) fn take_errors(&mut self) -> Vec<String> {
        self.errors.drain(..).collect()
    }

    /// Submit one AU to the shared interleaved VCP session and return every
    /// callback outcome that has arrived so far. An outcome may belong to an
    /// earlier submission and may contain no pixel buffer.
    pub(crate) fn decode(&mut self, stream_index: usize, unit: &AccessUnit) -> Vec<DecodedOutput> {
        let mut outputs = self.take_outputs();
        let is_sync = self.is_sync_unit(unit);
        if self.needs_sync && !is_sync {
            return outputs;
        }
        if self.needs_sync {
            outputs.extend(self.finish_session());
            self.needs_sync = false;
        }
        let parameter_sets = self.parameter_sets_for(unit);
        if !parameter_sets.is_empty() {
            let changed = self.merge_parameter_sets(parameter_sets);
            if changed {
                // Format descriptions are immutable. Finish the old async
                // session before replacing it so no callback can outlive its
                // output state or be attributed to the new session.
                outputs.extend(self.finish_session());
            }
        }
        if self.native.is_none() {
            match self.create_session() {
                Ok(decoder) => self.native = Some(decoder),
                Err(error) => {
                    self.errors.push_back(error);
                    return outputs;
                }
            }
        }
        let Some(native) = &self.native else {
            return outputs;
        };
        let submission = self.next_submission;
        self.next_submission = self.next_submission.wrapping_add(1);
        let avcc = unit.to_avcc();
        let context = SourceFrameContext {
            stream_index,
            timestamp: unit.timestamp,
            submission,
            encoded_bytes: unit.avcc_len(),
            visible_rect: native.visible_rect,
            output_state: Arc::clone(&native.output_state),
        };
        match unsafe { decode_access_unit(native, &avcc, unit.timestamp, is_sync, context) } {
            Ok(()) => {}
            Err(error) => {
                let error = format!(
                    "VCP decode submission failed: stream={stream_index} timestamp={} submission={submission}: {error}",
                    unit.timestamp,
                );
                if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
                    eprintln!("{error}");
                }
                self.errors.push_back(error);
            }
        }
        outputs.extend(self.take_outputs());
        outputs
    }

    /// Finish the asynchronous submissions belonging to one RTP sampling
    /// instant. The caller groups Apple's four SSRC bands by their shared RTP
    /// timestamp; VideoProcessing's `CheckIfLastSubFrame` describes codec
    /// subframes and is not the desktop-frame boundary for these streams.
    pub(crate) fn finish_frame(&mut self) -> Vec<DecodedOutput> {
        let outputs = if let Some(native) = &self.native {
            native.wait();
            native.take_outputs()
        } else {
            Vec::new()
        };
        self.order_outputs(outputs)
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<DecodedOutput> {
        let outputs = self
            .native
            .as_ref()
            .map(NativeDecoder::take_outputs)
            .unwrap_or_default();
        self.order_outputs(outputs)
    }

    fn order_outputs(&mut self, mut outputs: Vec<DecodedOutput>) -> Vec<DecodedOutput> {
        // VCP may intentionally omit callbacks for non-displayable subframes,
        // so submission numbers are not a contiguous sequence. Its callback
        // queue is serialized, but sort each drained batch to make callback
        // races deterministic without waiting for callbacks that will never
        // exist.
        outputs.sort_by_key(|output| output.submission);
        outputs
    }

    pub(crate) fn flush(&mut self) -> Vec<DecodedOutput> {
        let outputs = if let Some(native) = &self.native {
            native.wait();
            native.take_outputs()
        } else {
            Vec::new()
        };
        self.order_outputs(outputs)
    }

    fn finish_session(&mut self) -> Vec<DecodedOutput> {
        let Some(native) = self.native.take() else {
            return Vec::new();
        };
        native.wait();
        let outputs = native.take_outputs();
        let outputs = self.order_outputs(outputs);
        drop(native);
        outputs
    }

    fn parameter_sets_for(&self, unit: &AccessUnit) -> Vec<Vec<u8>> {
        match self.codec {
            MediaStreamCodec::H264 => unit
                .nal_units
                .iter()
                .filter(|nal| matches!(nal.first().map(|b| b & 0x1f), Some(7 | 8)))
                .cloned()
                .collect(),
            MediaStreamCodec::Hevc => unit
                .nal_units
                .iter()
                .filter(|nal| matches!(nal.first().map(|b| (b >> 1) & 0x3f), Some(32..=34)))
                .cloned()
                .collect(),
        }
    }

    fn is_sync_unit(&self, unit: &AccessUnit) -> bool {
        unit.nal_units.iter().any(|nal| match self.codec {
            MediaStreamCodec::H264 => matches!(nal.first().map(|byte| byte & 0x1f), Some(5)),
            MediaStreamCodec::Hevc => {
                matches!(nal.first().map(|byte| (byte >> 1) & 0x3f), Some(16..=23))
            }
        })
    }

    fn merge_parameter_sets(&mut self, sets: Vec<Vec<u8>>) -> bool {
        let mut changed = false;
        for set in sets {
            let kind = match self.codec {
                MediaStreamCodec::H264 => set.first().map(|byte| byte & 0x1f),
                MediaStreamCodec::Hevc => set.first().map(|byte| (byte >> 1) & 0x3f),
            };
            let Some(kind) = kind else {
                continue;
            };
            if let Some(current) = self.parameter_sets.iter_mut().find(|current| {
                let current_kind = match self.codec {
                    MediaStreamCodec::H264 => current.first().map(|byte| byte & 0x1f),
                    MediaStreamCodec::Hevc => current.first().map(|byte| (byte >> 1) & 0x3f),
                };
                current_kind == Some(kind)
            }) {
                if *current != set {
                    *current = set;
                    changed = true;
                }
            } else {
                self.parameter_sets.push(set);
                changed = true;
            }
        }
        changed
    }

    fn create_session(&self) -> Result<NativeDecoder, String> {
        if self.parameter_sets.is_empty() {
            return Err("VCP session cannot start before codec parameter sets arrive".into());
        }
        let pointers: Vec<*const u8> = self.parameter_sets.iter().map(|set| set.as_ptr()).collect();
        let sizes: Vec<usize> = self.parameter_sets.iter().map(Vec::len).collect();
        let mut format_description: CMVideoFormatDescriptionRef = std::ptr::null();
        let status = unsafe {
            match self.codec {
                MediaStreamCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    std::ptr::null(),
                    pointers.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut format_description,
                ),
                MediaStreamCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    std::ptr::null(),
                    pointers.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    std::ptr::null(),
                    &mut format_description,
                ),
            }
        };
        if status != 0 || format_description.is_null() {
            return Err(format!(
                "CoreMedia format description creation failed with status {status}"
            ));
        }
        let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(format_description) };
        let presentation_dimensions =
            unsafe { CMVideoFormatDescriptionGetPresentationDimensions(format_description, 0, 1) };
        let clean_aperture =
            unsafe { CMVideoFormatDescriptionGetCleanAperture(format_description, 1) };
        if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
            eprintln!(
                "VCP format: encoded={}x{} presentation={}x{} clean=({}, {}) {}x{}",
                dimensions.width,
                dimensions.height,
                presentation_dimensions.width,
                presentation_dimensions.height,
                clean_aperture.origin.x,
                clean_aperture.origin.y,
                clean_aperture.size.width,
                clean_aperture.size.height,
            );
        }
        if dimensions.width <= 0
            || dimensions.height <= 0
            || dimensions.width > 16_384
            || dimensions.height > 16_384
        {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err("codec parameter sets contain invalid frame dimensions".into());
        }
        let visible_rect = match pixel_rect_from_clean_aperture(
            dimensions.width as usize,
            dimensions.height as usize,
            clean_aperture,
        ) {
            Ok(rect) => rect,
            Err(error) => {
                unsafe { CFRelease(format_description as *const c_void) };
                return Err(error);
            }
        };

        let api = vcp_api().ok_or_else(|| {
            "VideoProcessing decompression API is unavailable on this macOS build".to_owned()
        })?;
        let output_state = Arc::new(CallbackState::default());
        let refcon = Arc::as_ptr(&output_state) as *mut c_void;
        let callback: VTDecompressionOutputCallback = decompression_output_callback;
        let record = VTDecompressionOutputCallbackRecord { callback, refcon };
        let Some(destination_attributes) =
            DestinationAttributes::new(dimensions.width, dimensions.height)
        else {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err("failed to allocate VCP NV12 destination attributes".into());
        };
        let mut session: VTDecompressionSessionRef = std::ptr::null();
        // Unlike the public VT entry point, VCP expects a real dictionary and
        // dereferences it even when no private decoder properties are needed.
        let decoder_specification = unsafe {
            CFDictionaryCreateMutable(std::ptr::null(), 0, std::ptr::null(), std::ptr::null())
        };
        if decoder_specification.is_null() {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err("failed to allocate VCP decoder specification".into());
        }
        let status = unsafe {
            (api.create)(
                std::ptr::null(),
                format_description,
                decoder_specification,
                destination_attributes.dictionary,
                &record,
                &mut session,
            )
        };
        unsafe { CFRelease(decoder_specification) };
        if status != 0 || session.is_null() {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err(format!("VCP session creation failed with status {status}"));
        }
        Ok(NativeDecoder {
            format_description,
            session,
            api,
            output_state,
            visible_rect,
            _destination_attributes: destination_attributes,
        })
    }
}

struct DestinationAttributes {
    dictionary: CFDictionaryRef,
    keys: Vec<CFStringRef>,
    values: Vec<CFNumberRef>,
}

impl DestinationAttributes {
    fn new(width: c_int, height: c_int) -> Option<Self> {
        let dictionary = unsafe {
            CFDictionaryCreateMutable(std::ptr::null(), 3, std::ptr::null(), std::ptr::null())
        };
        if dictionary.is_null() {
            return None;
        }
        let attributes = [
            // Apple's media-stream encoders emit full-range (0..255) planes:
            // every real-device elementary stream captured so far declares
            // `color_range=pc`. Asking VideoProcessing for a video-range buffer
            // makes it compress that range into 16..235, which collapses 256
            // levels into 220 and leaves every decoded frame a couple of levels
            // away from the bitstream (measured: +2 on luma everywhere). Request
            // the plane range the encoder actually produced and let the
            // conversion matrix carry it: `pixel_buffer_to_nv12` reports
            // `YuvRange::Full` for `420f`, and the renderer passes full-range
            // values through unchanged.
            //
            // This is a correctness fix for the decoded planes only. It is NOT
            // the cause of the H.265 banding, which is still open; do not treat
            // it as that fix.
            (
                c"PixelFormatType".as_ptr(),
                PIXEL_FORMAT_NV12_FULL_RANGE as c_int,
            ),
            (c"Width".as_ptr(), width),
            (c"Height".as_ptr(), height),
        ];
        let mut keys = Vec::with_capacity(attributes.len());
        let mut values = Vec::with_capacity(attributes.len());
        for (name, raw) in attributes {
            let key = unsafe {
                CFStringCreateWithCString(
                    std::ptr::null(),
                    name as *const u8,
                    kCFStringEncodingUTF8,
                )
            };
            let value = unsafe {
                CFNumberCreate(
                    std::ptr::null(),
                    kCFNumberSInt32Type,
                    &raw as *const c_int as *const c_void,
                )
            };
            if key.is_null() || value.is_null() {
                if !key.is_null() {
                    unsafe { CFRelease(key) };
                }
                if !value.is_null() {
                    unsafe { CFRelease(value) };
                }
                for key in keys {
                    unsafe { CFRelease(key) };
                }
                for value in values {
                    unsafe { CFRelease(value) };
                }
                unsafe { CFRelease(dictionary) };
                return None;
            }
            unsafe {
                CFDictionarySetValue(dictionary, key as *const c_void, value as *const c_void);
            }
            keys.push(key);
            values.push(value);
        }
        Some(Self {
            dictionary,
            keys,
            values,
        })
    }
}

impl Drop for DestinationAttributes {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.dictionary);
            for key in &self.keys {
                CFRelease(*key);
            }
            for value in &self.values {
                CFRelease(*value);
            }
        }
    }
}

unsafe fn decode_access_unit(
    native: &NativeDecoder,
    avcc: &[u8],
    rtp_timestamp: u32,
    is_sync: bool,
    context: SourceFrameContext,
) -> Result<(), String> {
    let mut block_buffer: CMBlockBufferRef = std::ptr::null();
    let status = CMBlockBufferCreateWithMemoryBlock(
        std::ptr::null(),
        std::ptr::null_mut(),
        avcc.len(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        avcc.len(),
        0,
        &mut block_buffer,
    );
    if status != 0 || block_buffer.is_null() {
        return Err(format!(
            "CMBlockBuffer allocation failed with status {status}"
        ));
    }
    let status =
        CMBlockBufferReplaceDataBytes(avcc.as_ptr() as *const c_void, block_buffer, 0, avcc.len());
    if status != 0 {
        CFRelease(block_buffer);
        return Err(format!("CMBlockBuffer copy failed with status {status}"));
    }
    let mut sample_buffer: CMSampleBufferRef = std::ptr::null();
    let sample_size = avcc.len();
    let timing = CMSampleTimingInfo {
        duration: CMTime {
            value: 1,
            timescale: 90_000,
            flags: 1,
            epoch: 0,
        },
        presentation_time_stamp: CMTime {
            value: i64::from(rtp_timestamp),
            timescale: 90_000,
            flags: 1, // kCMTimeFlags_Valid
            epoch: 0,
        },
        decode_time_stamp: CMTime {
            value: i64::from(rtp_timestamp),
            timescale: 90_000,
            flags: 1,
            epoch: 0,
        },
    };
    let status = CMSampleBufferCreateReady(
        std::ptr::null(),
        block_buffer,
        native.format_description,
        1,
        1,
        &timing,
        1,
        &sample_size,
        &mut sample_buffer,
    );
    if status != 0 || sample_buffer.is_null() {
        CFRelease(block_buffer);
        return Err(format!(
            "CMSampleBuffer creation failed with status {status}"
        ));
    }
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample_buffer, true);
    if !attachments.is_null() {
        let attachment = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
        if !attachment.is_null() {
            CFDictionarySetValue(
                attachment,
                kCMSampleAttachmentKey_DisplayImmediately as *const c_void,
                kCFBooleanTrue as *const c_void,
            );
            if !is_sync {
                CFDictionarySetValue(
                    attachment,
                    kCMSampleAttachmentKey_NotSync as *const c_void,
                    kCFBooleanTrue as *const c_void,
                );
            }
        }
    }
    let context = Box::into_raw(Box::new(context));
    let mut info_flags: VTDecodeInfoFlags = 0;
    let status = (native.api.decode)(
        native.session,
        sample_buffer,
        1, // kVTDecodeFrame_EnableAsynchronousDecompression
        context as *mut c_void,
        &mut info_flags,
    );
    CFRelease(sample_buffer);
    CFRelease(block_buffer);
    if status != 0 {
        // VCP does not issue a callback for a rejected submission, so reclaim
        // the unique source-frame context here.
        drop(Box::from_raw(context));
        if std::env::var_os("ARD_MEDIA_TRACE").is_some() {
            eprintln!("VCP decode failed: status={status} info_flags={info_flags:#x}");
        }
        return Err(format!(
            "VCP rejected the sample with status {status} and flags {info_flags:#x}"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn pixel_rect_from_clean_aperture(
    encoded_width: usize,
    encoded_height: usize,
    aperture: CGRect,
) -> Result<PixelRect, String> {
    fn exact_pixel(value: f64, field: &str) -> Result<usize, String> {
        if !value.is_finite() || value < 0.0 || value.fract().abs() > 1.0e-6 {
            return Err(format!(
                "codec clean aperture has unsupported {field}={value}"
            ));
        }
        usize::try_from(value as u64)
            .map_err(|_| format!("codec clean aperture {field} is too large"))
    }

    let rect = PixelRect {
        x: exact_pixel(aperture.origin.x, "x")?,
        y: exact_pixel(aperture.origin.y, "y")?,
        width: exact_pixel(aperture.size.width, "width")?,
        height: exact_pixel(aperture.size.height, "height")?,
    };
    if rect.width == 0 || rect.height == 0 {
        return Err("codec clean aperture is empty".to_owned());
    }
    if !rect.x.is_multiple_of(2) || !rect.y.is_multiple_of(2) {
        return Err(format!(
            "NV12 clean aperture origin must be chroma-aligned: ({}, {})",
            rect.x, rect.y
        ));
    }
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or_else(|| "codec clean aperture width overflow".to_owned())?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or_else(|| "codec clean aperture height overflow".to_owned())?;
    if right > encoded_width || bottom > encoded_height {
        return Err(format!(
            "codec clean aperture exceeds encoded frame: aperture={}x{}+{},{} encoded={}x{}",
            rect.width, rect.height, rect.x, rect.y, encoded_width, encoded_height
        ));
    }
    Ok(rect)
}

unsafe fn pixel_buffer_to_nv12(
    buffer: CVPixelBufferRef,
    visible_rect: PixelRect,
) -> Result<DecodedSlice, String> {
    let buffer_width = CVPixelBufferGetWidth(buffer);
    let buffer_height = CVPixelBufferGetHeight(buffer);
    if buffer_width == 0 || buffer_height == 0 || buffer_width > 16_384 || buffer_height > 16_384 {
        return Err(format!(
            "VCP returned invalid CVPixelBuffer dimensions {buffer_width}x{buffer_height}"
        ));
    }
    let visible_right = visible_rect
        .x
        .checked_add(visible_rect.width)
        .ok_or_else(|| "VCP visible width overflow".to_owned())?;
    let visible_bottom = visible_rect
        .y
        .checked_add(visible_rect.height)
        .ok_or_else(|| "VCP visible height overflow".to_owned())?;
    if visible_right > buffer_width || visible_bottom > buffer_height {
        return Err(format!(
            "VCP output is smaller than the codec clean aperture: buffer={buffer_width}x{buffer_height} aperture={}x{}+{},{}",
            visible_rect.width, visible_rect.height, visible_rect.x, visible_rect.y
        ));
    }
    let pixel_format = CVPixelBufferGetPixelFormatType(buffer);
    let range = match pixel_format {
        PIXEL_FORMAT_NV12_VIDEO_RANGE => YuvRange::Video,
        PIXEL_FORMAT_NV12_FULL_RANGE => YuvRange::Full,
        _ => {
            return Err(format!(
                "VCP returned unsupported pixel format {pixel_format:#010x}; expected native NV12"
            ));
        }
    };
    let plane_count = CVPixelBufferGetPlaneCount(buffer);
    if plane_count != 2 {
        return Err(format!(
            "VCP returned NV12 buffer with {plane_count} planes; expected 2"
        ));
    }
    let y_width = CVPixelBufferGetWidthOfPlane(buffer, 0);
    let y_height = CVPixelBufferGetHeightOfPlane(buffer, 0);
    let uv_width = CVPixelBufferGetWidthOfPlane(buffer, 1);
    let uv_height = CVPixelBufferGetHeightOfPlane(buffer, 1);
    if y_width != buffer_width
        || y_height != buffer_height
        || uv_width != buffer_width.div_ceil(2)
        || uv_height != buffer_height.div_ceil(2)
    {
        return Err(format!(
            "VCP returned inconsistent NV12 plane geometry: buffer={buffer_width}x{buffer_height}, y={y_width}x{y_height}, uv={uv_width}x{uv_height}"
        ));
    }
    let visible_uv_width = visible_rect.width.div_ceil(2);
    let visible_uv_height = visible_rect.height.div_ceil(2);
    let visible_uv_row_bytes = visible_uv_width
        .checked_mul(2)
        .ok_or_else(|| "VCP NV12 chroma row length overflow".to_owned())?;
    let y_len = visible_rect
        .width
        .checked_mul(visible_rect.height)
        .ok_or_else(|| "VCP NV12 luma plane length overflow".to_owned())?;
    let uv_len = visible_uv_row_bytes
        .checked_mul(visible_uv_height)
        .ok_or_else(|| "VCP NV12 chroma plane length overflow".to_owned())?;
    let _lock = PixelBufferReadLock::new(buffer)?;
    let y_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
    let uv_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 1);
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
    let mut y_plane = Vec::with_capacity(y_len);
    let mut uv_plane = Vec::with_capacity(uv_len);
    if !y_base.is_null()
        && !uv_base.is_null()
        && y_stride >= buffer_width
        && uv_stride >= uv_width.saturating_mul(2)
    {
        for row in 0..visible_rect.height {
            let source =
                (y_base as *const u8).add((visible_rect.y + row) * y_stride + visible_rect.x);
            y_plane.extend_from_slice(std::slice::from_raw_parts(source, visible_rect.width));
        }
        for row in 0..visible_uv_height {
            let source =
                (uv_base as *const u8).add((visible_rect.y / 2 + row) * uv_stride + visible_rect.x);
            uv_plane.extend_from_slice(std::slice::from_raw_parts(source, visible_uv_row_bytes));
        }
    }
    if y_plane.len() != y_len || uv_plane.len() != uv_len {
        return Err(format!(
            "VCP returned inaccessible NV12 plane storage: y={}/{y_len}, uv={}/{uv_len}",
            y_plane.len(),
            uv_plane.len()
        ));
    }
    let matrix = ycbcr_matrix(buffer, visible_rect.width, visible_rect.height);
    let primaries = ycbcr_primaries(buffer);
    Ok(DecodedSlice {
        width: visible_rect.width as u32,
        height: visible_rect.height as u32,
        y_plane,
        uv_plane,
        range,
        matrix,
        primaries,
    })
}

struct PixelBufferReadLock(CVPixelBufferRef);

impl PixelBufferReadLock {
    unsafe fn new(buffer: CVPixelBufferRef) -> Result<Self, String> {
        let status = CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
        if status == 0 {
            Ok(Self(buffer))
        } else {
            Err(format!(
                "CVPixelBuffer read lock failed with status {status}"
            ))
        }
    }
}

impl Drop for PixelBufferReadLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(self.0, kCVPixelBufferLock_ReadOnly);
        }
    }
}

unsafe fn ycbcr_matrix(buffer: CVPixelBufferRef, width: usize, height: usize) -> YuvMatrix {
    let value = CVBufferGetAttachment(buffer, kCVImageBufferYCbCrMatrixKey, std::ptr::null_mut());
    if !value.is_null() {
        if CFEqual(value, kCVImageBufferYCbCrMatrix_ITU_R_2020 as *const c_void) != 0 {
            return YuvMatrix::Bt2020;
        }
        if CFEqual(
            value,
            kCVImageBufferYCbCrMatrix_ITU_R_601_4 as *const c_void,
        ) != 0
        {
            return YuvMatrix::Bt601;
        }
        if CFEqual(
            value,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2 as *const c_void,
        ) != 0
        {
            return YuvMatrix::Bt709;
        }
    }
    // CoreVideo may omit this attachment for older AVC profiles. ITU-T's
    // conventional SD/HD split is deterministic and avoids misclassifying
    // 480/576-line sources as BT.709.
    if width <= 1024 && height <= 576 {
        YuvMatrix::Bt601
    } else {
        YuvMatrix::Bt709
    }
}

/// Colour primaries the decoder tagged the picture with.
///
/// The HEVC VUI of Apple's media stream carries a full colour description and
/// VideoProcessing copies it onto the output buffer. Only the primary set is
/// consumed here: the transfer function of a Mac screen is sRGB, which the
/// presentation shader already linearises, while the primaries are what the
/// shader has to convert away from (Display P3 planes presented as sRGB move
/// the whole picture). An unknown or absent tag keeps the historical
/// assumption that the planes are already sRGB.
unsafe fn ycbcr_primaries(buffer: CVPixelBufferRef) -> YuvPrimaries {
    let value = CVBufferGetAttachment(
        buffer,
        kCVImageBufferColorPrimariesKey,
        std::ptr::null_mut(),
    );
    if !value.is_null() {
        if CFEqual(value, kCVImageBufferColorPrimaries_P3_D65 as *const c_void) != 0 {
            return YuvPrimaries::P3D65;
        }
        if CFEqual(
            value,
            kCVImageBufferColorPrimaries_ITU_R_2020 as *const c_void,
        ) != 0
        {
            return YuvPrimaries::Bt2020;
        }
    }
    YuvPrimaries::Bt709
}

#[cfg(test)]
mod tests {
    use super::*;
    use ard_rs::media_stream::AccessUnit;

    #[test]
    fn clean_aperture_provides_the_only_authorized_codec_crop() {
        let rect = pixel_rect_from_clean_aperture(
            2_880,
            464,
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 2_880.0,
                    height: 450.0,
                },
            },
        )
        .expect("integer clean aperture");
        assert_eq!(
            rect,
            PixelRect {
                x: 0,
                y: 0,
                width: 2_880,
                height: 450,
            }
        );
    }

    #[test]
    fn clean_aperture_rejects_unrepresentable_or_out_of_bounds_crops() {
        let odd_origin = CGRect {
            origin: CGPoint { x: 0.0, y: 1.0 },
            size: CGSize {
                width: 100.0,
                height: 100.0,
            },
        };
        assert!(pixel_rect_from_clean_aperture(100, 102, odd_origin).is_err());

        let outside = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 101.0,
                height: 100.0,
            },
        };
        assert!(pixel_rect_from_clean_aperture(100, 100, outside).is_err());
    }

    /// Split an Annex-B byte stream into access units, keeping parameter sets
    /// with the following VCL NAL units.
    fn annex_b_to_access_units(data: &[u8], codec: MediaStreamCodec) -> Vec<AccessUnit> {
        let mut nals = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let start = find_start_code(data, offset);
            let Some((nal_start, code_len)) = start else {
                break;
            };
            let payload_start = nal_start + code_len;
            let nal_end = find_start_code(data, payload_start)
                .map(|(start, _)| start)
                .unwrap_or(data.len());
            if nal_end > payload_start {
                nals.push(data[payload_start..nal_end].to_vec());
            }
            offset = nal_end;
        }

        let mut units: Vec<AccessUnit> = Vec::new();
        let mut current: Vec<Vec<u8>> = Vec::new();
        let mut current_has_vcl = false;
        let mut timestamp = 0u32;
        for nal in nals {
            let is_vcl = match codec {
                MediaStreamCodec::H264 => (1..=5).contains(&(nal[0] & 0x1f)),
                MediaStreamCodec::Hevc => ((nal[0] >> 1) & 0x3f) <= 31,
            };
            if is_vcl && current_has_vcl {
                units.push(AccessUnit {
                    timestamp,
                    decode_order_number: None,
                    nal_units: std::mem::take(&mut current),
                });
                timestamp = timestamp.wrapping_add(1);
                current_has_vcl = false;
            }
            current.push(nal);
            current_has_vcl |= is_vcl;
        }
        if !current.is_empty() {
            units.push(AccessUnit {
                timestamp,
                decode_order_number: None,
                nal_units: current,
            });
        }
        units
    }

    fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut i = from;
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                return Some((i, 3));
            }
            i += 1;
        }
        None
    }

    #[test]
    fn callback_context_survives_no_output_and_reordered_callbacks() {
        let state = Arc::new(CallbackState::default());
        let make_context = |stream_index, timestamp, submission| {
            Box::into_raw(Box::new(SourceFrameContext {
                stream_index,
                timestamp,
                submission,
                encoded_bytes: submission as usize + 10,
                visible_rect: PixelRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                output_state: Arc::clone(&state),
            })) as *mut c_void
        };
        let first = make_context(0, 100, 0);
        let second = make_context(3, 200, 1);
        unsafe {
            decompression_output_callback(
                std::ptr::null_mut(),
                second,
                0,
                0x20,
                std::ptr::null(),
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: 0,
                    epoch: 0,
                },
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: 0,
                    epoch: 0,
                },
            );
            decompression_output_callback(
                std::ptr::null_mut(),
                first,
                -1,
                0x40,
                std::ptr::null(),
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: 0,
                    epoch: 0,
                },
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: 0,
                    epoch: 0,
                },
            );
        }
        let outputs = state
            .outputs
            .lock()
            .expect("callback output lock")
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert_eq!(
            outputs
                .iter()
                .map(|output| (
                    output.stream_index,
                    output.timestamp,
                    output.submission,
                    output.status,
                    output.info_flags,
                    output.frame.is_none(),
                ))
                .collect::<Vec<_>>(),
            vec![(3, 200, 1, 0, 0x20, true), (0, 100, 0, -1, 0x40, true)]
        );
    }

    #[test]
    fn decodes_real_h264_sample_with_vcp() {
        let path = "/tmp/ardre/avc_test/sample.h264";
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let units = annex_b_to_access_units(&bytes, MediaStreamCodec::H264);
        assert!(!units.is_empty(), "sample must contain access units");
        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::H264);
        let mut decoded = 0;
        let mut first: Option<DecodedSlice> = None;
        for unit in &units {
            for output in decoder.decode(0, unit) {
                assert_eq!(output.status, 0, "H.264 callback must succeed");
                assert!(
                    output.conversion_error.is_none(),
                    "H.264 native-plane conversion failed: {:?}",
                    output.conversion_error
                );
                if let Some(frame) = output.frame {
                    decoded += 1;
                    if first.is_none() {
                        first = Some(frame);
                    }
                }
            }
        }
        for output in decoder.flush() {
            assert_eq!(output.status, 0, "H.264 flush callback must succeed");
            assert!(
                output.conversion_error.is_none(),
                "H.264 native-plane conversion failed: {:?}",
                output.conversion_error
            );
            if let Some(frame) = output.frame {
                decoded += 1;
                if first.is_none() {
                    first = Some(frame);
                }
            }
        }
        assert!(decoded > 0, "VCP should decode at least one frame");
        assert!(decoder.take_errors().is_empty());
        let first = first.expect("first frame");
        assert_eq!(first.width, 320);
        assert_eq!(first.height, 240);
        assert_eq!(decoder.configured_dimensions(), Some((320, 240)));
        assert_eq!(first.y_plane.len(), 320 * 240);
        assert_eq!(first.uv_plane.len(), 320 * 120);
    }

    #[test]
    fn decodes_real_hevc_sample_with_vcp() {
        let path = "/tmp/ardre/avc_test/sample.h265";
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let units = annex_b_to_access_units(&bytes, MediaStreamCodec::Hevc);
        assert!(!units.is_empty(), "sample must contain access units");
        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::Hevc);
        let mut decoded = 0;
        let mut first = None;
        for unit in &units {
            for output in decoder.decode(0, unit) {
                assert_eq!(output.status, 0, "HEVC callback must succeed");
                assert!(
                    output.conversion_error.is_none(),
                    "HEVC native-plane conversion failed: {:?}",
                    output.conversion_error
                );
                decoded += usize::from(output.frame.is_some());
                first = first.or(output.frame);
            }
        }
        for output in decoder.flush() {
            assert_eq!(output.status, 0, "HEVC flush callback must succeed");
            assert!(
                output.conversion_error.is_none(),
                "HEVC native-plane conversion failed: {:?}",
                output.conversion_error
            );
            decoded += usize::from(output.frame.is_some());
            first = first.or(output.frame);
        }
        assert!(decoded > 0, "VCP should decode at least one HEVC frame");
        assert!(decoder.take_errors().is_empty());
        let first = first.expect("VCP should decode at least one HEVC frame");
        assert_eq!((first.width, first.height), (320, 240));
        assert_eq!(first.y_plane.len(), 320 * 240);
        assert_eq!(first.uv_plane.len(), 320 * 120);
    }

    #[test]
    #[ignore = "requires ARD_VCP_BENCH_SAMPLE and prints a local hardware latency baseline"]
    fn benchmarks_realtime_four_slice_frame_boundaries() {
        use std::time::Instant;

        // An absent sample must skip, not panic: this is an opt-in benchmark,
        // and a panic made `--ignored` runs look like a broken suite even
        // though nothing was being tested.
        let Ok(path) = std::env::var("ARD_VCP_BENCH_SAMPLE") else {
            eprintln!("skipping: ARD_VCP_BENCH_SAMPLE is not set");
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping: benchmark sample {path} is not readable");
            return;
        };
        let mut units = annex_b_to_access_units(&bytes, MediaStreamCodec::Hevc);
        assert!(
            units.len() >= 8 && units.len().is_multiple_of(4),
            "benchmark needs complete four-slice frame groups"
        );
        for (frame_index, group) in units.chunks_mut(4).enumerate() {
            let timestamp = u32::try_from(frame_index)
                .expect("benchmark frame count fits u32")
                .saturating_mul(1_500);
            for unit in group {
                unit.timestamp = timestamp;
            }
        }

        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::Hevc);
        let started = Instant::now();
        let mut frame_latencies = Vec::with_capacity(units.len() / 4);
        let mut decoded_images = 0usize;
        for group in units.chunks(4) {
            let frame_started = Instant::now();
            let mut outputs = Vec::with_capacity(4);
            for (slice_index, unit) in group.iter().enumerate() {
                outputs.extend(decoder.decode(slice_index, unit));
            }
            outputs.extend(decoder.finish_frame());
            decoded_images += outputs
                .iter()
                .filter(|output| output.frame.is_some())
                .count();
            assert!(
                decoder.take_errors().is_empty(),
                "VCP benchmark decode failed"
            );
            frame_latencies.push(frame_started.elapsed());
        }
        let elapsed = started.elapsed();
        frame_latencies.sort_unstable();
        let percentile = |numerator: usize, denominator: usize| {
            let index = (frame_latencies.len() - 1)
                .saturating_mul(numerator)
                .div_ceil(denominator);
            frame_latencies[index]
        };
        eprintln!(
            "VCP four-slice synchronous baseline: frames={} images={} total_ms={:.3} fps={:.2} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
            frame_latencies.len(),
            decoded_images,
            elapsed.as_secs_f64() * 1_000.0,
            frame_latencies.len() as f64 / elapsed.as_secs_f64(),
            percentile(50, 100).as_secs_f64() * 1_000.0,
            percentile(95, 100).as_secs_f64() * 1_000.0,
            percentile(99, 100).as_secs_f64() * 1_000.0,
        );
        assert_eq!(decoded_images, units.len());
    }

    /// Compare one decoded band plane against the independent reference.
    ///
    /// The comparison is exact except for the plane-range expansion both sides
    /// perform, which can round a level by one.
    fn assert_band_matches(reference: &[u8], ours: &[u8], plane: &str, band: usize) {
        assert_eq!(reference.len(), ours.len(), "band {band} {plane} size");
        let mut worst = 0_i16;
        let mut total = 0_i64;
        for (reference, ours) in reference.iter().zip(ours) {
            let delta = (*ours as i16 - *reference as i16).abs();
            worst = worst.max(delta);
            total += i64::from(delta);
        }
        let mean = total as f64 / reference.len() as f64;
        // Luma is the plane that exposes a displaced, duplicated or
        // re-quantised band, so it is held to one level. VideoProcessing's own
        // plane-range conversion rounds chroma harder (a few percent of samples
        // land one level away), so chroma is bounded by its mean instead: a band
        // assembled from the wrong picture moves that mean by orders of
        // magnitude, not by one level.
        let (max_delta, max_mean) = match plane {
            "luma" => (1_i16, 0.05_f64),
            _ => (64_i16, 2.0_f64),
        };
        assert!(
            worst <= max_delta && mean <= max_mean,
            "band {band} {plane} diverges from the independent decode: max={worst} mean={mean:.4}"
        );
    }

    /// A real device stream is tagged Display P3 (`colour_primaries = 12`,
    /// reported as `smpte432`) with the sRGB transfer function, so the decoded
    /// planes are P3-encoded even though the luma/chroma matrix is BT.709. A
    /// renderer that presents those numbers as sRGB moves the whole picture —
    /// measured on a real capture, the red channel of a flat desktop background
    /// shifts by ~48 of 255.
    ///
    /// The decoder is responsible for handing that tag on, because it reads it
    /// off the output buffer. This retags the in-repo fixture's VUI instead of
    /// capturing a device: the planes are untouched, only the attachment moves,
    /// so the assertion is about the tag being read at all.
    #[test]
    fn reads_the_display_p3_primaries_the_codec_attaches() {
        use std::process::{Command, Stdio};

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixture =
            root.join("../ard-core/examples/fixtures/oracle-diagonal-frames-1920x1080-4x272.h265");
        let Ok(bytes) = std::fs::read(&fixture) else {
            eprintln!("skipping: {} is not readable", fixture.display());
            return;
        };
        assert_eq!(
            decode_primaries(&bytes),
            vec![YuvPrimaries::Bt709],
            "the untagged fixture must keep the historical sRGB/Rec.709 reading"
        );

        let tagged = std::env::temp_dir().join("ard-viewer-p3-primaries.h265");
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&fixture)
            .args([
                "-c",
                "copy",
                // Table E-3 value 12 is SMPTE EG 432-1, i.e. Display P3 D65.
                "-bsf:v",
                "hevc_metadata=colour_primaries=12",
                "-f",
                "hevc",
            ])
            .arg(&tagged)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match output {
            Ok(status) if status.success() => {}
            _ => {
                eprintln!("skipping: ffmpeg could not retag the fixture");
                return;
            }
        }
        let tagged_bytes = std::fs::read(&tagged).expect("tagged fixture is readable");
        assert_eq!(
            decode_primaries(&tagged_bytes),
            vec![YuvPrimaries::P3D65],
            "a Display P3 tag must reach the renderer as P3, not as sRGB"
        );
        let _ = std::fs::remove_file(&tagged);
    }

    /// Decode an Annex-B HEVC stream and report the distinct plane colour
    /// descriptions the decoder produced, in order of first appearance.
    fn decode_primaries(bytes: &[u8]) -> Vec<YuvPrimaries> {
        let units = annex_b_to_access_units(bytes, MediaStreamCodec::Hevc);
        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::Hevc);
        let mut seen: Vec<YuvPrimaries> = Vec::new();
        let collect = |outputs: Vec<super::DecodedOutput>, seen: &mut Vec<YuvPrimaries>| {
            for output in outputs {
                let Some(frame) = output.frame else { continue };
                if !seen.contains(&frame.primaries) {
                    seen.push(frame.primaries);
                }
            }
        };
        for (index, unit) in units.iter().enumerate() {
            collect(decoder.decode(index % 4, unit), &mut seen);
        }
        collect(decoder.flush(), &mut seen);
        assert!(decoder.take_errors().is_empty());
        seen
    }

    /// The four native bands Apple's media stream carries are separate coded
    /// pictures in one serial prediction chain. They must reach the frame
    /// buffer byte-for-byte as the decoder produced them: a decoder or
    /// compositor that displaces, duplicates or re-quantises a band shows up
    /// as a seam on the band grid.
    ///
    /// This pins the band assembly geometry only. It does not reproduce or
    /// explain the H.265 banding, which is still open.
    ///
    /// The check decodes the first desktop frames of the in-repo oracle
    /// fixture through the production decoder and compositor and compares the
    /// composed frame against an independent decode of the same elementary
    /// stream. `ffmpeg` is used only as the reference decoder; without it the
    /// test skips exactly like the other sample-driven tests in this module.
    #[test]
    fn oracle_band_composite_matches_an_independent_decode() {
        use crate::media::pipeline::SliceCompositor;
        use std::process::{Command, Stdio};

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixture =
            root.join("../ard-core/examples/fixtures/oracle-diagonal-frames-1920x1080-4x272.h265");
        let Ok(bytes) = std::fs::read(&fixture) else {
            eprintln!("skipping: {} is not readable", fixture.display());
            return;
        };
        let units = annex_b_to_access_units(&bytes, MediaStreamCodec::Hevc);
        assert!(
            units.len() >= 20,
            "fixture must hold at least five desktop frames"
        );

        // Reference: 1200 codec-aligned 1920x272 pictures in decode order.
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-vsync", "0", "-i"])
            .arg(&fixture)
            .args([
                "-frames:v",
                "20",
                // Match the decoder's requested plane range: the production
                // session asks VideoProcessing for `420f`, so a video-range
                // fixture reaches the client in full range. The reference must
                // be expanded the same way before the pixels are compared.
                "-vf",
                "scale=out_range=full",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "rawvideo",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let Ok(output) = output else {
            eprintln!("skipping: ffmpeg is not available");
            return;
        };
        if !output.status.success() {
            eprintln!("skipping: ffmpeg could not decode the fixture");
            return;
        }
        const BAND: usize = 1920 * 272;
        const BAND_CHROMA: usize = 960 * 136;
        let picture = BAND + 2 * BAND_CHROMA;
        assert_eq!(
            output.stdout.len(),
            20 * picture,
            "ffmpeg reference must hold twenty band pictures"
        );

        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::Hevc);
        let mut compositor = SliceCompositor::new((1920, 1080));
        let mut ours = vec![0_u8; 1920 * 1080 + 2 * 960 * 540];
        let mut compared = 0_usize;

        for (index, unit) in units.iter().enumerate().take(20) {
            let mut outputs = decoder.decode(index % 4, unit);
            if index % 4 == 3 {
                outputs.extend(decoder.finish_frame());
            }
            for output in outputs {
                let Some(frame) = output.frame else { continue };
                compositor
                    .push(output.stream_index, output.encoded_bytes, Some(frame))
                    .expect("native band geometry");
            }
            if index % 4 != 3 {
                continue;
            }
            let errors = decoder.take_errors();
            assert!(errors.is_empty(), "fixture decode errors: {errors:?}");
            let Some(frame) = compositor.finish_frame().expect("native band layout") else {
                continue;
            };
            for update in &frame.updates {
                let width = frame.width as usize;
                for row in 0..update.y_rows as usize {
                    let source = row * width;
                    let destination = (update.y_origin as usize + row) * width;
                    ours[destination..destination + width]
                        .copy_from_slice(&update.pixels.y_plane[source..source + width]);
                }
                let uv_base = 1920 * 1080;
                for row in 0..update.uv_rows as usize {
                    let source = row * width;
                    let destination = uv_base + (update.uv_origin as usize + row) * width;
                    ours[destination..destination + width]
                        .copy_from_slice(&update.pixels.uv_plane[source..source + width]);
                }
            }
            compared += 1;
        }
        assert_eq!(compared, 5, "five desktop frames must compose");

        // The reference desktop frame is the four band pictures stacked, with
        // the codec padding of the last band cropped away exactly like the
        // compositor crops it.
        let desktop = 4_usize;
        let base = desktop * 4 * picture;
        let mut y_offset = 0_usize;
        let mut uv_offset = 0_usize;
        for band in 0..4 {
            let rows = if band == 3 { 264 } else { 272 };
            let chroma_rows = rows / 2;
            let picture_base = base + band * picture;
            let y_source = &output.stdout[picture_base..picture_base + rows * 1920];
            let y_target = &ours[y_offset..y_offset + rows * 1920];
            assert_band_matches(y_source, y_target, "luma", band);
            let u_source =
                &output.stdout[picture_base + BAND..picture_base + BAND + chroma_rows * 960];
            let v_source = &output.stdout[picture_base + BAND + BAND_CHROMA
                ..picture_base + BAND + BAND_CHROMA + chroma_rows * 960];
            let mut u_target = Vec::with_capacity(chroma_rows * 960);
            let mut v_target = Vec::with_capacity(chroma_rows * 960);
            for row in 0..chroma_rows {
                let start = 1920 * 1080 + (uv_offset + row) * 1920;
                let line = &ours[start..start + 1920];
                u_target.extend(line.iter().step_by(2).copied());
                v_target.extend(line.iter().skip(1).step_by(2).copied());
            }
            assert_band_matches(u_source, &u_target, "U", band);
            assert_band_matches(v_source, &v_target, "V", band);
            y_offset += rows * 1920;
            uv_offset += chroma_rows;
        }
    }
}
