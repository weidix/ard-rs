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

use super::{DecodedOutput, DecodedSlice, YuvMatrix, YuvRange};

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
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
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
const PIXEL_FORMAT_NV12_VIDEO_RANGE: u32 = 0x3432_3076; // '420v'
const PIXEL_FORMAT_NV12_FULL_RANGE: u32 = 0x3432_3066; // '420f'
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
        match pixel_buffer_to_nv12(image_buffer) {
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
    dimensions: (u32, u32),
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
    frames_decoded: u64,
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
            frames_decoded: 0,
            next_submission: 0,
            needs_sync: false,
            errors: VecDeque::new(),
        }
    }

    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Resolution configured on the active VideoToolbox session. When the
    /// server applies an explicitly requested display mode and sends updated
    /// parameter sets, the decoder recreates the immutable native session.
    pub fn configured_dimensions(&self) -> Option<(u32, u32)> {
        self.native.as_ref().map(|native| native.dimensions)
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
        self.frames_decoded += outputs
            .iter()
            .filter(|output| output.frame.is_some())
            .count() as u64;
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
        drop(destination_attributes);
        if status != 0 || session.is_null() {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err(format!("VCP session creation failed with status {status}"));
        }
        Ok(NativeDecoder {
            format_description,
            session,
            api,
            output_state,
            dimensions: (dimensions.width as u32, dimensions.height as u32),
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
            (
                c"PixelFormatType".as_ptr(),
                PIXEL_FORMAT_NV12_VIDEO_RANGE as c_int,
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

unsafe fn pixel_buffer_to_nv12(buffer: CVPixelBufferRef) -> Result<DecodedSlice, String> {
    let width = CVPixelBufferGetWidth(buffer);
    let height = CVPixelBufferGetHeight(buffer);
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(format!(
            "VCP returned invalid CVPixelBuffer dimensions {width}x{height}"
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
    if y_width != width
        || y_height != height
        || uv_width != width.div_ceil(2)
        || uv_height != height.div_ceil(2)
    {
        return Err(format!(
            "VCP returned inconsistent NV12 plane geometry: buffer={width}x{height}, y={y_width}x{y_height}, uv={uv_width}x{uv_height}"
        ));
    }
    let uv_row_bytes = uv_width
        .checked_mul(2)
        .ok_or_else(|| "VCP NV12 chroma row length overflow".to_owned())?;
    let y_len = y_width
        .checked_mul(y_height)
        .ok_or_else(|| "VCP NV12 luma plane length overflow".to_owned())?;
    let uv_len = uv_row_bytes
        .checked_mul(uv_height)
        .ok_or_else(|| "VCP NV12 chroma plane length overflow".to_owned())?;
    let _lock = PixelBufferReadLock::new(buffer)?;
    let y_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
    let uv_base = CVPixelBufferGetBaseAddressOfPlane(buffer, 1);
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
    let mut y_plane = Vec::with_capacity(y_len);
    let mut uv_plane = Vec::with_capacity(uv_len);
    if !y_base.is_null() && !uv_base.is_null() && y_stride >= y_width && uv_stride >= uv_row_bytes {
        for row in 0..y_height {
            let source = (y_base as *const u8).add(row * y_stride);
            y_plane.extend_from_slice(std::slice::from_raw_parts(source, y_width));
        }
        for row in 0..uv_height {
            let source = (uv_base as *const u8).add(row * uv_stride);
            uv_plane.extend_from_slice(std::slice::from_raw_parts(source, uv_row_bytes));
        }
    }
    if y_plane.len() != y_len || uv_plane.len() != uv_len {
        return Err(format!(
            "VCP returned inaccessible NV12 plane storage: y={}/{y_len}, uv={}/{uv_len}",
            y_plane.len(),
            uv_plane.len()
        ));
    }
    let matrix = ycbcr_matrix(buffer, width, height);
    Ok(DecodedSlice {
        width: width as u32,
        height: height as u32,
        y_plane,
        uv_plane,
        range,
        matrix,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ard_rs::media_stream::AccessUnit;

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
}
