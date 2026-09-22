//! GPU capture of the frame the viewer is about to present.
//!
//! The renderer calls [`RecordingTap::capture`] from inside the same command
//! encoder that draws the frame, and the tap enqueues a copy of the source
//! texture into a pooled staging buffer. Nothing is read back synchronously: the
//! buffer is mapped asynchronously and handed to the recording thread, which
//! encodes it and returns the buffer to the pool. A capture therefore costs the
//! render thread one extra copy on the GPU, never a stall.
//!
//! RGBA sources (MVS tiles decoded on the GPU, and native RGB framebuffers) are
//! swizzled to BGRA by a full-screen pass so the hardware encoder can take them
//! without a CPU-side byte swap. NV12 sources are copied plane by plane and reach
//! the encoder in the very format they were decoded into, with their colour range
//! and matrix preserved.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::media::{YuvMatrix, YuvPrimaries, YuvRange};

use super::encoder::SourceFrame;
use super::{RecordingControl, RecordingStats, StagingPool, Take};

/// Buffer copies require rows to start on a 256-byte boundary.
const ROW_ALIGNMENT: u64 = 256;

const SWIZZLE_SHADER: &str = r#"
@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle; the fragment shader ignores the interpolated uv and
// derives it from the fragment position instead.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    var out: VertexOutput;
    out.uv = vec2<f32>(x, y);
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let dimensions = vec2<f32>(textureDimensions(source_texture, 0));
    // `position.xy` is the pixel centre, so this samples exactly on texel
    // centres: the pass copies pixels and never filters them.
    let uv = in.position.xy / dimensions;
    return textureSampleLevel(source_texture, source_sampler, uv, 0.0);
}
"#;

/// What the renderer is about to draw, borrowed from the presentation pipeline.
pub enum PresentationSource<'a> {
    /// Single RGBA texture at remote resolution.
    Rgba {
        texture: &'a wgpu::Texture,
        width: u32,
        height: u32,
    },
    /// The two planes of a decoded AVC frame.
    Nv12 {
        y: &'a wgpu::Texture,
        uv: &'a wgpu::Texture,
        width: u32,
        height: u32,
        range: YuvRange,
        matrix: YuvMatrix,
        primaries: YuvPrimaries,
    },
}

impl PresentationSource<'_> {
    fn layout(&self) -> FrameLayout {
        match self {
            Self::Rgba { width, height, .. } => FrameLayout::bgra(*width, *height),
            Self::Nv12 {
                width,
                height,
                range,
                matrix,
                primaries,
                ..
            } => FrameLayout::nv12(*width, *height, *range, *matrix, *primaries),
        }
    }
}

/// Where each part of a captured frame sits inside its staging buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayout {
    Bgra {
        width: u32,
        height: u32,
        stride: u32,
        offset: u64,
    },
    Nv12 {
        width: u32,
        height: u32,
        y_stride: u32,
        y_offset: u64,
        uv_stride: u32,
        uv_offset: u64,
        range: YuvRange,
        matrix: YuvMatrix,
        /// Colour primaries the planes are encoded in. A Mac screen is normally
        /// Display P3, so a recording that claims BT.709 primaries shows a
        /// different picture from the viewer it was taken from.
        primaries: YuvPrimaries,
    },
}

impl FrameLayout {
    pub fn bgra(width: u32, height: u32) -> Self {
        let stride = align_row(u64::from(width) * 4);
        Self::Bgra {
            width,
            height,
            stride: stride as u32,
            offset: 0,
        }
    }

    /// Colour primaries of the planes, when this layout carries planes.
    pub fn primaries(&self) -> Option<YuvPrimaries> {
        match self {
            Self::Bgra { .. } => None,
            Self::Nv12 { primaries, .. } => Some(*primaries),
        }
    }

    pub fn nv12(
        width: u32,
        height: u32,
        range: YuvRange,
        matrix: YuvMatrix,
        primaries: YuvPrimaries,
    ) -> Self {
        let uv_width = width.div_ceil(2);
        let y_stride = align_row(u64::from(width));
        let y_offset = 0;
        let uv_stride = align_row(u64::from(uv_width) * 2);
        let uv_offset = align_row(y_stride * u64::from(height));
        Self::Nv12 {
            width,
            height,
            y_stride: y_stride as u32,
            y_offset,
            uv_stride: uv_stride as u32,
            uv_offset,
            range,
            matrix,
            primaries,
        }
    }

