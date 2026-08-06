use std::sync::{Arc, Mutex};

use ard_rs::{MvsGpuTile, MvsGpuTileUpdate};
use iced::widget::shader::{self, Program};
use iced::{Element, Fill, Rectangle, Size};

use crate::session_runtime::{FramePacket, SessionEvent, SharedMailbox, TileSet, fitted_viewport};

#[derive(Debug, Clone)]
pub struct RemoteProgram {
    mailbox: SharedMailbox,
    zoom: f32,
}

impl RemoteProgram {
    pub fn new(mailbox: SharedMailbox, zoom: f32) -> Self {
        Self { mailbox, zoom }
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
        }
    }
}

pub fn remote_display<Message: 'static>(
    mailbox: SharedMailbox,
    zoom: f32,
) -> Element<'static, Message> {
    shader::Shader::new(RemoteProgram::new(mailbox, zoom))
        .width(Fill)
        .height(Fill)
        .into()
}

#[derive(Debug)]
pub struct RemotePrimitive {
    mailbox: SharedMailbox,
    bounds: Rectangle,
    zoom: f32,
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
        pipeline.used_this_frame = true;
        let changed_session = pipeline
            .mailbox
            .as_ref()
            .is_none_or(|mailbox| !Arc::ptr_eq(mailbox, &self.mailbox));
        if changed_session {
            pipeline.reset_session();
        }
        pipeline.mailbox = Some(Arc::clone(&self.mailbox));
        pipeline.zoom = self.zoom;
        pipeline.scale_factor = viewport.scale_factor();
        pipeline.bounds = self.bounds;

        let frame = self
            .mailbox
            .lock()
            .ok()
            .and_then(|mut mailbox| mailbox.latest.take());
        let Some(mut frame) = frame else { return };
        let uploaded = pipeline.upload(&mut frame);
        if let Some(buffer) = frame.rgba.take()
            && let Ok(mut mailbox) = self.mailbox.lock()
        {
            mailbox.recycle_rgba(buffer);
        }
        if !uploaded {
            pipeline.report_error("远程帧 GPU 上传失败");
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

struct UploadBuffer {
    buffer: wgpu::Buffer,
    capacity: u64,
}

pub struct RemotePipeline {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compute_pipeline: wgpu::ComputePipeline,
    render_pipeline: wgpu::RenderPipeline,
    compute_layout: wgpu::BindGroupLayout,
    render_layout: wgpu::BindGroupLayout,
    empty_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    decoded: Option<DecodedTexture>,
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
    mailbox: Option<SharedMailbox>,
    bounds: Rectangle,
    zoom: f32,
    scale_factor: f32,
    used_this_frame: bool,
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
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ARD GPU MVS decoder"),
            source: wgpu::ShaderSource::Wgsl(include_str!("viewer_mvs.wgsl").into()),
        });
        let compute_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ARD MVS compute pipeline layout"),
                bind_group_layouts: &[&compute_layout],
                push_constant_ranges: &[],
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
                bind_group_layouts: &[&empty_layout, &render_layout],
                push_constant_ranges: &[],
            });
        let empty_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ARD empty presentation bind group"),
            layout: &empty_layout,
            entries: &[],
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ARD presentation pipeline"),
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
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
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
            render_pipeline,
            compute_layout,
            render_layout,
            empty_bind_group,
            sampler,
            decoded: None,
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
            mailbox: None,
            bounds: Rectangle::default(),
            zoom: 1.0,
            scale_factor: 1.0,
            used_this_frame: false,
        }
    }

    fn trim(&mut self) {
        if self.used_this_frame {
            self.used_this_frame = false;
            return;
        }
        self.reset_session();
        self.mailbox = None;
    }
}

impl RemotePipeline {
    fn reset_session(&mut self) {
        self.decoded = None;
        self.records_buffer = None;
        self.payload_buffer = None;
        self.quantization_buffer = None;
        self.uploaded_quantization = None;
        self.uploaded_mvs_tiles = None;
        self.mvs_bind_group = None;
        if let Ok(mut pending) = self.pending_mvs_decode.lock() {
            *pending = None;
        }
    }

    fn report_error(&self, error: &str) {
        if let Some(mailbox) = &self.mailbox
            && let Ok(mut mailbox) = mailbox.lock()
        {
            mailbox.push_event(SessionEvent::RenderFailed(error.to_owned()));
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
        if frame.rgba.is_some() {
            self.upload_rgba(frame)
        } else {
            self.upload_mvs(frame)
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
            return true;
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
            pass.dispatch_workgroups(workgroups, 1, 1);
        }
        let Some(decoded) = &self.decoded else { return };
        let scale = self.scale_factor;
        let bounds = Rectangle::new(
            iced::Point::new(self.bounds.x * scale, self.bounds.y * scale),
            iced::Size::new(self.bounds.width * scale, self.bounds.height * scale),
        );
        let viewport = fitted_viewport(
            bounds,
            Size::new(decoded.width as u16, decoded.height as u16),
            self.zoom,
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
        });
        pass.set_scissor_rect(
            clip_bounds.x,
            clip_bounds.y,
            clip_bounds.width,
            clip_bounds.height,
        );
        pass.set_viewport(
            viewport.x,
            viewport.y,
            viewport.width,
            viewport.height,
            0.0,
            1.0,
        );
        pass.set_pipeline(&self.render_pipeline);
        pass.set_bind_group(0, &self.empty_bind_group, &[]);
        pass.set_bind_group(1, &decoded.render_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
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
    records.reserve(tiles.dirty_len().saturating_mul(8));
    tiles.for_each_dirty(|update| pack_one_gpu_tile(update, records, payload));
    if payload.is_empty() {
        payload.push(0);
    }
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

    use super::remote_display;
    use crate::session_runtime::{FrameMailbox, FramePacket, framebuffer_to_rgba};

    #[test]
    fn gpu_shader_is_valid_wgsl() {
        let module =
            naga::front::wgsl::parse_str(include_str!("viewer_mvs.wgsl")).expect("shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("shader validates");
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
            remote_display::<()>(mailbox, 1.0),
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
            remote_display::<()>(mailbox, 1.0),
        );
        let snapshot = ui.snapshot(&iced::Theme::Dark)?;
        assert!(snapshot.matches_image("/tmp/ard-viewer-iced-mvs-pipeline")?);
        Ok(())
    }
}
