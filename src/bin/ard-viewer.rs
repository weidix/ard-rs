#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::env;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ard_rs::{
    ArdClient, ArdClientConfig, ArdVideoQuality, MvsGpuFrame, MvsGpuTile, MvsGpuTileUpdate,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

const WINDOW_TITLE: &str = "ard-rs Viewer";

#[derive(Debug)]
enum ViewerEvent {
    FrameReady,
    Status(String),
    Connected(String),
}

struct FramePacket {
    width: u16,
    height: u16,
    quality: ArdVideoQuality,
    luminance_quantization: [u16; 64],
    chrominance_quantization: [u16; 64],
    tiles: TileSet,
    rgba: Option<Vec<u8>>,
}

const EMPTY_TILE_SLOT: u32 = u32::MAX;
const MAX_DENSE_TILE_SLOTS: usize = 1_048_576;

/// Coalesces MVS updates without hashing every 8x8 coordinate. Real ARD
/// framebuffers are small enough for the dense index and avoid the allocator
/// and rehash traffic of the old per-frame HashMap. Keep a sparse fallback for
/// unusually large protocol dimensions so a malicious size cannot force a
/// large slot table.
struct TileSet {
    tiles_wide: usize,
    storage: TileStorage,
}

enum TileStorage {
    Dense {
        slots: Vec<u32>,
        tiles: Vec<MvsGpuTileUpdate>,
    },
    Sparse(HashMap<(u16, u16), MvsGpuTileUpdate>),
}

enum TileSetIter<'a> {
    Dense(slice::Iter<'a, MvsGpuTileUpdate>),
    Sparse(std::collections::hash_map::Values<'a, (u16, u16), MvsGpuTileUpdate>),
}

impl<'a> Iterator for TileSetIter<'a> {
    type Item = &'a MvsGpuTileUpdate;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dense(iter) => iter.next(),
            Self::Sparse(iter) => iter.next(),
        }
    }
}

impl TileSet {
    fn new(width: u16, height: u16, expected_tiles: usize) -> Self {
        let tiles_wide = usize::from(width).div_ceil(8);
        let tiles_high = usize::from(height).div_ceil(8);
        let tile_count = tiles_wide.saturating_mul(tiles_high);
        if tile_count <= MAX_DENSE_TILE_SLOTS {
            Self {
                tiles_wide,
                storage: TileStorage::Dense {
                    slots: vec![EMPTY_TILE_SLOT; tile_count],
                    tiles: Vec::with_capacity(expected_tiles),
                },
            }
        } else {
            Self {
                tiles_wide,
                storage: TileStorage::Sparse(HashMap::with_capacity(expected_tiles)),
            }
        }
    }

    fn insert(&mut self, update: MvsGpuTileUpdate) {
        match &mut self.storage {
            TileStorage::Dense { slots, tiles } => {
                let slot = (usize::from(update.y) / 8)
                    .saturating_mul(self.tiles_wide)
                    .saturating_add(usize::from(update.x) / 8);
                if let Some(position) = slots.get_mut(slot) {
                    if *position == EMPTY_TILE_SLOT {
                        *position = u32::try_from(tiles.len()).expect("MVS tile count fits u32");
                        tiles.push(update);
                    } else {
                        let current = &tiles[*position as usize];
                        if current.x != update.x
                            || current.y != update.y
                            || current.width != update.width
                            || current.height != update.height
                            || !same_mvs_tile(&current.tile, &update.tile)
                        {
                            tiles[*position as usize] = update;
                        }
                    }
                }
            }
            TileStorage::Sparse(tiles) => {
                tiles.insert((update.x, update.y), update);
            }
        }
    }