    pub fn width(&self) -> u32 {
        match self {
            Self::Bgra { width, .. } | Self::Nv12 { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Self::Bgra { height, .. } | Self::Nv12 { height, .. } => *height,
        }
    }

    pub fn is_nv12(&self) -> bool {
        matches!(self, Self::Nv12 { .. })
    }

    /// Bytes a staging buffer needs for one frame of this layout.
    pub fn buffer_size(&self) -> u64 {
        match self {
            Self::Bgra {
                height,
                stride,
                offset,
                ..
            } => offset + u64::from(*stride) * u64::from(*height),
            Self::Nv12 {
                height,
                uv_stride,
                uv_offset,
                ..
            } => uv_offset + u64::from(*uv_stride) * u64::from(height.div_ceil(2)),
        }
    }

    /// Borrow one frame out of a mapped staging buffer.
    ///
    /// Returns `None` when the mapping is shorter than the layout promises, which
    /// is the only way an encoder could read past the copy it was given.
    pub(crate) fn source_frame<'a>(&self, data: &'a [u8]) -> Option<SourceFrame<'a>> {
        let required = self.buffer_size();
        if u64::try_from(data.len()).ok()? < required {
            return None;
        }
        match self {
            Self::Bgra { stride, offset, .. } => Some(SourceFrame::Bgra {
                stride: *stride as usize,
                bytes: &data[*offset as usize..],
            }),
            Self::Nv12 {
                y_stride,
                y_offset,
                uv_stride,
                uv_offset,
                range,
                matrix,
                ..
            } => Some(SourceFrame::Nv12 {
                y_stride: *y_stride as usize,
                y: &data[*y_offset as usize..],
                uv_stride: *uv_stride as usize,
                uv: &data[*uv_offset as usize..],
                range: *range,
                matrix: *matrix,
            }),
        }
    }
}

fn align_row(bytes: u64) -> u64 {
    bytes.div_ceil(ROW_ALIGNMENT) * ROW_ALIGNMENT
}

/// One captured frame on its way to the encoder.
pub(crate) struct CapturedFrame {
    pub buffer: Arc<wgpu::Buffer>,
    pub pool: Arc<StagingPool>,
    pub layout: FrameLayout,
    /// Position on the take timeline at which the frame was presented.
    pub pts: Duration,
    /// The instant the frame was presented, which the take's first frame turns
    /// into the origin of the recorded timeline.
    pub presented: Instant,
}

struct SwizzleTap {
    width: u32,
    height: u32,
    target: wgpu::Texture,
    view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl SwizzleTap {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ARD recording swizzle bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ARD recording swizzle"),
            source: wgpu::ShaderSource::Wgsl(SWIZZLE_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ARD recording swizzle pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ARD recording swizzle pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ARD recording BGRA frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ARD recording swizzle sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Self {
            width,
            height,
            target,
            view,
            pipeline,
            layout,
            sampler,
        }
    }
}

/// Capture state owned by the render pipeline.
pub struct RecordingTap {
    device: wgpu::Device,
    take: Option<Arc<Take>>,
    take_id: u64,
    layout: Option<FrameLayout>,
    pool: Option<Arc<StagingPool>>,
    swizzle: Option<SwizzleTap>,
    /// A take starts by capturing the frame that is already on screen.
    awaiting_first_frame: bool,
    /// Frames whose copy is already encoded for presentation but whose staging
    /// buffer cannot be mapped yet.
    ///
    /// `wgpu` rejects mapping a buffer that a not-yet-submitted command buffer
    /// writes to, and the encoder this tap writes into is submitted by iced only
    /// after the draw returns. The staged frames are therefore mapped at the
    /// start of the *next* draw, by which time iced has submitted the previous
    /// one (`iced_wgpu::Renderer::present` builds, submits and presents one
    /// encoder per window and frame).
    pending: Vec<PendingCapture>,
}

/// One captured frame waiting for its presentation submission to be handed to
/// the GPU queue.
struct PendingCapture {
    buffer: Arc<wgpu::Buffer>,
    pool: Arc<StagingPool>,
    layout: FrameLayout,
    pts: Duration,
    presented: Instant,
    sender: Sender<CapturedFrame>,
    stats: Arc<RecordingStats>,
}

impl RecordingTap {
    pub fn new(device: &wgpu::Device) -> Self {
        Self {
            device: device.clone(),
            take: None,
            take_id: 0,
            layout: None,
            pool: None,
            swizzle: None,
            awaiting_first_frame: false,
            pending: Vec::new(),
        }
    }

