use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ard_rs::{MvsGpuTile, MvsGpuTileUpdate};
use iced::widget::shader::{self, Program};
use iced::{Element, Fill, Rectangle, Size};

use crate::recording::{PresentationSource, RecordingControl, RecordingTap};
use crate::session_runtime::{FramePacket, SessionEvent, SharedMailbox, TileSet, fitted_viewport};

#[derive(Debug, Clone)]
pub struct RemoteProgram {
    mailbox: SharedMailbox,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
    recording: Arc<RecordingControl>,
}

impl RemoteProgram {
    pub fn new(
        mailbox: SharedMailbox,
        zoom: f32,
        actual_size: bool,
        should_interpolate: bool,
        sharp_sampling: bool,
        recording: Arc<RecordingControl>,
    ) -> Self {
        Self {
            mailbox,
            zoom,
            actual_size,
            should_interpolate,
            sharp_sampling,
            recording,
        }
    }
}

impl<Message> Program<Message> for RemoteProgram {
    type State = ();
    type Primitive = RemotePrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: iced::mouse::Cursor,
        bounds: Rectangle,
    ) -> Self::Primitive {
        RemotePrimitive {
            mailbox: Arc::clone(&self.mailbox),
            bounds,
            zoom: self.zoom,
            actual_size: self.actual_size,
            should_interpolate: self.should_interpolate,
            sharp_sampling: self.sharp_sampling,
            recording: Arc::clone(&self.recording),
        }
    }
}

pub fn remote_display<Message: 'static>(
    mailbox: SharedMailbox,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
    recording: Arc<RecordingControl>,
) -> Element<'static, Message> {
    shader::Shader::new(RemoteProgram::new(
        mailbox,
        zoom,
        actual_size,
        should_interpolate,
        sharp_sampling,
        recording,
    ))
    .width(Fill)
    .height(Fill)
    .into()
}

#[derive(Debug)]
pub struct RemotePrimitive {
    mailbox: SharedMailbox,
    bounds: Rectangle,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
    recording: Arc<RecordingControl>,
}

impl shader::Primitive for RemotePrimitive {
    type Pipeline = RemotePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        let changed_session = pipeline
            .mailbox
            .as_ref()
            .is_none_or(|mailbox| !Arc::ptr_eq(mailbox, &self.mailbox));
        if changed_session {
            pipeline.reset_session();
        }
        pipeline.mailbox = Some(Arc::clone(&self.mailbox));
        pipeline.recording = Some(Arc::clone(&self.recording));
        pipeline.zoom = self.zoom;
        pipeline.actual_size = self.actual_size;
        pipeline.should_interpolate = self.should_interpolate;
        pipeline.sharp_sampling = self.sharp_sampling;
        pipeline.scale_factor = viewport.scale_factor();
        pipeline.bounds = self.bounds;

        let frame = self
            .mailbox
            .lock()
            .ok()
            .and_then(|mut mailbox| mailbox.latest.take());
        let Some(mut frame) = frame else { return };
        // The RGBA pool must be fed back even when the upload below is skipped.
        let avc_timing = frame.nv12.as_ref().and_then(|frame| frame.timing);
        let outcome = pipeline.upload(&mut frame);
        let uploaded = outcome.is_uploaded();
        if uploaded {
            // A recorded frame must be one that was really drawn, so the flag is
            // consumed by `render` rather than here.
            pipeline.presented_frame.store(true, Ordering::Release);
        }
        if let Some(buffer) = frame.rgba.take()
            && let Ok(mut mailbox) = self.mailbox.lock()
        {
            mailbox.recycle_rgba(buffer);
        }
        if let Ok(mut pending) = pipeline.pending_avc_timing.lock() {
            *pending = uploaded.then_some(avc_timing).flatten();
        }
        // A recreated texture that has not yet collected all four native slices
        // is incomplete, not broken. Only a frame that failed validation is a
        // rendering failure, and a later successful upload clears it — without
        // that recovery the error text replaced the remote desktop for the rest
        // of the session.
        match outcome {
            UploadOutcome::Invalid if frame.nv12.is_some() => {
                pipeline.reported_failure = true;
                if let Ok(mut mailbox) = self.mailbox.lock() {
                    mailbox.push_event(SessionEvent::RenderFailed(
                        "原生 NV12 帧未通过 GPU 纹理布局校验".into(),
                    ));
                }
            }
            UploadOutcome::Uploaded => {
                if pipeline.reported_failure
                    && let Ok(mut mailbox) = self.mailbox.lock()
                {
                    mailbox.push_event(SessionEvent::RenderRecovered);
                    pipeline.reported_failure = false;
                }
            }
            UploadOutcome::Invalid | UploadOutcome::Incomplete => {}
        }
    }

    fn render(
        &self,
        pipeline: &Self::Pipeline,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        clip_bounds: &Rectangle<u32>,
    ) {
        pipeline.render(encoder, target, clip_bounds);
    }
}

struct DecodedTexture {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    storage_view: wgpu::TextureView,
    render_bind_group: wgpu::BindGroup,
}

struct NativeNv12Texture {
    width: u32,
    height: u32,
    y_texture: wgpu::Texture,
    uv_texture: wgpu::Texture,
    conversion_buffer: wgpu::Buffer,
    render_bind_group: wgpu::BindGroup,
    /// Colour description of the planes currently uploaded. Recording keeps it so
    /// a captured frame can be tagged with the same matrix and range the
    /// presenter used.
    range: crate::media::YuvRange,
    matrix: crate::media::YuvMatrix,
    /// Primary set the planes are encoded in. The presentation surface is
    /// sRGB, so a Display P3 stream has to be converted instead of presented
    /// with its numbers unchanged.
    primaries: crate::media::YuvPrimaries,
    /// Native AVC splits a desktop frame into four independent slices. A
    /// texture that has not yet received all four has unwritten regions, so it
    /// must not be presented until every slice has contributed at least once.
    fully_initialized: bool,
    initialized_y: Vec<bool>,
    initialized_uv: Vec<bool>,
}

/// Outcome of uploading one frame to the GPU pipeline.
///
/// The three cases must stay distinct. Reporting an incomplete slice set as a
/// rendering failure produced a permanent on-screen error, and presenting the
/// half-written texture instead blanked the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadOutcome {
    /// The frame reached the presentation texture.
    Uploaded,
    /// The frame was valid but not yet displayable (fewer than four native
    /// slices on a fresh texture, or no dirty tiles). Previous content stays.
    Incomplete,
    /// The frame failed validation and was discarded.
    Invalid,
}

impl UploadOutcome {
    fn is_uploaded(self) -> bool {
        matches!(self, Self::Uploaded)
    }
}

struct UploadBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
}

