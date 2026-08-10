//! macOS VideoToolbox decoder for the AVC media stream.
//!
//! Consumes access units (H.264 or HEVC NAL units) from `ard_rs::avc`, builds
//! a `CMVideoFormatDescription` from the parameter sets, decodes with a
//! `VTDecompressionSession`, and returns RGBA8 pixels for the viewer's
//! existing framebuffer path.

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code)]

use std::os::raw::{c_int, c_void};
use std::sync::Mutex;

use ard_rs::avc::{AccessUnit, MediaStreamCodec};

use super::DecodedFrame;

type OSStatus = i32;
type CFIndex = isize;
type CFAllocatorRef = *const c_void;
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
    fn CFRelease(cf: *const c_void);
    fn CFRetain(cf: *const c_void) -> *const c_void;
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
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetDimensions(
        video_desc: CMVideoFormatDescriptionRef,
    ) -> CMVideoDimensions;
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

    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: VTDecodeFrameFlags,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut VTDecodeInfoFlags,
    ) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);

    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u32) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: CVPixelBufferRef,
        unlock_flags: u32,
    ) -> OSStatus;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMVideoDimensions {
    width: c_int,
    height: c_int,
}

#[allow(non_upper_case_globals)]
const kCFNumberSInt32Type: i32 = 3;
#[allow(non_upper_case_globals)]
const kCFStringEncodingUTF8: u32 = 0x08000100;
#[allow(non_upper_case_globals)]
const kCVPixelFormatType_32BGRA: u32 = 0x42475241; // 'BGRA'
#[allow(clippy::missing_safety_doc)]
#[allow(private_interfaces)]
pub unsafe extern "C" fn decompression_output_callback(
    decompression_output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: VTDecodeInfoFlags,
    image_buffer: CVPixelBufferRef,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    if status != 0 || image_buffer.is_null() {
        return;
    }
    let slot = &*(decompression_output_ref_con as *const Mutex<Option<CVPixelBufferRef>>);
    if let Ok(mut guard) = slot.lock() {
        if let Some(previous) = guard.replace(image_buffer) {
            unsafe { CFRelease(previous) };
        }
        unsafe { CFRetain(image_buffer) };
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
    output_slot: Box<Mutex<Option<CVPixelBufferRef>>>,
}

impl Drop for NativeDecoder {
    fn drop(&mut self) {
        unsafe {
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session as *const c_void);
            CFRelease(self.format_description as *const c_void);
            if let Ok(mut slot) = self.output_slot.lock()
                && let Some(buffer) = slot.take()
            {
                CFRelease(buffer);
            }
        }
    }
}

/// Safe wrapper around a VideoToolbox decompression session.
pub struct VideoToolboxDecoder {
    codec: MediaStreamCodec,
    native: Option<NativeDecoder>,
    parameter_sets: Vec<Vec<u8>>,
    frames_decoded: u64,
}

impl VideoToolboxDecoder {
    pub fn new(codec: MediaStreamCodec) -> Self {
        Self {
            codec,
            native: None,
            parameter_sets: Vec::new(),
            frames_decoded: 0,
        }
    }

    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    /// Decode one access unit; returns the displayable frame when the decoder
    /// produced output.
    pub fn decode(&mut self, unit: &AccessUnit) -> Option<DecodedFrame> {
        let parameter_sets = self.parameter_sets_for(unit);
        if !parameter_sets.is_empty() {
            let changed = self.merge_parameter_sets(parameter_sets);
            if changed {
                // VideoToolbox format descriptions are immutable. A server
                // keyframe can refresh SPS/PPS/VPS, so recreate the session
                // before decoding the first frame using the new parameters.
                self.native = None;
            }
        }
        if self.native.is_none() {
            match self.create_session() {
                Ok(decoder) => self.native = Some(decoder),
                Err(_) => return None,
            }
        }
        let Some(native) = &self.native else {
            return None;
        };
        let avcc = unit.to_avcc();
        let result = unsafe { decode_access_unit(native, &avcc) };
        let frame = result.ok().flatten();
        if frame.is_some() {
            self.frames_decoded += 1;
        }
        frame
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

    fn create_session(&self) -> Result<NativeDecoder, ()> {
        if self.parameter_sets.is_empty() {
            return Err(());
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
                    &mut format_description,
                ),
            }
        };
        if status != 0 || format_description.is_null() {
            return Err(());
        }

        let output_slot = Box::new(Mutex::new(None));
        let refcon = &*output_slot as *const Mutex<Option<CVPixelBufferRef>> as *mut c_void;
        let callback: VTDecompressionOutputCallback = decompression_output_callback;
        let record = VTDecompressionOutputCallbackRecord { callback, refcon };
        let Some(destination_attributes) = DestinationAttributes::new() else {
            eprintln!("destination attributes creation failed");
            unsafe { CFRelease(format_description as *const c_void) };
            return Err(());
        };
        let mut session: VTDecompressionSessionRef = std::ptr::null();
        let status = unsafe {
            VTDecompressionSessionCreate(
                std::ptr::null(),
                format_description,
                std::ptr::null(),
                destination_attributes.dictionary,
                &record,
                &mut session,
            )
        };
        drop(destination_attributes);
        if status != 0 || session.is_null() {
            unsafe { CFRelease(format_description as *const c_void) };
            return Err(());
        }
        Ok(NativeDecoder {
            format_description,
            session,
            output_slot,
        })
    }
}

struct DestinationAttributes {
    dictionary: CFDictionaryRef,
    key: CFStringRef,
    value: CFNumberRef,
}