    /// Follow the shared control switch. Returns whether a take is active.
    ///
    /// Every call first hands over the frames staged by earlier draws, which is
    /// the only point at which mapping their staging buffers is legal.
    pub fn sync(&mut self, control: &RecordingControl) -> bool {
        self.flush_pending();
        let active = control.active();
        let id = active.as_ref().map(|take| take.id()).unwrap_or(0);
        if id != self.take_id {
            self.take_id = id;
            self.take = active;
            self.awaiting_first_frame = self.take.is_some();
            // A different take may present a differently sized or formatted
            // frame, so both the pool and the swizzle target are rebuilt on
            // first use rather than reused across takes.
            self.layout = None;
            self.pool = None;
            if let Some(take) = self.take.as_ref() {
                take.publish_device(&self.device);
            }
        }
        match self.take.as_ref() {
            Some(take) if take.is_stopping() => false,
            Some(_) => true,
            None => false,
        }
    }

    /// Hand the staged frames to the recording thread.
    ///
    /// The mapping is requested here, after the draw that recorded the copy has
    /// been submitted, and completes when the device is polled (the recording
    /// thread polls while it waits for frames).
    fn flush_pending(&mut self) {
        for pending in self.pending.drain(..) {
            let PendingCapture {
                buffer,
                pool,
                layout,
                pts,
                presented,
                sender,
                stats,
            } = pending;
            let mapping = Arc::clone(&buffer);
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| match result {
                    Ok(()) => {
                        let frame = CapturedFrame {
                            buffer: Arc::clone(&mapping),
                            pool: Arc::clone(&pool),
                            layout,
                            pts,
                            presented,
                        };
                        if sender.send(frame).is_err() {
                            // The recording thread is gone, so nothing else will
                            // unmap or recycle this buffer.
                            mapping.unmap();
                            pool.release(mapping);
                        }
                    }
                    Err(_) => stats.record_dropped_frame(),
                });
        }
    }

    /// Whether the first frame of a newly started take still has to be written.
    ///
    /// The canvas already shows the most recent frame, and pressing record must
    /// record it even if the remote desktop is static and nothing is uploaded
    /// again afterwards.
    pub fn take_started(&mut self) -> bool {
        std::mem::take(&mut self.awaiting_first_frame)
    }