pub struct RemotePipeline {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compute_pipeline: wgpu::ComputePipeline,
    interpolated_render_pipeline: wgpu::RenderPipeline,
    sharp_render_pipeline: wgpu::RenderPipeline,
    nearest_render_pipeline: wgpu::RenderPipeline,
    native_interpolated_render_pipeline: wgpu::RenderPipeline,
    native_sharp_render_pipeline: wgpu::RenderPipeline,
    native_nearest_render_pipeline: wgpu::RenderPipeline,
    compute_layout: wgpu::BindGroupLayout,
    render_layout: wgpu::BindGroupLayout,
    native_render_layout: wgpu::BindGroupLayout,
    empty_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    decoded: Option<DecodedTexture>,
    native_nv12: Option<NativeNv12Texture>,
    present_native_nv12: bool,
    records_buffer: Option<UploadBuffer>,
    payload_buffer: Option<UploadBuffer>,
    quantization_buffer: Option<UploadBuffer>,
    records_scratch: Vec<u32>,
    payload_scratch: Vec<i32>,
    quantization_scratch: Vec<u32>,
    uploaded_quantization: Option<([u16; 64], [u16; 64])>,
    uploaded_mvs_tiles: Option<TileSet>,
    mvs_bind_group: Option<wgpu::BindGroup>,
    pending_mvs_decode: Mutex<Option<u32>>,
    pending_avc_timing: Mutex<Option<crate::media::AvcFrameTiming>>,
    /// Recording control shared with the session window, and the capture state
    /// built for the take that is currently running.
    recording: Option<Arc<RecordingControl>>,
    recording_tap: Mutex<Option<RecordingTap>>,
    presented_frame: AtomicBool,
    /// Whether this session has reported a rendering failure that a later
    /// successful upload must clear.
    reported_failure: bool,
    mailbox: Option<SharedMailbox>,
    bounds: Rectangle,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
    scale_factor: f32,
}

impl std::fmt::Debug for RemotePipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemotePipeline")
            .finish_non_exhaustive()
    }
}

impl shader::Pipeline for RemotePipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ARD MVS compute bindings"),
            entries: &[
                storage_buffer_layout(0),
                storage_buffer_layout(1),
                storage_buffer_layout(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
            ],
        });
        let render_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ARD presentation bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let native_render_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ARD native NV12 presentation bindings"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ARD GPU MVS decoder"),
            source: wgpu::ShaderSource::Wgsl(include_str!("viewer_mvs.wgsl").into()),
        });
        let native_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ARD native NV12 presenter"),
            source: wgpu::ShaderSource::Wgsl(include_str!("viewer_nv12.wgsl").into()),
        });
        let compute_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ARD MVS compute pipeline layout"),
                bind_group_layouts: &[Some(&compute_layout)],
                immediate_size: 0,
            });
        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ARD MVS tile decoder"),
            layout: Some(&compute_pipeline_layout),
            module: &shader,
            entry_point: Some("decode_tiles"),
            compilation_options: Default::default(),
            cache: None,
        });
        let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ARD empty presentation group"),
            entries: &[],
        });
        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ARD presentation pipeline layout"),
                bind_group_layouts: &[Some(&empty_layout), Some(&render_layout)],
                immediate_size: 0,
            });
        let native_render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ARD native NV12 presentation pipeline layout"),
                bind_group_layouts: &[Some(&empty_layout), Some(&native_render_layout)],
                immediate_size: 0,
            });
        let empty_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ARD empty presentation bind group"),
            layout: &empty_layout,
            entries: &[],
        });
        let create_render_pipeline = |label, entry_point| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let interpolated_render_pipeline =
            create_render_pipeline("ARD interpolated presentation pipeline", "fs_interpolated");
        let sharp_render_pipeline =
            create_render_pipeline("ARD sharp presentation pipeline", "fs_sharp");
        let nearest_render_pipeline =
            create_render_pipeline("ARD nearest presentation pipeline", "fs_nearest");
        let create_native_render_pipeline = |label, entry_point| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&native_render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &native_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &native_shader,
                    entry_point: Some(entry_point),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let native_interpolated_render_pipeline = create_native_render_pipeline(
            "ARD native NV12 interpolated presentation pipeline",
            "fs_interpolated",
        );
        let native_sharp_render_pipeline = create_native_render_pipeline(
            "ARD native NV12 sharp presentation pipeline",
            "fs_sharp",
        );
        let native_nearest_render_pipeline = create_native_render_pipeline(
            "ARD native NV12 nearest presentation pipeline",
            "fs_nearest",
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ARD decoded frame sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            device: device.clone(),
            queue: queue.clone(),
            compute_pipeline,
            interpolated_render_pipeline,
            sharp_render_pipeline,
            nearest_render_pipeline,
            native_interpolated_render_pipeline,
            native_sharp_render_pipeline,
            native_nearest_render_pipeline,
            compute_layout,
            render_layout,
            native_render_layout,
            empty_bind_group,
            sampler,
            decoded: None,
            native_nv12: None,
            present_native_nv12: false,
            records_buffer: None,
            payload_buffer: None,
            quantization_buffer: None,
            records_scratch: Vec::new(),
            payload_scratch: Vec::new(),
            quantization_scratch: Vec::with_capacity(128),
            uploaded_quantization: None,
            uploaded_mvs_tiles: None,
            mvs_bind_group: None,
            pending_mvs_decode: Mutex::new(None),
            pending_avc_timing: Mutex::new(None),
            recording: None,
            recording_tap: Mutex::new(None),
            presented_frame: AtomicBool::new(false),
            reported_failure: false,
            mailbox: None,
            bounds: Rectangle::default(),
            zoom: 1.0,
            actual_size: false,
            should_interpolate: true,
            sharp_sampling: false,
            scale_factor: 1.0,
        }
    }
}

impl RemotePipeline {
    fn reset_session(&mut self) {
        self.decoded = None;
        self.native_nv12 = None;
        self.present_native_nv12 = false;
        self.records_buffer = None;
        self.payload_buffer = None;
        self.quantization_buffer = None;
        self.uploaded_quantization = None;
        self.uploaded_mvs_tiles = None;
        self.mvs_bind_group = None;
        if let Ok(mut pending) = self.pending_mvs_decode.lock() {
            *pending = None;
        }
        if let Ok(mut pending) = self.pending_avc_timing.lock() {
            *pending = None;
        }
        // The previous session's textures are gone, so a frame that was never
        // drawn must not be captured against the new one.
        self.presented_frame.store(false, Ordering::Release);
    }