    fn len(&self) -> usize {
        match &self.storage {
            TileStorage::Dense { tiles, .. } => tiles.len(),
            TileStorage::Sparse(tiles) => tiles.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> TileSetIter<'_> {
        match &self.storage {
            TileStorage::Dense { tiles, .. } => TileSetIter::Dense(tiles.iter()),
            TileStorage::Sparse(tiles) => TileSetIter::Sparse(tiles.values()),
        }
    }
}

fn same_mvs_tile(left: &MvsGpuTile, right: &MvsGpuTile) -> bool {
    match (left, right) {
        (MvsGpuTile::SolidYcbcr(left), MvsGpuTile::SolidYcbcr(right)) => left == right,
        (MvsGpuTile::SolidRgba(left), MvsGpuTile::SolidRgba(right)) => left == right,
        (MvsGpuTile::PixelsYcbcr(left), MvsGpuTile::PixelsYcbcr(right)) => left == right,
        (MvsGpuTile::PixelsRgba(left), MvsGpuTile::PixelsRgba(right)) => left == right,
        (MvsGpuTile::RiceDct(left), MvsGpuTile::RiceDct(right))
        | (MvsGpuTile::Dct(left), MvsGpuTile::Dct(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

impl FramePacket {
    fn from_mvs(frame: MvsGpuFrame, quality: ArdVideoQuality) -> Self {
        let mut packet = Self {
            width: frame.framebuffer_width,
            height: frame.framebuffer_height,
            quality,
            luminance_quantization: frame.luminance_quantization,
            chrominance_quantization: frame.chrominance_quantization,
            tiles: TileSet::new(
                frame.framebuffer_width,
                frame.framebuffer_height,
                frame.tiles.len(),
            ),
            rgba: None,
        };
        packet.merge_mvs(frame);
        packet
    }

    fn from_rgba(width: u16, height: u16, rgba: Vec<u8>, quality: ArdVideoQuality) -> Self {
        Self {
            width,
            height,
            quality,
            luminance_quantization: [0; 64],
            chrominance_quantization: [0; 64],
            tiles: TileSet::new(width, height, 0),
            rgba: Some(rgba),
        }
    }

    fn merge_mvs(&mut self, frame: MvsGpuFrame) {
        self.width = frame.framebuffer_width;
        self.height = frame.framebuffer_height;
        self.luminance_quantization = frame.luminance_quantization;
        self.chrominance_quantization = frame.chrominance_quantization;
        for tile in frame.tiles {
            self.tiles.insert(tile);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StreamRates {
    updates_per_second: f64,
    megabits_per_second: f64,
}

struct RateMeter {
    window_started: Instant,
    framebuffer_updates: usize,
    wire_bytes: usize,
}

impl RateMeter {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            framebuffer_updates: 0,
            wire_bytes: 0,
        }
    }

    fn record(&mut self, framebuffer_updates: usize, wire_bytes: usize) -> Option<StreamRates> {
        self.framebuffer_updates = self.framebuffer_updates.saturating_add(framebuffer_updates);
        self.wire_bytes = self.wire_bytes.saturating_add(wire_bytes);
        let elapsed = self.window_started.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let seconds = elapsed.as_secs_f64();
            let current = StreamRates {
                updates_per_second: self.framebuffer_updates as f64 / seconds,
                megabits_per_second: self.wire_bytes as f64 * 8.0 / seconds / 1_000_000.0,
            };
            self.window_started = Instant::now();
            self.framebuffer_updates = 0;
            self.wire_bytes = 0;
            return Some(current);
        }
        None
    }
}

#[derive(Default)]
struct FrameMailbox {
    latest: Option<FramePacket>,
    rgba_pool: Vec<Vec<u8>>,
}

type SharedFrameMailbox = Arc<Mutex<FrameMailbox>>;

struct ViewerApp {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    mailbox: SharedFrameMailbox,
    frame_event_pending: Arc<AtomicBool>,
    presentation_meter: PresentationMeter,
    redraw_pending: bool,
    status: String,
    server_name: Option<String>,
}

struct PresentationMeter {
    window_started: Instant,
    presented_frames: usize,
}

impl PresentationMeter {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            presented_frames: 0,
        }
    }

    fn record(&mut self) {
        self.presented_frames = self.presented_frames.saturating_add(1);
        let elapsed = self.window_started.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let frames_per_second = self.presented_frames as f64 / elapsed.as_secs_f64();
            eprintln!("ARD display: {frames_per_second:.1} presents/s");
            self.window_started = Instant::now();
            self.presented_frames = 0;
        }
    }
}

impl ViewerApp {
    fn new(mailbox: SharedFrameMailbox, frame_event_pending: Arc<AtomicBool>) -> Self {
        Self {
            window: None,
            renderer: None,
            mailbox,
            frame_event_pending,
            presentation_meter: PresentationMeter::new(),
            redraw_pending: false,
            status: "正在连接…".to_owned(),
            server_name: None,
        }
    }

    fn update_title(&self, frame: Option<&FramePacket>) {
        let Some(window) = &self.window else { return };
        let server = self.server_name.as_deref().unwrap_or("ARD");
        let title = if let Some(frame) = frame {
            let decoder = if frame.rgba.is_some() {
                "RGBA"
            } else {
                "GPU MVS"
            };
            // Do not put changing frame counters or rates in the title. The
            // title is part of the captured desktop when the viewer connects
            // to the local ARD server, and changing it per frame creates a
            // self-sustaining screen-update feedback loop.
            format!(
                "{server} — {}×{} — {} — {decoder}",
                frame.width,
                frame.height,
                frame.quality.label()
            )
        } else {
            format!("{WINDOW_TITLE} — {}", self.status)
        };
        window.set_title(&title);
    }
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title(format!("{WINDOW_TITLE} — {}", self.status))
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(480.0, 300.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("无法创建窗口：{error}");
                event_loop.exit();
                return;
            }
        };
        match pollster::block_on(Renderer::new(window.clone())) {
            Ok(renderer) => {
                self.renderer = Some(renderer);
                self.window = Some(window);
                self.process_latest_frame();
            }
            Err(error) => {
                eprintln!("GPU 初始化失败：{error}");
                self.status = format!("GPU 初始化失败：{error}");
                self.update_title(None);
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::FrameReady => {
                self.process_latest_frame();
            }
            ViewerEvent::Status(status) => {
                self.status = status;
                self.update_title(None);
            }
            ViewerEvent::Connected(server_name) => {
                self.server_name = Some(server_name);
                self.status = "已连接，等待首帧…".to_owned();
                self.update_title(None);
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.window.as_ref().map(|window| window.id()) != Some(window_id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.redraw_pending = false;
                if let Some(renderer) = &mut self.renderer {
                    match renderer.render() {
                        RenderResult::Presented => self.presentation_meter.record(),
                        RenderResult::Retry => {
                            self.redraw_pending = true;
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                        RenderResult::Skipped => {}
                    }
                }
                if !self.redraw_pending {
                    self.process_latest_frame();
                }
            }
            _ => {}
        }
    }
}

impl ViewerApp {
    fn process_latest_frame(&mut self) {
        // A user event can be delivered before `resumed` has initialized the
        // renderer. Leave the mailbox flag set so `resumed` can consume the
        // frame after GPU initialization instead of dropping the first frame.
        if self.renderer.is_none() || self.redraw_pending {
            return;
        }
        let frame = {
            let mut mailbox = self.mailbox.lock().expect("frame mailbox poisoned");
            let frame = mailbox.latest.take();
            if frame.is_some() {
                self.frame_event_pending.store(false, Ordering::Release);
            }
            frame
        };
        let Some(frame) = frame else { return };
        if let Some(renderer) = &mut self.renderer {
            renderer.upload(&frame);
        }
        if self.status != "正在查看" {
            self.status = "正在查看".to_owned();
            self.update_title(Some(&frame));
        }
        if let Some(window) = &self.window {
            self.redraw_pending = true;
            window.request_redraw();
        }
        let mut mailbox = self.mailbox.lock().expect("frame mailbox poisoned");
        if let Some(buffer) = frame.rgba
            && mailbox.rgba_pool.len() < 3
        {
            mailbox.rgba_pool.push(buffer);
        }
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

struct PendingMvsDecode {
    bind_group: wgpu::BindGroup,
    workgroups: u32,
}

struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    compute_pipeline: wgpu::ComputePipeline,
    render_pipeline: wgpu::RenderPipeline,
    compute_layout: wgpu::BindGroupLayout,
    render_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    decoded: Option<DecodedTexture>,
    records_buffer: Option<UploadBuffer>,
    payload_buffer: Option<UploadBuffer>,
    quantization_buffer: Option<UploadBuffer>,
    pending_mvs_decode: Option<PendingMvsDecode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderResult {
    Presented,
    Retry,
    Skipped,
}

impl Renderer {
    async fn new(window: Arc<Window>) -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| error.to_string())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ard-rs GPU MVS device"),
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        device.on_uncaptured_error(Arc::new(|error| {
            eprintln!("wgpu uncaptured error: {error}");
        }));
        device.set_device_lost_callback(|reason, message| {
            eprintln!("wgpu device lost ({reason:?}): {message}");
        });
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "GPU does not support this window surface".to_owned())?;
        let surface_capabilities = surface.get_capabilities(&adapter);
        if let Some(srgb_format) = surface_capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
        {
            config.format = srgb_format;
        }
        // A remote framebuffer is a presentation stream, not an uncapped
        // compute benchmark. Vsync bounds the number of in-flight surface
        // textures and prevents a fast ARD update stream from building an
        // unbounded Metal command backlog while the window can only display
        // one image per refresh.
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MVS compute bindings"),
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
            label: Some("MVS presentation bindings"),
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
            label: Some("GPU MVS decoder"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../viewer_mvs.wgsl").into()),
        });
        let compute_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("MVS compute pipeline layout"),
                bind_group_layouts: &[Some(&compute_layout)],
                immediate_size: 0,
            });
        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("MVS tile decoder"),
            layout: Some(&compute_pipeline_layout),
            module: &shader,
            entry_point: Some("decode_tiles"),
            compilation_options: Default::default(),
            cache: None,
        });
        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("MVS presentation pipeline layout"),
                bind_group_layouts: &[None, Some(&render_layout)],
                immediate_size: 0,
            });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("MVS presentation pipeline"),
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
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("decoded MVS sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            compute_pipeline,
            render_pipeline,
            compute_layout,
            render_layout,
            sampler,
            decoded: None,
            records_buffer: None,
            payload_buffer: None,
            quantization_buffer: None,
            pending_mvs_decode: None,
        })
    }

    fn ensure_decoded_texture(&mut self, width: u32, height: u32) {
        if self
            .decoded
            .as_ref()
            .is_some_and(|decoded| decoded.width == width && decoded.height == height)
        {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("GPU-decoded MVS framebuffer"),
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
            // Compute storage textures cannot use an sRGB format. The final
            // fragment shader performs the sRGB-to-linear transfer before
            // writing to the sRGB presentation surface.
            view_formats: &[],
        });
        let storage_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let render_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("decoded MVS presentation bind group"),
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
        self.decoded = Some(DecodedTexture {
            width,
            height,
            texture,
            storage_view,
            render_bind_group,
        });
    }

    fn upload(&mut self, frame: &FramePacket) {
        if frame.rgba.is_some() {
            self.upload_rgba(frame);
        } else {
            self.upload_mvs(frame);
        }
    }

    fn upload_rgba(&mut self, frame: &FramePacket) {
        let Some(rgba) = frame.rgba.as_deref() else {
            return;
        };
        let width = u32::from(frame.width);
        let height = u32::from(frame.height);
        let Some(bytes_per_row) = width.checked_mul(4) else {
            return;
        };
        let Some(expected_len) = usize::try_from(bytes_per_row)
            .ok()
            .and_then(|row| row.checked_mul(usize::try_from(height).ok()?))
        else {
            return;
        };
        if rgba.len() != expected_len {
            return;
        }
        self.pending_mvs_decode = None;
        self.ensure_decoded_texture(width, height);
        let decoded = self.decoded.as_ref().expect("decoded texture created");
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
    }

    fn upload_mvs(&mut self, frame: &FramePacket) {
        if frame.tiles.is_empty() {
            return;
        }
        self.ensure_decoded_texture(u32::from(frame.width), u32::from(frame.height));
        let (records, payload) = pack_gpu_tiles(frame.tiles.iter());
        let mut quantization = Vec::with_capacity(128);
        quantization.extend(
            frame
                .luminance_quantization
                .iter()
                .map(|&value| u32::from(value)),
        );
        quantization.extend(
            frame
                .chrominance_quantization
                .iter()
                .map(|&value| u32::from(value)),
        );
        write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.records_buffer,
            "MVS tile records",
            &records,
        );
        write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.payload_buffer,
            "MVS tile payload",
            &payload,
        );
        write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.quantization_buffer,
            "MVS quantization tables",
            &quantization,
        );
        let records_buffer = &self
            .records_buffer
            .as_ref()
            .expect("records uploaded")
            .buffer;
        let payload_buffer = &self
            .payload_buffer
            .as_ref()
            .expect("payload uploaded")
            .buffer;
        let quantization_buffer = &self
            .quantization_buffer
            .as_ref()
            .expect("quantization uploaded")
            .buffer;
        let decoded = self.decoded.as_ref().expect("decoded texture created");
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("MVS compute bind group"),
            layout: &self.compute_layout,
            entries: &[
                buffer_entry(0, records_buffer),
                buffer_entry(1, payload_buffer),
                buffer_entry(2, quantization_buffer),
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&decoded.storage_view),
                },
            ],
        });
        self.pending_mvs_decode = Some(PendingMvsDecode {
            bind_group,
            workgroups: u32::try_from(frame.tiles.len()).expect("MVS tile count fits u32"),
        });
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self) -> RenderResult {
        let (output, reconfigure_after_present) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Timeout => return RenderResult::Retry,
            wgpu::CurrentSurfaceTexture::Occluded => return RenderResult::Skipped,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.surface.configure(&self.device, &self.config);
                return RenderResult::Retry;
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("MVS presentation commands"),
            });
        if let Some(decode) = self.pending_mvs_decode.take() {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("GPU MVS tile decode"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.compute_pipeline);
            pass.set_bind_group(0, &decode.bind_group, &[]);
            pass.dispatch_workgroups(decode.workgroups, 1, 1);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MVS presentation"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.018,
                            g: 0.022,
                            b: 0.028,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(decoded) = &self.decoded {
                let (x, y, width, height) = fitted_viewport(
                    self.config.width,
                    self.config.height,
                    decoded.width,
                    decoded.height,
                );
                pass.set_viewport(x, y, width, height, 0.0, 1.0);
                pass.set_pipeline(&self.render_pipeline);
                pass.set_bind_group(1, &decoded.render_bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(output);
        if reconfigure_after_present {
            self.surface.configure(&self.device, &self.config);
        }
        RenderResult::Presented
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
) {
    let bytes = bytemuck::cast_slice(values);
    let needed = u64::try_from(bytes.len())
        .expect("GPU upload length fits u64")
        .max(4);
    if slot.as_ref().is_none_or(|upload| upload.capacity < needed) {
        let capacity = needed.next_power_of_two();
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
    queue.write_buffer(
        &slot.as_ref().expect("upload buffer created").buffer,
        0,
        bytes,
    );
}

fn buffer_entry<'a>(binding: u32, buffer: &'a wgpu::Buffer) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn pack_gpu_tiles<'a>(tiles: impl Iterator<Item = &'a MvsGpuTileUpdate>) -> (Vec<u32>, Vec<i32>) {
    let tiles = tiles;
    let (tile_count, _) = tiles.size_hint();
    let mut records = Vec::with_capacity(tile_count.saturating_mul(8));
    let mut payload = Vec::new();
    for update in tiles {
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
    if payload.is_empty() {
        payload.push(0);
    }
    (records, payload)
}

fn pack_bytes(rgb: [u8; 3], alpha: u8) -> u32 {
    u32::from_le_bytes([rgb[0], rgb[1], rgb[2], alpha])
}

fn fitted_viewport(
    surface_width: u32,
    surface_height: u32,
    frame_width: u32,
    frame_height: u32,
) -> (f32, f32, f32, f32) {
    let scale = (surface_width as f32 / frame_width as f32)
        .min(surface_height as f32 / frame_height as f32);
    // Keep the viewport on physical-pixel boundaries. Fractional origins and
    // extents make even an otherwise sharp reconstruction sample between
    // texels, which is especially visible on small glyph stems.
    let width = (frame_width as f32 * scale).round().max(1.0);
    let height = (frame_height as f32 * scale).round().max(1.0);
    (
        ((surface_width as f32 - width) * 0.5).round(),
        ((surface_height as f32 - height) * 0.5).round(),
        width,
        height,
    )
}

fn start_receiver(
    config: ArdClientConfig,
    proxy: EventLoopProxy<ViewerEvent>,
    mailbox: SharedFrameMailbox,
    frame_event_pending: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let quality = config.video_quality;
        let _ = proxy.send_event(ViewerEvent::Status(format!("正在连接 {}…", config.address)));
        let mut client = match ArdClient::connect(config) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("ARD 连接失败：{error}");
                let _ = proxy.send_event(ViewerEvent::Status(format!("连接失败：{error}")));
                return;
            }
        };
        let _ = proxy.send_event(ViewerEvent::Connected(client.server_name().to_owned()));
        let mut rate_meter = RateMeter::new();
        loop {
            let info = match client.next_frame() {
                Ok(info) => info,
                Err(error) => {
                    eprintln!("ARD 接收失败：{error}");
                    let _ = proxy.send_event(ViewerEvent::Status(format!("连接已断开：{error}")));
                    return;
                }
            };
            let rates = rate_meter.record(info.framebuffer_updates, info.wire_bytes);
            if let Some(rates) = rates {
                eprintln!(
                    "ARD stream: {:.1} updates/s, ↓{:.2} Mbit/s",
                    rates.updates_per_second, rates.megabits_per_second
                );
            }
            let frames = client.take_gpu_mvs_frames();
            let mut queued = mailbox.lock().expect("frame mailbox poisoned");
            if frames.is_empty() {
                let framebuffer = client.framebuffer();
                if framebuffer.rgba().is_empty() {
                    continue;
                }
                let mut rgba = queued.rgba_pool.pop().unwrap_or_default();
                rgba.clear();
                rgba.extend_from_slice(framebuffer.rgba());
                let packet = FramePacket::from_rgba(
                    framebuffer.width(),
                    framebuffer.height(),
                    rgba,
                    quality,
                );
                if let Some(old) = queued.latest.replace(packet)
                    && let Some(buffer) = old.rgba
                    && queued.rgba_pool.len() < 3
                {
                    queued.rgba_pool.push(buffer);
                }
            } else {
                for frame in frames {
                    let can_merge = queued.latest.as_ref().is_some_and(|packet| {
                        packet.rgba.is_none()
                            && packet.width == frame.framebuffer_width
                            && packet.height == frame.framebuffer_height
                            && packet.luminance_quantization == frame.luminance_quantization
                            && packet.chrominance_quantization == frame.chrominance_quantization
                    });
                    if can_merge {
                        queued
                            .latest
                            .as_mut()
                            .expect("merge target checked")
                            .merge_mvs(frame);
                    } else {
                        let packet = FramePacket::from_mvs(frame, quality);
                        if let Some(old) = queued.latest.replace(packet)
                            && let Some(buffer) = old.rgba
                            && queued.rgba_pool.len() < 3
                        {
                            queued.rgba_pool.push(buffer);
                        }
                    }
                }
            }
            if !frame_event_pending.swap(true, Ordering::AcqRel) {
                let _ = proxy.send_event(ViewerEvent::FrameReady);
            }
        }
    });
}