    /// Enqueue the capture of one frame.
    pub fn capture(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        source: PresentationSource<'_>,
        now: Instant,
    ) {
        let Some(take) = self.take.clone() else {
            return;
        };
        let Some(sender) = take.sender() else {
            // The take is closing; nothing more will be written to the file.
            return;
        };
        let layout = source.layout();
        if layout.width() == 0 || layout.height() == 0 {
            return;
        }
        if self.pool.is_none() || self.layout != Some(layout) {
            self.pool = Some(StagingPool::new(layout.buffer_size()));
            self.layout = Some(layout);
        }
        let pool = self.pool.clone().expect("pool was just created");
        let Some(buffer) = pool.acquire(&self.device) else {
            take.record_dropped_frame();
            return;
        };
        match &source {
            PresentationSource::Rgba {
                texture,
                width,
                height,
            } => {
                let resize = self
                    .swizzle
                    .as_ref()
                    .is_none_or(|swizzle| swizzle.width != *width || swizzle.height != *height);
                if resize {
                    self.swizzle = Some(SwizzleTap::new(&self.device, *width, *height));
                }
                let swizzle = self.swizzle.as_ref().expect("swizzle was just created");
                self.swizzle_into(encoder, swizzle, texture);
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &swizzle.target,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(layout_row_bytes(layout)),
                            rows_per_image: Some(*height),
                        },
                    },
                    wgpu::Extent3d {
                        width: *width,
                        height: *height,
                        depth_or_array_layers: 1,
                    },
                );
            }
            PresentationSource::Nv12 {
                y,
                uv,
                width,
                height,
                ..
            } => {
                let (y_stride, uv_stride, uv_offset) = match layout {
                    FrameLayout::Nv12 {
                        y_stride,
                        uv_stride,
                        uv_offset,
                        ..
                    } => (y_stride, uv_stride, uv_offset),
                    FrameLayout::Bgra { .. } => return,
                };
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: y,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(y_stride),
                            rows_per_image: Some(*height),
                        },
                    },
                    wgpu::Extent3d {
                        width: *width,
                        height: *height,
                        depth_or_array_layers: 1,
                    },
                );
                let uv_height = height.div_ceil(2);
                let uv_width = width.div_ceil(2);
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: uv,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: uv_offset,
                            bytes_per_row: Some(uv_stride),
                            rows_per_image: Some(uv_height),
                        },
                    },
                    wgpu::Extent3d {
                        width: uv_width,
                        height: uv_height,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        take.record_captured_frame();
        let pts = take.position(now);
        let stats = take.stats();
        // Staged rather than mapped: `wgpu` rejects mapping a buffer that the
        // command buffer being encoded still writes to, and iced submits that
        // command buffer only after this draw returns. The staged frames are
        // mapped by `flush_pending` on the next draw.
        self.pending.push(PendingCapture {
            buffer,
            pool,
            layout,
            pts,
            presented: now,
            sender,
            stats,
        });
    }

    fn swizzle_into(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        swizzle: &SwizzleTap,
        source: &wgpu::Texture,
    ) {
        let view = source.create_view(&wgpu::TextureViewDescriptor::default());
        // The bind group is rebuilt per frame because the pipeline may have
        // replaced its presentation texture with an identically sized one.
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ARD recording swizzle bind group"),
            layout: &swizzle.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&swizzle.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ARD recording swizzle"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &swizzle.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&swizzle.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn layout_row_bytes(layout: FrameLayout) -> u32 {
    match layout {
        FrameLayout::Bgra { stride, .. } => stride,
        FrameLayout::Nv12 { y_stride, .. } => y_stride,
    }
}