    fn ensure_texture(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        if self
            .decoded
            .as_ref()
            .is_some_and(|decoded| decoded.width == width && decoded.height == height)
        {
            return false;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ARD decoded framebuffer"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let storage_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let render_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ARD presentation bind group"),
            layout: &self.render_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&storage_view),
                },
            ],
        });
        self.mvs_bind_group = None;
        self.decoded = Some(DecodedTexture {
            width,
            height,
            texture,
            storage_view,
            render_bind_group,
        });
        true
    }

    fn upload(&mut self, frame: &mut FramePacket) -> UploadOutcome {
        if let Some(native) = frame.nv12.as_ref() {
            self.upload_nv12(native)
        } else if frame.rgba.is_some() {
            if self.upload_rgba(frame) {
                UploadOutcome::Uploaded
            } else {
                UploadOutcome::Invalid
            }
        } else if self.upload_mvs(frame) {
            UploadOutcome::Uploaded
        } else {
            UploadOutcome::Incomplete
        }
    }

    fn ensure_nv12_texture(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        if self
            .native_nv12
            .as_ref()
            .is_some_and(|native| native.width == width && native.height == height)
        {
            return false;
        }
        let make_texture = |label, format, width, height| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                // `COPY_SRC` lets the recorder read the decoded planes back
                // verbatim; it costs nothing for a texture the presenter already
                // keeps on the GPU.
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let y_texture = make_texture(
            "ARD native NV12 luma",
            wgpu::TextureFormat::R8Unorm,
            width,
            height,
        );
        let uv_texture = make_texture(
            "ARD native NV12 chroma",
            wgpu::TextureFormat::Rg8Unorm,
            width.div_ceil(2),
            height.div_ceil(2),
        );
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let conversion_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ARD NV12 YCbCr conversion"),
            size: YUV_CONVERSION_UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let render_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ARD native NV12 presentation bind group"),
            layout: &self.native_render_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: conversion_buffer.as_entire_binding(),
                },
            ],
        });
        self.native_nv12 = Some(NativeNv12Texture {
            width,
            height,
            y_texture,
            uv_texture,
            conversion_buffer,
            render_bind_group,
            range: crate::media::YuvRange::Video,
            matrix: crate::media::YuvMatrix::Bt709,
            primaries: crate::media::YuvPrimaries::Bt709,
            fully_initialized: false,
            initialized_y: vec![false; height as usize],
            initialized_uv: vec![false; height.div_ceil(2) as usize],
        });
        true
    }

    fn upload_nv12(&mut self, frame: &crate::media::DecodedFrame) -> UploadOutcome {
        let width = frame.width;
        let height = frame.height;
        let uv_width = width.div_ceil(2);
        let uv_height = height.div_ceil(2);
        let Some(uv_bytes_per_row) = uv_width.checked_mul(2) else {
            return UploadOutcome::Invalid;
        };
        for update in &frame.updates {
            let pixels = &update.pixels;
            let expected_y = usize::try_from(width)
                .ok()
                .and_then(|row| row.checked_mul(pixels.height as usize));
            let expected_uv = usize::try_from(uv_bytes_per_row)
                .ok()
                .and_then(|row| row.checked_mul(pixels.height.div_ceil(2) as usize));
            if pixels.width != width
                || pixels.range != frame.range
                || pixels.matrix != frame.matrix
                || pixels.primaries != frame.primaries
                || expected_y != Some(pixels.y_plane.len())
                || expected_uv != Some(pixels.uv_plane.len())
                || update.y_origin.saturating_add(update.y_rows) > height
                || update.uv_origin.saturating_add(update.uv_rows) > uv_height
                || update.y_rows > pixels.height
                || update.uv_rows > pixels.height.div_ceil(2)
            {
                return UploadOutcome::Invalid;
            }
        }
        let recreated = self.ensure_nv12_texture(width, height);
        if !recreated && self.native_nv12.is_none() {
            return UploadOutcome::Invalid;
        }
        // Partial frames may initialize the surface across several redraws.
        // Counting updates neither proves coverage nor remembers earlier rows.
        let native = self.native_nv12.as_mut().expect("native textures exist");
        native.range = frame.range;
        native.matrix = frame.matrix;
        native.primaries = frame.primaries;
        for update in &frame.updates {
            native.initialized_y
                [update.y_origin as usize..(update.y_origin + update.y_rows) as usize]
                .fill(true);
            native.initialized_uv
                [update.uv_origin as usize..(update.uv_origin + update.uv_rows) as usize]
                .fill(true);
        }
        native.fully_initialized = native.initialized_y.iter().all(|&row| row)
            && native.initialized_uv.iter().all(|&row| row);
        *self.pending_mvs_decode.lock().expect("decode lock") = None;
        let native = self.native_nv12.as_ref().expect("native textures exist");
        self.present_native_nv12 = native.fully_initialized;
        let native = self.native_nv12.as_ref().expect("native textures exist");
        for update in &frame.updates {
            if update.y_rows != 0 {
                let y_bytes = usize::try_from(width)
                    .expect("width fits usize")
                    .checked_mul(update.y_rows as usize)
                    .expect("validated slice size");
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &native.y_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: update.y_origin,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &update.pixels.y_plane[..y_bytes],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(width),
                        rows_per_image: Some(update.y_rows),
                    },
                    wgpu::Extent3d {
                        width,
                        height: update.y_rows,
                        depth_or_array_layers: 1,
                    },
                );
            }
            if update.uv_rows != 0 {
                let uv_bytes = usize::try_from(uv_bytes_per_row)
                    .expect("chroma row fits usize")
                    .checked_mul(update.uv_rows as usize)
                    .expect("validated chroma size");
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &native.uv_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: update.uv_origin,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &update.pixels.uv_plane[..uv_bytes],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(uv_bytes_per_row),
                        rows_per_image: Some(update.uv_rows),
                    },
                    wgpu::Extent3d {
                        width: uv_width,
                        height: update.uv_rows,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        let conversion = yuv_conversion(frame.range, frame.matrix);
        self.queue.write_buffer(
            &native.conversion_buffer,
            0,
            bytemuck::cast_slice(&conversion),
        );
        if self.present_native_nv12 {
            UploadOutcome::Uploaded
        } else {
            UploadOutcome::Incomplete
        }
    }

    fn upload_rgba(&mut self, frame: &FramePacket) -> bool {
        let Some(rgba) = frame.rgba.as_deref() else {
            return false;
        };
        let width = u32::from(frame.width);
        let height = u32::from(frame.height);
        let Some(bytes_per_row) = width.checked_mul(4) else {
            return false;
        };
        let Some(expected) = usize::try_from(bytes_per_row)
            .ok()
            .and_then(|row| row.checked_mul(height as usize))
        else {
            return false;
        };
        if rgba.len() != expected {
            return false;
        }
        if !self.ensure_texture(width, height) && self.decoded.is_none() {
            return false;
        }
        *self.pending_mvs_decode.lock().expect("decode lock") = None;
        self.present_native_nv12 = false;
        // The CPU framebuffer overwrites these pixels. Coefficient equality
        // with an earlier MVS frame no longer proves the texture is current.
        self.uploaded_mvs_tiles = None;
        let decoded = self.decoded.as_ref().expect("texture exists");
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &decoded.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        true
    }

    fn upload_mvs(&mut self, frame: &mut FramePacket) -> bool {
        self.present_native_nv12 = false;
        let incoming = std::mem::replace(&mut frame.tiles, TileSet::new(0, 0, 0));
        let recreated = self.ensure_texture(u32::from(frame.width), u32::from(frame.height));
        if self.decoded.is_none() {
            return false;
        }
        let same_dimensions = self
            .uploaded_mvs_tiles
            .as_ref()
            .is_some_and(|tiles| tiles.matches_dimensions(frame.width, frame.height));
        let quantization = (frame.luminance_quantization, frame.chrominance_quantization);
        let quantization_changed =
            self.uploaded_quantization != Some(quantization) || self.quantization_buffer.is_none();
        let mut tiles = if same_dimensions {
            let mut tiles = self.uploaded_mvs_tiles.take().expect("dimensions checked");
            // prepare can run again before render consumes the pending dispatch.
            // Keep its dirty tiles until that dispatch has actually been encoded.
            if self
                .pending_mvs_decode
                .lock()
                .expect("decode lock")
                .is_none()
            {
                tiles.clear_dirty();
            }
            tiles.merge(incoming, false);
            if recreated {
                tiles.mark_all_dirty();
            }
            tiles
        } else {
            incoming
        };
        let dirty = tiles.dirty_len();
        if dirty == 0 && !recreated {
            tiles.clear_dirty();
            self.uploaded_mvs_tiles = Some(tiles);
            return false;
        }
        if dirty == 0 {
            // `ensure_texture` just replaced the presentation texture, so the
            // previous tile set no longer matches it and there is nothing new
            // to decode. Skip this frame instead of presenting a texture that
            // has never been written (which showed as a black canvas).
            self.uploaded_mvs_tiles = None;
            return false;
        }
        pack_dirty_gpu_tiles(&tiles, &mut self.records_scratch, &mut self.payload_scratch);
        let records_recreated = write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.records_buffer,
            "ARD MVS records",
            &self.records_scratch,
        );
        let payload_recreated = write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.payload_buffer,
            "ARD MVS payload",
            &self.payload_scratch,
        );
        let quantization_recreated = if quantization_changed {
            self.quantization_scratch.clear();
            self.quantization_scratch.extend(
                frame
                    .luminance_quantization
                    .iter()
                    .map(|&value| u32::from(value)),
            );
            self.quantization_scratch.extend(
                frame
                    .chrominance_quantization
                    .iter()
                    .map(|&value| u32::from(value)),
            );
            let changed = write_storage_buffer(
                &self.device,
                &self.queue,
                &mut self.quantization_buffer,
                "ARD MVS quantization",
                &self.quantization_scratch,
            );
            self.uploaded_quantization = Some(quantization);
            changed
        } else {
            false
        };
        if records_recreated
            || payload_recreated
            || quantization_recreated
            || self.mvs_bind_group.is_none()
        {
            let decoded = self.decoded.as_ref().expect("texture exists");
            self.mvs_bind_group = Some(
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ARD MVS compute bind group"),
                    layout: &self.compute_layout,
                    entries: &[
                        buffer_entry(0, &self.records_buffer.as_ref().expect("records").buffer),
                        buffer_entry(1, &self.payload_buffer.as_ref().expect("payload").buffer),
                        buffer_entry(
                            2,
                            &self
                                .quantization_buffer
                                .as_ref()
                                .expect("quantization")
                                .buffer,
                        ),
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&decoded.storage_view),
                        },
                    ],
                }),
            );
        }
        *self.pending_mvs_decode.lock().expect("decode lock") =
            Some(u32::try_from(dirty).expect("tile count fits u32"));
        self.uploaded_mvs_tiles = Some(tiles);
        true
    }

    /// Hand the frame that was just drawn to the recorder.
    ///
    /// The capture reads the same source texture the draw sampled, so an MVS
    /// frame is recorded exactly as the GPU decoded it and an AVC frame is
    /// recorded as the decoded planes, at the remote resolution — never as a
    /// second, differently scaled or filtered copy of the window.
    fn capture_recording(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(control) = self.recording.as_ref() else {
            return;
        };
        let Ok(mut slot) = self.recording_tap.lock() else {
            return;
        };
        let tap = slot.get_or_insert_with(|| RecordingTap::new(&self.device));
        if !tap.sync(control) {
            return;
        }
        // The first frame of a take is whatever is already on screen, even if
        // this redraw uploaded nothing new.
        let started = tap.take_started();
        let pending = self.presented_frame.swap(false, Ordering::AcqRel);
        if !started && !pending {
            return;
        }
        let source = if self.present_native_nv12
            && self
                .native_nv12
                .as_ref()
                .is_some_and(|native| native.fully_initialized)
        {
            let Some(native) = self.native_nv12.as_ref() else {
                return;
            };
            PresentationSource::Nv12 {
                y: &native.y_texture,
                uv: &native.uv_texture,
                width: native.width,
                height: native.height,
                range: native.range,
                matrix: native.matrix,
                primaries: native.primaries,
            }
        } else {
            let Some(decoded) = self.decoded.as_ref() else {
                return;
            };
            PresentationSource::Rgba {
                texture: &decoded.texture,
                width: decoded.width,
                height: decoded.height,
            }
        };
        tap.capture(encoder, source, Instant::now());
    }

    fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        clip_bounds: &Rectangle<u32>,
    ) {
        if let Some(workgroups) = self.pending_mvs_decode.lock().expect("decode lock").take() {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ARD GPU MVS decode"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.compute_pipeline);
            pass.set_bind_group(
                0,
                self.mvs_bind_group.as_ref().expect("MVS bind group"),
                &[],
            );
            let (workgroups_x, workgroups_y) = mvs_dispatch_size(workgroups);
            pass.dispatch_workgroups(workgroups_x, workgroups_y, 1);
        }
        let (frame_width, frame_height) = if self.present_native_nv12
            && self
                .native_nv12
                .as_ref()
                .is_some_and(|native| native.fully_initialized)
        {
            let Some(native) = &self.native_nv12 else {
                return;
            };
            (native.width, native.height)
        } else {
            let Some(decoded) = &self.decoded else {
                return;
            };
            (decoded.width, decoded.height)
        };
        let scale = self.scale_factor;
        let bounds = Rectangle::new(
            iced::Point::new(self.bounds.x * scale, self.bounds.y * scale),
            iced::Size::new(self.bounds.width * scale, self.bounds.height * scale),
        );
        let viewport = fitted_viewport(
            bounds,
            Size::new(frame_width as u16, frame_height as u16),
            self.zoom,
            self.actual_size,
        );
        if viewport.width <= 0.0 || viewport.height <= 0.0 {
            return;
        }
        // Zooming a large framebuffer on a HiDPI canvas can compute a viewport
        // larger than the device's maximum texture dimension, which wgpu rejects
        // as an invalid viewport (and, with iced's default error handling, kills
        // the frame). Shrink about the centre instead: the scissor already clips
        // to the canvas, so the visible result is unchanged.
        let viewport = clamp_viewport(viewport, self.device.limits().max_texture_dimension_2d);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ARD frame presentation"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_scissor_rect(
            clip_bounds.x,
            clip_bounds.y,
            clip_bounds.width,
            clip_bounds.height,
        );
        pass.set_viewport(
            viewport.x.round(),
            viewport.y.round(),
            viewport.width,
            viewport.height,
            0.0,
            1.0,
        );
        if self.present_native_nv12 {
            pass.set_pipeline(if self.should_interpolate {
                if self.sharp_sampling {
                    &self.native_sharp_render_pipeline
                } else {
                    &self.native_interpolated_render_pipeline
                }
            } else {
                &self.native_nearest_render_pipeline
            });
            pass.set_bind_group(0, &self.empty_bind_group, &[]);
            pass.set_bind_group(
                1,
                &self
                    .native_nv12
                    .as_ref()
                    .expect("native texture selected")
                    .render_bind_group,
                &[],
            );
        } else {
            pass.set_pipeline(if self.should_interpolate {
                if self.sharp_sampling {
                    &self.sharp_render_pipeline
                } else {
                    &self.interpolated_render_pipeline
                }
            } else {
                &self.nearest_render_pipeline
            });
            pass.set_bind_group(0, &self.empty_bind_group, &[]);
            pass.set_bind_group(
                1,
                &self
                    .decoded
                    .as_ref()
                    .expect("RGBA texture selected")
                    .render_bind_group,
                &[],
            );
        }
        pass.draw(0..3, 0..1);
        drop(pass);
        self.capture_recording(encoder);
        let timing = self
            .pending_avc_timing
            .lock()
            .ok()
            .and_then(|mut pending| pending.take());
        if let Some(timing) = timing
            && let Some(mailbox) = &self.mailbox
            && let Ok(mut mailbox) = mailbox.lock()
        {
            let scale = f64::from(viewport.width) / f64::from(frame_width);
            mailbox.record_avc_render_encoding(timing, scale);
        }
    }
}