const VIEWER_USAGE: &str = "用法：ARD_PASSWORD='密码' ard-viewer [--quality low|medium|high|adaptive|full] [--frame-interval-ms 毫秒] 地址:5900 用户名";

fn parse_quality(value: &str) -> Result<ArdVideoQuality, String> {
    match value {
        "low" => Ok(ArdVideoQuality::Low),
        "medium" => Ok(ArdVideoQuality::Medium),
        "high" => Ok(ArdVideoQuality::High),
        "adaptive" => Ok(ArdVideoQuality::Adaptive),
        "full" => Ok(ArdVideoQuality::Full),
        _ => Err(format!(
            "无效画质 {value:?}；可选 low、medium、high、adaptive、full"
        )),
    }
}

fn parse_cli_args(
    args: impl IntoIterator<Item = String>,
) -> Result<(String, String, ArdVideoQuality, Duration), String> {
    let mut args = args.into_iter();
    let mut positional = Vec::with_capacity(2);
    let mut quality = ArdVideoQuality::Adaptive;
    let mut frame_interval = Duration::ZERO;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--quality" => {
                let value = args.next().ok_or_else(|| "--quality 缺少参数".to_owned())?;
                quality = parse_quality(&value)?;
            }
            "--frame-interval-ms" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--frame-interval-ms 缺少参数".to_owned())?;
                let milliseconds = value
                    .parse::<u64>()
                    .map_err(|_| format!("无效帧间隔 {value:?}"))?;
                frame_interval = Duration::from_millis(milliseconds);
            }
            "-h" | "--help" => return Err(String::new()),
            value if value.starts_with('-') => return Err(format!("未知参数 {value:?}")),
            value => positional.push(value.to_owned()),
        }
    }
    if positional.len() != 2 {
        return Err("必须提供地址和用户名".to_owned());
    }
    Ok((
        positional.remove(0),
        positional.remove(0),
        quality,
        frame_interval,
    ))
}