/// A headless wgpu device, or `None` where no adapter is available.
///
/// Shared by the recording tests that need real GPU buffers.
#[cfg(test)]
pub(crate) fn test_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{FrameLayout, PresentationSource, RecordingTap, test_device};
    use crate::media::{YuvMatrix, YuvPrimaries, YuvRange};
    use crate::recording::encoder::SourceFrame;
    use crate::recording::{RecordingControl, RecordingStats, Take};

    fn create_texture(
        device: &wgpu::Device,
        label: &str,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
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
            // The recorder copies these planes back out, which is exactly what
            // the presentation pipeline must allow.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    /// A deterministic pattern that a half-texel shift or a channel swap cannot
    /// survive: every pixel differs from its neighbours.
    fn pattern(width: u32, height: u32, channels: u32, seed: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity((width * height * channels) as usize);
        for y in 0..height {
            for x in 0..width {
                for channel in 0..channels {
                    bytes.push((((x * 7 + y * 13 + channel * 29 + seed * 53) % 251) + 1) as u8);
                }
            }
        }
        bytes
    }

    /// Start a take whose frames land in the returned receiver.
    fn start_take() -> (
        Arc<RecordingControl>,
        std::sync::mpsc::Receiver<super::CapturedFrame>,
        Arc<RecordingStats>,
    ) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let stats = Arc::new(RecordingStats::default());
        let take = Arc::new(Take::new(11, Arc::clone(&stats), sender));
        let control = Arc::new(RecordingControl::new());
        control.publish(take);
        (control, receiver, stats)
    }

    /// Draw one frame, then let the next draw hand it to the recording thread,
    /// exactly as the renderer does across two iced frames.
    fn capture_and_wait(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tap: &mut RecordingTap,
        control: &RecordingControl,
        source: PresentationSource<'_>,
        receiver: &std::sync::mpsc::Receiver<super::CapturedFrame>,
    ) -> super::CapturedFrame {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ARD capture test"),
        });
        tap.capture(&mut encoder, source, Instant::now());
        let index = queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(Duration::from_secs(10)),
            })
            .expect("the presentation submission completes");
        // The next draw flushes the staged frame.
        assert!(tap.sync(control), "the take is still active");
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(10)),
            })
            .expect("device poll");
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("a captured frame reaches the recording thread")
    }

    #[test]
    fn captured_rgb_frame_is_the_presented_texture() {
        let Some((device, queue)) = test_device() else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let (width, height) = (64_u32, 48_u32);
        let rgba = pattern(width, height, 4, 3);
        let source = create_texture(
            &device,
            "presented rgba",
            wgpu::TextureFormat::Rgba8Unorm,
            width,
            height,
        );
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &source,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let (control, receiver, stats) = start_take();
        let mut tap = RecordingTap::new(&device);
        assert!(tap.sync(&control), "the take is active");
        let frame = capture_and_wait(
            &device,
            &queue,
            &mut tap,
            &control,
            PresentationSource::Rgba {
                texture: &source,
                width,
                height,
            },
            &receiver,
        );

        assert_eq!(frame.layout, FrameLayout::bgra(width, height));
        assert_eq!(
            frame.layout.buffer_size(),
            u64::from(width) * 4 * u64::from(height)
        );
        let mapped = frame.buffer.slice(..).get_mapped_range();
        let stride = (width * 4) as usize;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let source_offset = (y * width as usize + x) * 4;
                let captured = &mapped[y * stride + x * 4..y * stride + x * 4 + 4];
                // The screen shows RGBA; the encoder takes BGRA, so the pass is
                // a byte swap and nothing else.
                assert_eq!(
                    captured,
                    &[
                        rgba[source_offset + 2],
                        rgba[source_offset + 1],
                        rgba[source_offset],
                        rgba[source_offset + 3],
                    ],
                    "pixel ({x}, {y}) differs from the presented texture"
                );
            }
        }
        drop(mapped);
        frame.buffer.unmap();
        let progress = stats.snapshot();
        assert_eq!(progress.captured_frames, 1);
        assert_eq!(progress.dropped_frames, 0);
    }

    #[test]
    fn captured_nv12_frame_keeps_both_planes_verbatim() {
        let Some((device, queue)) = test_device() else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        // An odd width exercises the 256-byte row padding of both planes.
        let (width, height) = (63_u32, 34_u32);
        let (uv_width, uv_height) = (width.div_ceil(2), height.div_ceil(2));
        let luma = pattern(width, height, 1, 5);
        let chroma = pattern(uv_width, uv_height, 2, 9);
        let luma_texture = create_texture(
            &device,
            "decoded luma",
            wgpu::TextureFormat::R8Unorm,
            width,
            height,
        );
        let chroma_texture = create_texture(
            &device,
            "decoded chroma",
            wgpu::TextureFormat::Rg8Unorm,
            uv_width,
            uv_height,
        );
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &luma_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &luma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &chroma_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &chroma,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(uv_width * 2),
                rows_per_image: Some(uv_height),
            },
            wgpu::Extent3d {
                width: uv_width,
                height: uv_height,
                depth_or_array_layers: 1,
            },
        );
        let (control, receiver, _stats) = start_take();
        let mut tap = RecordingTap::new(&device);
        assert!(tap.sync(&control), "the take is active");
        let frame = capture_and_wait(
            &device,
            &queue,
            &mut tap,
            &control,
            PresentationSource::Nv12 {
                y: &luma_texture,
                uv: &chroma_texture,
                width,
                height,
                range: YuvRange::Full,
                matrix: YuvMatrix::Bt601,
                primaries: YuvPrimaries::Bt709,
            },
            &receiver,
        );

        let expected = FrameLayout::nv12(
            width,
            height,
            YuvRange::Full,
            YuvMatrix::Bt601,
            YuvPrimaries::Bt709,
        );
        assert_eq!(frame.layout, expected);
        let FrameLayout::Nv12 {
            y_stride,
            y_offset,
            uv_stride,
            uv_offset,
            ..
        } = expected
        else {
            panic!("expected an NV12 layout");
        };
        // The encoder reads the planes through the layout helpers, so this is
        // what it would copy into its own buffers.
        let mapped = frame.buffer.slice(..).get_mapped_range();
        let luma_stride = y_stride as usize;
        for y in 0..height as usize {
            assert_eq!(
                &mapped[y_offset as usize + y * luma_stride
                    ..y_offset as usize + y * luma_stride + width as usize],
                &luma[y * width as usize..(y + 1) * width as usize],
                "luma row {y} differs from the decoded plane"
            );
        }
        let chroma_stride = uv_stride as usize;
        for y in 0..uv_height as usize {
            assert_eq!(
                &mapped[uv_offset as usize + y * chroma_stride
                    ..uv_offset as usize + y * chroma_stride + uv_width as usize * 2],
                &chroma[y * uv_width as usize * 2..(y + 1) * uv_width as usize * 2],
                "chroma row {y} differs from the decoded plane"
            );
        }
        drop(mapped);
        frame.buffer.unmap();
    }

    #[test]
    fn a_stopped_take_stops_capturing() {
        let Some((device, queue)) = test_device() else {
            eprintln!("skipping: no wgpu adapter");
            return;
        };
        let (control, _receiver, _stats) = start_take();
        let mut tap = RecordingTap::new(&device);
        assert!(tap.sync(&control));
        control.request_stop();
        assert!(
            !tap.sync(&control),
            "a take that is being saved must not capture more frames"
        );
        control.clear();
        assert!(!tap.sync(&control), "a cleared control has no active take");
        let source = create_texture(
            &device,
            "presented rgba",
            wgpu::TextureFormat::Rgba8Unorm,
            8,
            8,
        );
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ARD capture test"),
        });
        // Must be a no-op rather than a panic or a copy into a dead take.
        tap.capture(
            &mut encoder,
            PresentationSource::Rgba {
                texture: &source,
                width: 8,
                height: 8,
            },
            Instant::now(),
        );
        queue.submit([encoder.finish()]);
    }

    #[test]
    fn bgra_layout_pads_rows_to_the_copy_alignment() {
        let layout = FrameLayout::bgra(1920, 1080);
        match layout {
            FrameLayout::Bgra { stride, offset, .. } => {
                assert_eq!(stride, 1920 * 4);
                assert_eq!(offset, 0);
            }
            FrameLayout::Nv12 { .. } => panic!("expected BGRA"),
        }
        assert_eq!(layout.buffer_size(), 1920 * 4 * 1080);
        // A width whose row is not already aligned must be padded.
        let padded = FrameLayout::bgra(1919, 2);
        match padded {
            FrameLayout::Bgra { stride, .. } => assert_eq!(stride, 7680),
            FrameLayout::Nv12 { .. } => panic!("expected BGRA"),
        }
    }

    #[test]
    fn nv12_layout_keeps_both_planes_aligned_and_apart() {
        let layout = FrameLayout::nv12(
            1440,
            900,
            YuvRange::Video,
            YuvMatrix::Bt709,
            YuvPrimaries::Bt709,
        );
        match layout {
            FrameLayout::Nv12 {
                y_stride,
                y_offset,
                uv_stride,
                uv_offset,
                range,
                matrix,
                ..
            } => {
                // 1440 luma bytes per row are padded to 1536 for the copy.
                assert_eq!(y_stride, 1536);
                assert_eq!(y_offset, 0);
                assert_eq!(uv_stride, 1536);
                assert_eq!(uv_offset, 1536 * 900);
                assert_eq!(range, YuvRange::Video);
                assert_eq!(matrix, YuvMatrix::Bt709);
            }
            FrameLayout::Bgra { .. } => panic!("expected NV12"),
        }
        assert_eq!(layout.buffer_size(), 1536 * 900 + 1536 * 450);
        // An odd width pads the luma rows to 2048 and the 960 chroma pairs to
        // the same alignment.
        let odd = FrameLayout::nv12(
            1919,
            2,
            YuvRange::Full,
            YuvMatrix::Bt601,
            YuvPrimaries::Bt709,
        );
        assert_eq!(odd.buffer_size(), 2048 * 2 + 2048);
    }

    #[test]
    fn layouts_reject_short_mappings() {
        let layout = FrameLayout::bgra(64, 4);
        let short = vec![0_u8; layout.buffer_size() as usize - 1];
        assert!(layout.source_frame(&short).is_none());
        let exact = vec![0_u8; layout.buffer_size() as usize];
        let frame = layout.source_frame(&exact).expect("exact size is valid");
        match frame {
            SourceFrame::Bgra { stride, bytes } => {
                assert_eq!(stride, 256);
                assert_eq!(bytes.len(), exact.len());
            }
            SourceFrame::Nv12 { .. } => panic!("expected BGRA"),
        }
    }
}