/// Size of the presentation conversion uniform: three `vec4` rows for the
/// YCbCr-to-RGB matrix followed by an identity primaries matrix in the padded
/// three-column layout WGSL uses for `mat3x3<f32>` in uniform address space.
///
/// The primaries step stays the identity on purpose. The reference client
/// presents the decoded planes with their numbers unchanged, so a stream that
/// declares Display P3 reaches the screen as those same sRGB numbers:
///
/// * the native client's own snapshot of the take
///   (`屏幕共享图片2026年9月22日 GMT+8下午1.12.27.jpeg`, 3548x1996, sRGB) has a flat
///   background of `(51.96, 117.96, 115.96)`;
/// * the decoded planes of the same desktop are `(52.01, 117.01, 115.01)`, and
///   a recording of them carries those numbers too;
/// * rotating the planes' P3 numbers into sRGB instead produces
///   `(4.0, 120.0, 116.0)` — 48 levels of red away from what the native client
///   shows, which is exactly the whole-frame colour error being fixed here.
///
/// The recorder still tags the recorded frames with the stream's real primaries;
/// only this presentation step is pass-through.
const YUV_CONVERSION_UNIFORM_BYTES: u64 = 24 * 4;

fn yuv_conversion(range: crate::media::YuvRange, matrix: crate::media::YuvMatrix) -> [f32; 24] {
    let (kr, kb) = match matrix {
        crate::media::YuvMatrix::Bt601 => (0.299_f32, 0.114_f32),
        crate::media::YuvMatrix::Bt709 => (0.2126_f32, 0.0722_f32),
        crate::media::YuvMatrix::Bt2020 => (0.2627_f32, 0.0593_f32),
    };
    let kg = 1.0 - kr - kb;
    let (y_scale, chroma_scale, y_offset) = match range {
        crate::media::YuvRange::Video => (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0),
        crate::media::YuvRange::Full => (1.0, 1.0, 0.0),
    };
    let chroma_offset = 128.0 / 255.0;
    let red_cr = 2.0 * (1.0 - kr) * chroma_scale;
    let blue_cb = 2.0 * (1.0 - kb) * chroma_scale;
    let green_cb = -2.0 * kb * (1.0 - kb) / kg * chroma_scale;
    let green_cr = -2.0 * kr * (1.0 - kr) / kg * chroma_scale;
    [
        y_scale,
        0.0,
        red_cr,
        -y_scale * y_offset - red_cr * chroma_offset,
        y_scale,
        green_cb,
        green_cr,
        -y_scale * y_offset - (green_cb + green_cr) * chroma_offset,
        y_scale,
        blue_cb,
        0.0,
        -y_scale * y_offset - blue_cb * chroma_offset,
        1.0,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
        0.0,
    ]
}