impl DestinationAttributes {
    fn new() -> Option<Self> {
        let key = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                c"PixelFormatType".as_ptr() as *const u8,
                kCFStringEncodingUTF8,
            )
        };
        if key.is_null() {
            return None;
        }
        let value = {
            let raw = kCVPixelFormatType_32BGRA;
            unsafe {
                CFNumberCreate(
                    std::ptr::null(),
                    kCFNumberSInt32Type,
                    &raw as *const u32 as *const c_void,
                )
            }
        };
        if value.is_null() {
            unsafe { CFRelease(key) };
            return None;
        }
        let dictionary = unsafe {
            CFDictionaryCreateMutable(std::ptr::null(), 1, std::ptr::null(), std::ptr::null())
        };
        if dictionary.is_null() {
            unsafe {
                CFRelease(key);
                CFRelease(value);
            }
            return None;
        }
        unsafe {
            CFDictionarySetValue(dictionary, key as *const c_void, value as *const c_void);
        }
        Some(Self {
            dictionary,
            key,
            value,
        })
    }
}

impl Drop for DestinationAttributes {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.dictionary);
            CFRelease(self.key);
            CFRelease(self.value);
        }
    }
}

unsafe fn decode_access_unit(
    native: &NativeDecoder,
    avcc: &[u8],
) -> Result<Option<DecodedFrame>, ()> {
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
        return Err(());
    }
    let status =
        CMBlockBufferReplaceDataBytes(avcc.as_ptr() as *const c_void, block_buffer, 0, avcc.len());
    if status != 0 {
        CFRelease(block_buffer);
        return Err(());
    }
    let mut sample_buffer: CMSampleBufferRef = std::ptr::null();
    let sample_size = avcc.len();
    let status = CMSampleBufferCreateReady(
        std::ptr::null(),
        block_buffer,
        native.format_description,
        1,
        0,
        std::ptr::null(),
        1,
        &sample_size,
        &mut sample_buffer,
    );
    if status != 0 || sample_buffer.is_null() {
        CFRelease(block_buffer);
        return Err(());
    }
    let mut info_flags: VTDecodeInfoFlags = 0;
    let status = VTDecompressionSessionDecodeFrame(
        native.session,
        sample_buffer,
        0, // synchronous decode: the callback runs before DecodeFrame returns
        std::ptr::null_mut(),
        &mut info_flags,
    );
    if status == 0 {
        let _ = VTDecompressionSessionWaitForAsynchronousFrames(native.session);
    }
    CFRelease(sample_buffer);
    CFRelease(block_buffer);
    if status != 0 {
        return Err(());
    }
    let buffer = native
        .output_slot
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    let Some(buffer) = buffer else {
        return Ok(None);
    };
    let frame = pixel_buffer_to_rgba(buffer);
    CFRelease(buffer);
    Ok(frame)
}

unsafe fn pixel_buffer_to_rgba(buffer: CVPixelBufferRef) -> Option<DecodedFrame> {
    let width = CVPixelBufferGetWidth(buffer);
    let height = CVPixelBufferGetHeight(buffer);
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return None;
    }
    let status = CVPixelBufferLockBaseAddress(buffer, 0);
    if status != 0 {
        return None;
    }
    let base = CVPixelBufferGetBaseAddress(buffer);
    let bytes_per_row = CVPixelBufferGetBytesPerRow(buffer);
    let expected = width * height * 4;
    let mut rgba = Vec::with_capacity(expected);
    if !base.is_null() && bytes_per_row >= width * 4 {
        for row in 0..height {
            let src = (base as *const u8).add(row * bytes_per_row);
            let row_bytes = std::slice::from_raw_parts(src, width * 4);
            // BGRA -> RGBA byte swap.
            for pixel in row_bytes.chunks_exact(4) {
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
    }
    CVPixelBufferUnlockBaseAddress(buffer, 0);
    if rgba.len() != expected {
        return None;
    }
    Some(DecodedFrame {
        width: width as u32,
        height: height as u32,
        encoded_bytes: 0,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ard_rs::avc::AccessUnit;

    /// Split an Annex-B byte stream into access units, keeping parameter sets
    /// with the following VCL NAL units.
    fn annex_b_to_access_units(data: &[u8]) -> Vec<AccessUnit> {
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
        let mut timestamp = 0u32;
        for nal in nals {
            let nal_type = nal[0] & 0x1f;
            let is_vcl = (1..=5).contains(&nal_type);
            if is_vcl && !current.is_empty() {
                units.push(AccessUnit {
                    timestamp,
                    nal_units: std::mem::take(&mut current),
                });
                timestamp = timestamp.wrapping_add(1);
            }
            current.push(nal);
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
    fn decodes_real_h264_sample_with_videotoolbox() {
        let path = "/tmp/ardre/avc_test/sample.h264";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not available");
            return;
        };
        let units = annex_b_to_access_units(&bytes);
        assert!(!units.is_empty(), "sample must contain access units");
        let mut decoder = VideoToolboxDecoder::new(MediaStreamCodec::H264);
        let mut decoded = 0;
        let mut first: Option<DecodedFrame> = None;
        for unit in &units {
            if let Some(frame) = decoder.decode(unit) {
                decoded += 1;
                if first.is_none() {
                    first = Some(frame);
                }
            }
        }
        assert!(decoded > 0, "VideoToolbox should decode at least one frame");
        let first = first.expect("first frame");
        assert_eq!(first.width, 320);
        assert_eq!(first.height, 240);
        assert_eq!(first.rgba.len(), 320 * 240 * 4);
        let out = "/tmp/ardre/avc_test/out.rgba";
        std::fs::write(out, &first.rgba).expect("write rgba");
        eprintln!("decoded {decoded} frames; first frame dumped to {out}");
    }
}