fn main() {
    let (address, username, quality, frame_interval) = match parse_cli_args(env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("{error}");
            }
            eprintln!("{VIEWER_USAGE}");
            std::process::exit(if error.is_empty() { 0 } else { 2 });
        }
    };
    let password = match env::var_os("ARD_PASSWORD") {
        Some(password) => password.to_string_lossy().into_owned().into_bytes(),
        None => {
            eprintln!("缺少 ARD_PASSWORD 环境变量");
            std::process::exit(2);
        }
    };
    let event_loop = EventLoop::<ViewerEvent>::with_user_event()
        .build()
        .expect("无法创建事件循环");
    let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
    let frame_event_pending = Arc::new(AtomicBool::new(false));
    let mut config = ArdClientConfig::new(address, username.into_bytes(), password);
    config.video_quality = quality;
    config.frame_interval = frame_interval;
    start_receiver(
        config,
        event_loop.create_proxy(),
        mailbox.clone(),
        frame_event_pending.clone(),
    );
    let mut app = ViewerApp::new(mailbox, frame_event_pending);
    event_loop.run_app(&mut app).expect("查看器事件循环失败");
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{TileSet, fitted_viewport, pack_gpu_tiles, parse_cli_args};
    use ard_rs::{ArdVideoQuality, MvsGpuTile, MvsGpuTileUpdate};

    #[test]
    fn viewer_defaults_to_adaptive_mvs_and_native_maximum_rate() {
        let (_, _, quality, interval) =
            parse_cli_args(["host:5900".to_owned(), "user".to_owned()]).unwrap();
        assert_eq!(quality, ArdVideoQuality::Adaptive);
        assert_eq!(interval, Duration::ZERO);
    }

    #[test]
    fn viewer_accepts_adaptive_quality_and_frame_interval() {
        let (_, _, quality, interval) = parse_cli_args([
            "--quality".to_owned(),
            "adaptive".to_owned(),
            "--frame-interval-ms".to_owned(),
            "16".to_owned(),
            "host:5900".to_owned(),
            "user".to_owned(),
        ])
        .unwrap();
        assert_eq!(quality, ArdVideoQuality::Adaptive);
        assert_eq!(interval, Duration::from_millis(16));
    }

    #[test]
    fn gpu_shader_is_valid_wgsl() {
        let module = naga::front::wgsl::parse_str(include_str!("../viewer_mvs.wgsl"))
            .expect("viewer shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("viewer shader validates");
    }

    #[test]
    fn gpu_tile_packing_keeps_dct_coefficients_native() {
        let mut coefficients = [[0_i16; 64]; 3];
        coefficients[0][0] = -12;
        coefficients[2][63] = 99;
        let coefficients = std::sync::Arc::new(coefficients);
        let tile = MvsGpuTileUpdate {
            x: 8,
            y: 16,
            width: 8,
            height: 8,
            tile: MvsGpuTile::Dct(coefficients),
        };
        let (records, payload) = pack_gpu_tiles([&tile].into_iter());
        assert_eq!(&records[..7], &[8, 16, 8, 8, 4, 0, 0]);
        assert_eq!(payload.len(), 192);
        assert_eq!(payload[0], -12);
        assert_eq!(payload[191], 99);
    }

    #[test]
    fn tile_set_coalesces_same_tile_without_stale_dimensions() {
        let mut tiles = TileSet::new(16, 16, 2);
        tiles.insert(MvsGpuTileUpdate {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        });
        tiles.insert(MvsGpuTileUpdate {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        });

        let update = tiles.iter().next().expect("tile was inserted");
        assert_eq!(tiles.len(), 1);
        assert_eq!((update.width, update.height), (8, 8));
    }

    #[test]
    fn viewport_preserves_aspect_ratio() {
        let actual = fitted_viewport(1000, 1000, 1920, 1080);
        let expected = (0.0, 219.0, 1000.0, 563.0);
        for (actual, expected) in [actual.0, actual.1, actual.2, actual.3]
            .into_iter()
            .zip([expected.0, expected.1, expected.2, expected.3])
        {
            assert!((actual - expected).abs() < 0.001);
        }
    }
}