fn storage_buffer_layout(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn write_storage_buffer<T: bytemuck::NoUninit>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    slot: &mut Option<UploadBuffer>,
    label: &str,
    values: &[T],
) -> bool {
    let bytes = bytemuck::cast_slice(values);
    let needed = u64::try_from(bytes.len())
        .expect("upload length fits u64")
        .max(4);
    let recreated = slot.as_ref().is_none_or(|upload| upload.capacity < needed);
    if recreated {
        let capacity = slot.as_ref().map_or(needed, |upload| {
            needed.max(upload.capacity.saturating_add(upload.capacity / 4))
        });
        *slot = Some(UploadBuffer {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: capacity,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            capacity,
        });
    }
    queue.write_buffer(&slot.as_ref().expect("buffer exists").buffer, 0, bytes);
    recreated
}

fn buffer_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn pack_dirty_gpu_tiles(tiles: &TileSet, records: &mut Vec<u32>, payload: &mut Vec<i32>) {
    records.clear();
    payload.clear();
    records.reserve(1 + tiles.dirty_len().saturating_mul(8));
    records.push(u32::try_from(tiles.dirty_len()).expect("tile count fits u32"));
    tiles.for_each_dirty(|update| {
        pack_one_gpu_tile(update, tiles.quantization_for(update), records, payload)
    });
    if payload.is_empty() {
        payload.push(0);
    }
}

/// Shrinks `viewport` about its centre until it fits inside `max_dimension`.
///
/// wgpu rejects a viewport wider or taller than the device's maximum texture
/// dimension. Large framebuffers zoomed on a HiDPI canvas can exceed it, so the
/// viewport is scaled down (preserving aspect) rather than being submitted
/// invalid. Clipping to the canvas is the scissor's job, so the visible result
/// is unchanged.
fn clamp_viewport(viewport: Rectangle, max_dimension: u32) -> Rectangle {
    if max_dimension == 0 {
        return viewport;
    }
    let max = max_dimension as f32;
    if viewport.width <= max && viewport.height <= max {
        return viewport;
    }
    let shrink = (max / viewport.width).min(max / viewport.height);
    let width = viewport.width * shrink;
    let height = viewport.height * shrink;
    Rectangle::new(
        iced::Point::new(
            viewport.center_x() - width / 2.0,
            viewport.center_y() - height / 2.0,
        ),
        iced::Size::new(width, height),
    )
}

fn mvs_dispatch_size(workgroups: u32) -> (u32, u32) {
    const MAX_PER_DIMENSION: u32 = 65_535;

    debug_assert!(workgroups > 0);
    let workgroups_y = workgroups.div_ceil(MAX_PER_DIMENSION);
    let workgroups_x = workgroups.div_ceil(workgroups_y);
    assert!(workgroups_x <= MAX_PER_DIMENSION && workgroups_y <= MAX_PER_DIMENSION);
    (workgroups_x, workgroups_y)
}

fn pack_one_gpu_tile(
    update: &MvsGpuTileUpdate,
    quantization: &[[u16; 64]; 2],
    records: &mut Vec<u32>,
    payload: &mut Vec<i32>,
) {
    let data_offset = payload.len() as u32;
    let (kind, color) = match &update.tile {
        MvsGpuTile::SolidYcbcr(sample) => (0, pack_bytes(*sample, 255)),
        MvsGpuTile::SolidRgba(rgba) => (1, u32::from_le_bytes(*rgba)),
        MvsGpuTile::PixelsYcbcr(samples) => {
            payload.extend(samples.iter().map(|&sample| pack_bytes(sample, 255) as i32));
            (2, 0)
        }
        MvsGpuTile::PixelsRgba(samples) => {
            payload.extend(samples.iter().map(|&rgba| u32::from_le_bytes(rgba) as i32));
            (3, 0)
        }
        MvsGpuTile::RiceDct(coefficients) => {
            for (index, component) in coefficients.iter().enumerate() {
                let table = &quantization[usize::from(index != 0)];
                payload.extend(
                    component
                        .iter()
                        .zip(table)
                        .map(|(&value, &quant)| i32::from(value) * i32::from(quant)),
                );
            }
            (5, 0)
        }
        MvsGpuTile::Dct(coefficients) => {
            for (index, component) in coefficients.iter().enumerate() {
                let table = &quantization[usize::from(index != 0)];
                payload.extend(
                    component
                        .iter()
                        .zip(table)
                        .map(|(&value, &quant)| i32::from(value) * i32::from(quant)),
                );
            }
            (4, 0)
        }
    };
    records.extend_from_slice(&[
        u32::from(update.x),
        u32::from(update.y),
        u32::from(update.width),
        u32::from(update.height),
        kind,
        data_offset,
        color,
        0,
    ]);
}

