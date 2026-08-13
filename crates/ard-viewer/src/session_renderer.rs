use std::sync::{Arc, Mutex};

use ard_rs::{MvsGpuTile, MvsGpuTileUpdate};
use iced::widget::shader::{self, Program};
use iced::{Element, Fill, Rectangle, Size};

use crate::session_runtime::{FramePacket, SessionEvent, SharedMailbox, TileSet, fitted_viewport};

#[derive(Debug, Clone)]
pub struct RemoteProgram {
    mailbox: SharedMailbox,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
}

impl RemoteProgram {
    pub fn new(
        mailbox: SharedMailbox,
        zoom: f32,
        actual_size: bool,
        should_interpolate: bool,
        sharp_sampling: bool,
    ) -> Self {
        Self {
            mailbox,
            zoom,
            actual_size,
            should_interpolate,
            sharp_sampling,
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
        }
    }
}

pub fn remote_display<Message: 'static>(
    mailbox: SharedMailbox,
    zoom: f32,
    actual_size: bool,
    should_interpolate: bool,
    sharp_sampling: bool,
) -> Element<'static, Message> {
    shader::Shader::new(RemoteProgram::new(
        mailbox,
        zoom,
        actual_size,
        should_interpolate,
        sharp_sampling,
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
        let native_upload = frame.nv12.is_some();
        let avc_timing = frame.nv12.as_ref().and_then(|frame| frame.timing);
        let uploaded = pipeline.upload(&mut frame);
        if let Ok(mut pending) = pipeline.pending_avc_timing.lock() {
            *pending = uploaded.then_some(avc_timing).flatten();
        }
        if native_upload
            && !uploaded
            && let Ok(mut mailbox) = self.mailbox.lock()
        {
            mailbox.push_event(SessionEvent::RenderFailed(
                "原生 NV12 帧未通过 GPU 纹理布局校验".into(),
            ));
        }
        if let Some(buffer) = frame.rgba.take()
            && let Ok(mut mailbox) = self.mailbox.lock()
        {
            mailbox.recycle_rgba(buffer);
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

    fn upload(&mut self, frame: &mut FramePacket) -> bool {
        if let Some(native) = frame.nv12.as_ref() {
            self.upload_nv12(native)
        } else if frame.rgba.is_some() {
            self.upload_rgba(frame)
        } else {
            self.upload_mvs(frame)
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
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
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
            size: 48,
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
        });
        true
    }

    fn upload_nv12(&mut self, frame: &crate::media::DecodedFrame) -> bool {
        let width = frame.width;
        let height = frame.height;
        let uv_width = width.div_ceil(2);
        let uv_height = height.div_ceil(2);
        let Some(uv_bytes_per_row) = uv_width.checked_mul(2) else {
            return false;
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
                || expected_y != Some(pixels.y_plane.len())
                || expected_uv != Some(pixels.uv_plane.len())
                || update.y_origin.saturating_add(update.y_rows) > height
                || update.uv_origin.saturating_add(update.uv_rows) > uv_height
                || update.y_rows > pixels.height
                || update.uv_rows > pixels.height.div_ceil(2)
            {
                return false;
            }
        }
        let recreated = self.ensure_nv12_texture(width, height);
        if !recreated && self.native_nv12.is_none() {
            return false;
        }
        if recreated && frame.updates.len() < 4 {
            // A fresh texture must be initialized by all four native desktop
            // slices. The compositor guarantees this after startup, loss, or
            // a dimension change.
            self.native_nv12 = None;
            return false;
        }
        *self.pending_mvs_decode.lock().expect("decode lock") = None;
        self.present_native_nv12 = true;
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
        true
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
            tiles.merge(incoming, recreated || quantization_changed);
            tiles
        } else {
            incoming
        };
        let dirty = tiles.dirty_len();
        if dirty == 0 {
            tiles.clear_dirty();
            self.uploaded_mvs_tiles = Some(tiles);
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
        tiles.clear_dirty();
        self.uploaded_mvs_tiles = Some(tiles);
        true
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
        let (frame_width, frame_height) = if self.present_native_nv12 {
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

fn yuv_conversion(range: crate::media::YuvRange, matrix: crate::media::YuvMatrix) -> [f32; 12] {
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
    tiles.for_each_dirty(|update| pack_one_gpu_tile(update, records, payload));
    if payload.is_empty() {
        payload.push(0);
    }
}

fn mvs_dispatch_size(workgroups: u32) -> (u32, u32) {
    const MAX_PER_DIMENSION: u32 = 65_535;

    debug_assert!(workgroups > 0);
    let workgroups_y = workgroups.div_ceil(MAX_PER_DIMENSION);
    let workgroups_x = workgroups.div_ceil(workgroups_y);
    assert!(workgroups_x <= MAX_PER_DIMENSION && workgroups_y <= MAX_PER_DIMENSION);
    (workgroups_x, workgroups_y)
}

fn pack_one_gpu_tile(update: &MvsGpuTileUpdate, records: &mut Vec<u32>, payload: &mut Vec<i32>) {
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
            for component in coefficients.iter() {
                payload.extend(component.iter().map(|&value| i32::from(value)));
            }
            (5, 0)
        }
        MvsGpuTile::Dct(coefficients) => {
            for component in coefficients.iter() {
                payload.extend(component.iter().map(|&value| i32::from(value)));
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
    use crate::session_runtime::{FrameMailbox, FramePacket, framebuffer_to_rgba};

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
            remote_display::<()>(mailbox, 1.0, false, true, false),
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
            remote_display::<()>(mailbox, 1.0, false, true, false),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        assert!(snapshot.matches_image("/tmp/ard-viewer-iced-rgba-pipeline")?);
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
            remote_display::<()>(mailbox, 1.0, false, true, false),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        assert!(snapshot.matches_image("/tmp/ard-viewer-iced-mvs-pipeline")?);
        Ok(())
    }
}