fn pack_bytes(rgb: [u8; 3], alpha: u8) -> u32 {
    u32::from_le_bytes([rgb[0], rgb[1], rgb[2], alpha])
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ard_rs::{ArdVideoQuality, MvsGpuFrame, MvsGpuTile, MvsGpuTileUpdate, PixelFormat};

    use super::{mvs_dispatch_size, remote_display, yuv_conversion};
    #[allow(unused_imports)]
    use crate::media::YuvPrimaries;
    use crate::recording::RecordingControl;
    use crate::session_runtime::{FrameMailbox, FramePacket, framebuffer_to_rgba};

    /// A control with no active take: these tests exercise rendering, not
    /// recording, so the tap stays idle.
    fn idle_recording() -> Arc<RecordingControl> {
        Arc::new(RecordingControl::new())
    }

    #[test]
    fn gpu_shader_is_valid_wgsl() {
        for source in [
            include_str!("viewer_mvs.wgsl"),
            include_str!("viewer_nv12.wgsl"),
        ] {
            let module = naga::front::wgsl::parse_str(source).expect("shader parses");
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .expect("shader validates");
        }
    }

    #[test]
    fn mvs_dispatch_spreads_large_frames_across_two_dimensions() {
        assert_eq!(mvs_dispatch_size(65_535), (65_535, 1));
        assert_eq!(mvs_dispatch_size(118_984), (59_492, 2));
    }

    #[test]
    fn bt709_video_range_conversion_maps_nominal_black_and_white() {
        let matrix = yuv_conversion(
            crate::media::YuvRange::Video,
            crate::media::YuvMatrix::Bt709,
        );
        let convert = |y: f32, cb: f32, cr: f32| {
            [0, 4, 8].map(|row| {
                matrix[row] * y + matrix[row + 1] * cb + matrix[row + 2] * cr + matrix[row + 3]
            })
        };
        for value in convert(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0) {
            assert!(value.abs() < 1.0e-5);
        }
        for value in convert(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0) {
            assert!((value - 1.0).abs() < 1.0e-5);
        }
    }

    /// End-to-end check of the presentation colour chain against the native
    /// client's own snapshot of the same desktop.
    ///
    /// The flat background of `~/Movies/ARD Viewer` decodes to full-range NV12
    /// `Y=104 U=134 V=95` while the stream declares Display P3 primaries with
    /// the sRGB transfer function. The native client's snapshot of that desktop
    /// (`屏幕共享图片2026年9月22日 GMT+8下午1.12.27.jpeg`, tagged sRGB) measures
    /// `(51.96, 117.96, 115.96)` there, and the decoded planes measure
    /// `(52.01, 117.01, 115.01)`: the reference presents the plane numbers as
    /// they are. Rotating them through the P3 matrix would instead reach the
    /// screen as `(4.0, 120.0, 116.0)`, 48 levels of red away from the
    /// reference, which is the whole-frame colour error this keeps out.
    #[test]
    fn presentation_keeps_the_planes_colour_numbers() {
        let yuv = yuv_conversion(crate::media::YuvRange::Full, crate::media::YuvMatrix::Bt709);
        let y = 104.0 / 255.0;
        let cb = 134.0 / 255.0;
        let cr = 95.0 / 255.0;
        let encoded = [0, 4, 8]
            .map(|row| yuv[row] * y + yuv[row + 1] * cb + yuv[row + 2] * cr + yuv[row + 3]);
        let srgb = encoded.map(|value| (value * 255.0).round());
        assert_eq!(srgb, [52.0, 118.0, 115.0]);
    }

    /// The presenter hands the surface linear values and the surface encodes
    /// them with the sRGB transfer function, so every 8-bit code must survive
    /// that round trip: a curve mismatch would shift the whole picture by a
    /// level against the reference client and against a recording.
    #[test]
    fn presentation_round_trip_is_exact() {
        fn linearize(encoded: f32) -> f32 {
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        }
        fn encode(linear: f32) -> f32 {
            if linear <= 0.0031308 {
                12.92 * linear
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            }
        }
        for code in 0..=255_u16 {
            let encoded = f32::from(code) / 255.0;
            let round_tripped = (encode(linearize(encoded)) * 255.0).round();
            assert_eq!(
                round_tripped,
                f32::from(code),
                "code {code} came back as {round_tripped}"
            );
        }
    }

    /// The conversion uniform's primaries block must stay the identity: a
    /// rotation there is invisible in the YCbCr coefficients and would silently
    /// move the whole picture's colour away from the reference client.
    #[test]
    fn conversion_uniform_does_not_rotate_primaries() {
        let uniform = yuv_conversion(crate::media::YuvRange::Full, crate::media::YuvMatrix::Bt709);
        assert_eq!(uniform.len(), 24);
        assert_eq!(
            &uniform[12..],
            &[
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0,
            ]
        );
    }

    /// End-to-end check of the presented colour through the real GPU pipeline.
    ///
    /// The take's flat background is full-range NV12 with `Y=104 Cb=134 Cr=95`
    /// and Display P3 primaries; the native client's snapshot of that desktop
    /// measures it as `(51.97, 118.97, 115.96)` sRGB. Rotating the planes
    /// through the P3 matrix instead reaches the screen as `(4, 120, 116)`,
    /// which is the whole-frame colour error this presentation keeps out.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a GPU and writes a visual QA snapshot to /tmp"]
    fn flat_background_reaches_the_screen_as_the_native_client_shows_it()
    -> Result<(), iced_test::Error> {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        mailbox.lock().expect("mailbox").latest = Some(FramePacket::from_nv12(
            crate::media::DecodedFrame {
                width: 2,
                height: 2,
                encoded_bytes: 6,
                range: crate::media::YuvRange::Full,
                matrix: crate::media::YuvMatrix::Bt709,
                primaries: crate::media::YuvPrimaries::P3D65,
                updates: vec![crate::media::DecodedSliceUpdate {
                    slice_index: 0,
                    y_origin: 0,
                    y_rows: 2,
                    uv_origin: 0,
                    uv_rows: 1,
                    pixels: crate::media::DecodedSlice {
                        width: 2,
                        height: 2,
                        y_plane: vec![104, 104, 104, 104],
                        uv_plane: vec![134, 95],
                        range: crate::media::YuvRange::Full,
                        matrix: crate::media::YuvMatrix::Bt709,
                        primaries: crate::media::YuvPrimaries::P3D65,
                    },
                }],
                timing: None,
            },
            ArdVideoQuality::HighPerformanceAvc,
        ));
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(320.0, 200.0),
            remote_display::<()>(mailbox, 1.0, false, true, false, idle_recording()),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        let base = "/tmp/ard-viewer-iced-nv12-flat-colour";
        assert!(snapshot.matches_image(base)?);

        let file = std::fs::File::open(format!("{base}-wgpu.png"))?;
        let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info()?;
        let mut rgba = vec![0; reader.output_buffer_size().expect("snapshot size")];
        let info = reader.next_frame(&mut rgba)?;
        let offset =
            ((info.height as usize / 2) * info.width as usize + info.width as usize / 2) * 4;
        let pixel = &rgba[offset..offset + 4];
        let (red, green, blue) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
        // The reference client's numbers for this colour, with room for the
        // snapshot's own quantisation.
        assert!(
            (red - 52).abs() <= 3 && (green - 118).abs() <= 4 && (blue - 115).abs() <= 4,
            "presented ({red}, {green}, {blue}) is not the reference client's (52, 118, 115)"
        );

        // The source is one flat colour, so every presented pixel of it must be
        // that same colour: a dither, banding, or a filter footprint would show
        // up here as more than one value, which is the "background is not a
        // solid colour" a viewer reports.
        let mut seen: Vec<(u8, u8, u8)> = Vec::new();
        for row in (info.height as usize / 4)..(info.height as usize * 3 / 4) {
            for column in (info.width as usize / 4)..(info.width as usize * 3 / 4) {
                let at = (row * info.width as usize + column) * 4;
                let value = (rgba[at], rgba[at + 1], rgba[at + 2]);
                if !seen.contains(&value) {
                    seen.push(value);
                }
            }
        }
        assert!(
            seen.len() <= 2,
            "a flat source reached the screen with {} distinct values: {:?}",
            seen.len(),
            seen
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a GPU and writes a visual QA snapshot to /tmp"]
    fn nv12_frame_renders_through_the_iced_gpu_pipeline() -> Result<(), iced_test::Error> {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        mailbox.lock().expect("mailbox").latest = Some(FramePacket::from_nv12(
            crate::media::DecodedFrame {
                width: 2,
                height: 4,
                encoded_bytes: 12,
                range: crate::media::YuvRange::Video,
                matrix: crate::media::YuvMatrix::Bt709,
                primaries: crate::media::YuvPrimaries::Bt709,
                updates: (0..4)
                    .map(|slice_index| crate::media::DecodedSliceUpdate {
                        slice_index,
                        y_origin: slice_index as u32,
                        y_rows: 1,
                        uv_origin: slice_index.min(1) as u32,
                        uv_rows: u32::from(slice_index < 2),
                        pixels: crate::media::DecodedSlice {
                            width: 2,
                            height: 1,
                            y_plane: vec![[16, 81, 145, 235][slice_index.min(3)]; 2],
                            uv_plane: vec![128, 128],
                            range: crate::media::YuvRange::Video,
                            matrix: crate::media::YuvMatrix::Bt709,
                            primaries: crate::media::YuvPrimaries::Bt709,
                        },
                    })
                    .collect(),
                timing: None,
            },
            ArdVideoQuality::HighPerformanceAvc,
        ));
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(320.0, 200.0),
            remote_display::<()>(mailbox, 1.0, false, true, false, idle_recording()),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        let snapshot_base = "/tmp/ard-viewer-iced-nv12-slice-pipeline";
        assert!(snapshot.matches_image(snapshot_base)?);

        // Do not let a pre-existing all-black baseline make this GPU test a
        // false positive. Inspect the four scaled source rows in the actual
        // wgpu snapshot and require the expected video-range luma ramp.
        let file = std::fs::File::open(format!("{snapshot_base}-wgpu.png"))?;
        let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info()?;
        let mut rgba = vec![0; reader.output_buffer_size().expect("snapshot size")];
        let info = reader.next_frame(&mut rgba)?;
        assert_eq!((info.width, info.height), (640, 400));
        let level = |y: usize| {
            let offset = (y * info.width as usize + 320) * 4;
            let pixel = &rgba[offset..offset + 4];
            assert_eq!(pixel[3], 255);
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            pixel[0]
        };
        let levels = [level(50), level(150), level(250), level(350)];
        assert!(levels[0] <= 2, "nominal black was {}", levels[0]);
        assert!(
            (70..=85).contains(&levels[1]),
            "dark gray was {}",
            levels[1]
        );
        assert!(
            (140..=160).contains(&levels[2]),
            "light gray was {}",
            levels[2]
        );
        assert!(levels[3] >= 250, "nominal white was {}", levels[3]);
        Ok(())
    }

    #[test]
    #[ignore = "requires a GPU and writes a visual QA snapshot to /tmp"]
    fn rgba_frame_renders_through_the_iced_gpu_pipeline() -> Result<(), iced_test::Error> {
        let mut framebuffer =
            ard_rs::Framebuffer::new_native(2, 2, PixelFormat::XRGB8888).expect("test framebuffer");
        framebuffer
            .pixels_mut()
            .copy_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
        let mut rgba = Vec::new();
        assert!(framebuffer_to_rgba(&framebuffer, &mut rgba));
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        mailbox.lock().expect("mailbox").latest =
            Some(FramePacket::from_rgba(2, 2, rgba, ArdVideoQuality::Full));
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(320.0, 200.0),
            remote_display::<()>(mailbox, 1.0, false, false, false, idle_recording()),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        let pixels = snapshot_pixels(snapshot, "rgba");
        for (x, y, expected) in [
            (270, 100, [255, 0, 0, 255]),
            (370, 100, [0, 255, 0, 255]),
            (270, 300, [0, 0, 255, 255]),
            (370, 300, [255, 255, 255, 255]),
        ] {
            let pixel = &pixels[(y * 640 + x) * 4..(y * 640 + x + 1) * 4];
            // Sample within each nearest-neighbour colour quadrant.
            assert_eq!(pixel, expected);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a GPU and writes a visual QA snapshot to /tmp"]
    fn mvs_frame_decodes_on_gpu_inside_iced() -> Result<(), iced_test::Error> {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        mailbox.lock().expect("mailbox").latest = Some(FramePacket::from_mvs(
            MvsGpuFrame {
                framebuffer_width: 8,
                framebuffer_height: 8,
                luminance_quantization: [1; 64],
                chrominance_quantization: [1; 64],
                tiles: vec![MvsGpuTileUpdate {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                    tile: MvsGpuTile::SolidRgba([24, 136, 232, 255]),
                }],
            },
            ArdVideoQuality::Adaptive,
        ));
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(320.0, 200.0),
            remote_display::<()>(mailbox, 1.0, false, true, false, idle_recording()),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        assert!(snapshot.matches_image("/tmp/ard-viewer-iced-mvs-pipeline")?);
        Ok(())
    }
    fn snapshot_pixels(snapshot: iced_test::simulator::Snapshot, name: &str) -> Vec<u8> {
        let base = std::env::temp_dir().join(format!("ard-pixels-{}-{name}", std::process::id()));
        let path = base.with_file_name(format!(
            "{}-wgpu.png",
            base.file_name().unwrap().to_string_lossy()
        ));
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        assert!(snapshot.matches_image(&base).unwrap());
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut pixels).unwrap();
        pixels
    }

    fn dct_frame(x: u16, quant: u16) -> MvsGpuFrame {
        let mut coefficients = [[0; 64]; 3];
        coefficients[0][0] = 8;
        MvsGpuFrame {
            framebuffer_width: 16,
            framebuffer_height: 8,
            luminance_quantization: [quant; 64],
            chrominance_quantization: [1; 64],
            tiles: vec![MvsGpuTileUpdate {
                x,
                y: 0,
                width: 8,
                height: 8,
                tile: MvsGpuTile::Dct(Arc::new(coefficients)),
            }],
        }
    }

    #[test]
    fn mvs_coalescing_preserves_each_tiles_quantization() {
        let mut first = FramePacket::from_mvs(dct_frame(0, 8), ArdVideoQuality::Adaptive);
        let second = FramePacket::from_mvs(dct_frame(8, 24), ArdVideoQuality::Adaptive);
        first.tiles.merge(second.tiles, false);
        let mut records = Vec::new();
        let mut payload = Vec::new();
        super::pack_dirty_gpu_tiles(&first.tiles, &mut records, &mut payload);
        assert_eq!(records[0], 2);
        for record in records[1..].chunks_exact(8) {
            assert_eq!(
                payload[record[5] as usize],
                if record[0] == 0 { 64 } else { 192 }
            );
        }
        first.tiles.clear_dirty();
        first.tiles.merge(
            FramePacket::from_mvs(dct_frame(0, 16), ArdVideoQuality::Adaptive).tiles,
            false,
        );
        assert_eq!(
            first.tiles.dirty_len(),
            1,
            "same coefficients with a new quantizer must be redrawn"
        );
    }

    #[test]
    #[ignore = "requires a GPU; exports and checks actual presentation pixels"]
    fn incremental_mvs_quantization_pixels_match_expected_luma() -> Result<(), iced_test::Error> {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        let mut initial = FramePacket::from_mvs(dct_frame(0, 8), ArdVideoQuality::Adaptive);
        initial.tiles.merge(
            FramePacket::from_mvs(dct_frame(8, 24), ArdVideoQuality::Adaptive).tiles,
            false,
        );
        mailbox.lock().unwrap().latest = Some(initial);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(8.0, 4.0),
            remote_display::<()>(
                Arc::clone(&mailbox),
                1.0,
                false,
                false,
                false,
                idle_recording(),
            ),
        );
        let pixels = snapshot_pixels(ui.snapshot(&iced::Theme::Dark)?, "mvs-quantization");
        assert_eq!(pixels.len(), 16 * 8 * 4);
        for y in 0..8 {
            for x in 0..16 {
                let expected = if x < 8 { 136 } else { 152 };
                assert_eq!(
                    &pixels[(y * 16 + x) * 4..(y * 16 + x + 1) * 4],
                    &[expected, expected, expected, 255]
                );
            }
        }
        mailbox.lock().unwrap().latest = Some(FramePacket::from_mvs(
            dct_frame(0, 16),
            ArdVideoQuality::Adaptive,
        ));
        let pixels = snapshot_pixels(ui.snapshot(&iced::Theme::Dark)?, "mvs-incremental");
        for y in 0..8 {
            for x in 0..16 {
                let expected = if x < 8 { 144 } else { 152 };
                assert_eq!(
                    &pixels[(y * 16 + x) * 4..(y * 16 + x + 1) * 4],
                    &[expected, expected, expected, 255]
                );
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a GPU; verifies switching CPU and GPU frame updates"]
    fn rgba_overwrite_invalidates_mvs_pixel_cache() -> Result<(), iced_test::Error> {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        let mut initial = FramePacket::from_mvs(dct_frame(0, 8), ArdVideoQuality::Adaptive);
        initial.tiles.merge(
            FramePacket::from_mvs(dct_frame(8, 8), ArdVideoQuality::Adaptive).tiles,
            false,
        );
        mailbox.lock().unwrap().latest = Some(initial);
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(8.0, 4.0),
            remote_display::<()>(
                Arc::clone(&mailbox),
                1.0,
                false,
                false,
                false,
                idle_recording(),
            ),
        );
        let _ = ui.snapshot(&iced::Theme::Dark)?;
        mailbox.lock().unwrap().latest = Some(FramePacket::from_rgba(
            16,
            8,
            [0, 255, 0, 255].repeat(128),
            ArdVideoQuality::Adaptive,
        ));
        let _ = ui.snapshot(&iced::Theme::Dark)?;
        mailbox.lock().unwrap().latest = Some(FramePacket::from_mvs(
            dct_frame(0, 8),
            ArdVideoQuality::Adaptive,
        ));
        let pixels = snapshot_pixels(ui.snapshot(&iced::Theme::Dark)?, "mvs-after-rgba");
        for y in 0..8 {
            for x in 0..16 {
                let expected = if x < 8 {
                    [136, 136, 136, 255]
                } else {
                    [0, 255, 0, 255]
                };
                assert_eq!(&pixels[(y * 16 + x) * 4..(y * 16 + x + 1) * 4], &expected);
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a GPU; exports and checks actual presentation pixels"]
    fn nv12_partial_initialization_preserves_all_rows() -> Result<(), iced_test::Error> {
        use crate::media::{DecodedFrame, DecodedSlice, DecodedSliceUpdate, YuvMatrix, YuvRange};
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            iced::Size::new(4.0, 4.0),
            remote_display::<()>(
                Arc::clone(&mailbox),
                1.0,
                false,
                false,
                false,
                idle_recording(),
            ),
        );
        for index in 0..4 {
            mailbox.lock().unwrap().latest = Some(FramePacket::from_nv12(
                DecodedFrame {
                    width: 8,
                    height: 8,
                    encoded_bytes: 0,
                    range: YuvRange::Full,
                    matrix: YuvMatrix::Bt709,
                    primaries: YuvPrimaries::Bt709,
                    timing: None,
                    updates: vec![DecodedSliceUpdate {
                        slice_index: index,
                        y_origin: index as u32 * 2,
                        y_rows: 2,
                        uv_origin: index as u32,
                        uv_rows: 1,
                        pixels: DecodedSlice {
                            width: 8,
                            height: 2,
                            y_plane: vec![32 + index as u8 * 48; 16],
                            uv_plane: vec![128; 8],
                            range: YuvRange::Full,
                            matrix: YuvMatrix::Bt709,
                            primaries: YuvPrimaries::Bt709,
                        },
                    }],
                },
                ArdVideoQuality::HighPerformanceHevc,
            ));
            let pixels = snapshot_pixels(
                ui.snapshot(&iced::Theme::Dark)?,
                &format!("nv12-partial-{index}"),
            );
            if index == 3 {
                assert_eq!(pixels.len(), 8 * 8 * 4);
                for y in 0..8 {
                    for x in 0..8 {
                        let expected = 32 + (y / 2) as u8 * 48;
                        let pixel = &pixels[(y * 8 + x) * 4..(y * 8 + x + 1) * 4];
                        assert!(
                            pixel[..3].iter().all(|&v| v.abs_diff(expected) <= 1),
                            "row {y}: {pixel:?}, expected {expected}"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}
