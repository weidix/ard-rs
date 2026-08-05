#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::env;
#[cfg(test)]
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdClientInput, ArdKey, ArdNamedKey,
    ArdVideoQuality, MvsGpuFrame, MvsGpuTile, MvsGpuTileUpdate, keysym_for_key,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{
    ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

const WINDOW_TITLE: &str = "ard-rs Viewer";
const MAX_RGBA_POOL: usize = 2;

#[derive(Debug)]
enum ViewerEvent {
    FrameReady,
    Status(String),
    Connected {
        server_name: String,
        input: ArdClientInput,
    },
    Clipboard(String),
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
    width: u16,
    height: u16,
    tiles_wide: usize,
    storage: TileStorage,
}

enum TileStorage {
    Dense {
        slots: Vec<u32>,
        tiles: Vec<MvsGpuTileUpdate>,
        dirty_bits: Vec<u64>,
        dirty_positions: Vec<usize>,
    },
    Sparse {
        tiles: HashMap<(u16, u16), MvsGpuTileUpdate>,
        dirty: HashSet<(u16, u16)>,
    },
}

#[cfg(test)]
enum TileSetIter<'a> {
    Dense(slice::Iter<'a, MvsGpuTileUpdate>),
    Sparse(std::collections::hash_map::Values<'a, (u16, u16), MvsGpuTileUpdate>),
}

#[cfg(test)]
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
                width,
                height,
                tiles_wide,
                storage: TileStorage::Dense {
                    slots: vec![EMPTY_TILE_SLOT; tile_count],
                    tiles: Vec::with_capacity(expected_tiles),
                    dirty_bits: vec![0; tile_count.div_ceil(64)],
                    dirty_positions: Vec::with_capacity(expected_tiles),
                },
            }
        } else {
            Self {
                width,
                height,
                tiles_wide,
                storage: TileStorage::Sparse {
                    tiles: HashMap::with_capacity(expected_tiles),
                    dirty: HashSet::with_capacity(expected_tiles),
                },
            }
        }
    }

    fn from_updates(width: u16, height: u16, updates: Vec<MvsGpuTileUpdate>) -> Self {
        let tiles_wide = usize::from(width).div_ceil(8);
        let tiles_high = usize::from(height).div_ceil(8);
        let tile_count = tiles_wide.saturating_mul(tiles_high);
        let expected_tiles = updates.len();
        if tile_count <= MAX_DENSE_TILE_SLOTS {
            let mut slots = vec![EMPTY_TILE_SLOT; tile_count];
            let mut dirty_bits = vec![0; tile_count.div_ceil(64)];
            let mut dirty_positions = Vec::with_capacity(expected_tiles);
            let mut unique_slots = true;
            for (position, update) in updates.iter().enumerate() {
                let slot = (usize::from(update.y) / 8)
                    .saturating_mul(tiles_wide)
                    .saturating_add(usize::from(update.x) / 8);
                let Some(slot_position) = slots.get_mut(slot) else {
                    unique_slots = false;
                    break;
                };
                if *slot_position != EMPTY_TILE_SLOT {
                    unique_slots = false;
                    break;
                }
                *slot_position = u32::try_from(position).expect("MVS tile count fits u32");
                let word = slot / 64;
                dirty_bits[word] |= 1_u64 << (slot % 64);
                dirty_positions.push(position);
            }
            if unique_slots {
                return Self {
                    width,
                    height,
                    tiles_wide,
                    storage: TileStorage::Dense {
                        slots,
                        tiles: updates,
                        dirty_bits,
                        dirty_positions,
                    },
                };
            }
        } else {
            let mut tiles = HashMap::with_capacity(expected_tiles);
            let mut dirty = HashSet::with_capacity(expected_tiles);
            for update in updates {
                let key = (update.x, update.y);
                tiles.insert(key, update);
                dirty.insert(key);
            }
            return Self {
                width,
                height,
                tiles_wide,
                storage: TileStorage::Sparse { tiles, dirty },
            };
        }

        let mut set = Self::new(width, height, expected_tiles);
        for update in updates {
            set.insert_force_dirty(update);
        }
        set
    }

    fn insert(&mut self, update: MvsGpuTileUpdate) {
        self.insert_inner(update, false);
    }

    fn insert_force_dirty(&mut self, update: MvsGpuTileUpdate) {
        self.insert_inner(update, true);
    }

    fn insert_inner(&mut self, update: MvsGpuTileUpdate, force_dirty: bool) {
        match &mut self.storage {
            TileStorage::Dense {
                slots,
                tiles,
                dirty_bits,
                dirty_positions,
            } => {
                let slot = (usize::from(update.y) / 8)
                    .saturating_mul(self.tiles_wide)
                    .saturating_add(usize::from(update.x) / 8);
                if let Some(position) = slots.get_mut(slot) {
                    let mut changed = force_dirty;
                    let position = if *position == EMPTY_TILE_SLOT {
                        let slot_position =
                            u32::try_from(tiles.len()).expect("MVS tile count fits u32");
                        *position = slot_position;
                        tiles.push(update);
                        changed = true;
                        slot_position as usize
                    } else {
                        let position = *position as usize;
                        let current = &tiles[position];
                        if force_dirty
                            || current.x != update.x
                            || current.y != update.y
                            || current.width != update.width
                            || current.height != update.height
                            || !same_mvs_tile(&current.tile, &update.tile)
                        {
                            tiles[position] = update;
                            changed = true;
                        }
                        position
                    };
                    if changed {
                        let word = slot / 64;
                        let mask = 1_u64 << (slot % 64);
                        if dirty_bits[word] & mask == 0 {
                            dirty_bits[word] |= mask;
                            dirty_positions.push(position);
                        }
                    }
                }
            }
            TileStorage::Sparse { tiles, dirty } => {
                let key = (update.x, update.y);
                let changed = force_dirty
                    || tiles.get(&key).is_none_or(|current| {
                        current.width != update.width
                            || current.height != update.height
                            || !same_mvs_tile(&current.tile, &update.tile)
                    });
                tiles.insert((update.x, update.y), update);
                if changed {
                    dirty.insert(key);
                }
            }
        }
    }

    fn merge(&mut self, other: TileSet, force_dirty: bool) {
        match other.storage {
            TileStorage::Dense { tiles, .. } => {
                for update in tiles {
                    if force_dirty {
                        self.insert_force_dirty(update);
                    } else {
                        self.insert(update);
                    }
                }
            }
            TileStorage::Sparse { tiles, .. } => {
                for (_, update) in tiles {
                    if force_dirty {
                        self.insert_force_dirty(update);
                    } else {
                        self.insert(update);
                    }
                }
            }
        }
    }

    fn clear_dirty(&mut self) {
        match &mut self.storage {
            TileStorage::Dense {
                dirty_bits,
                dirty_positions,
                ..
            } => {
                dirty_bits.fill(0);
                dirty_positions.clear();
            }
            TileStorage::Sparse { dirty, .. } => dirty.clear(),
        }
    }

    fn matches_dimensions(&self, width: u16, height: u16) -> bool {
        (self.width, self.height) == (width, height)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        match &self.storage {
            TileStorage::Dense { tiles, .. } => tiles.len(),
            TileStorage::Sparse { tiles, .. } => tiles.len(),
        }
    }

    #[cfg(test)]
    fn iter(&self) -> TileSetIter<'_> {
        match &self.storage {
            TileStorage::Dense { tiles, .. } => TileSetIter::Dense(tiles.iter()),
            TileStorage::Sparse { tiles, .. } => TileSetIter::Sparse(tiles.values()),
        }
    }

    fn dirty_len(&self) -> usize {
        match &self.storage {
            TileStorage::Dense {
                dirty_positions, ..
            } => dirty_positions.len(),
            TileStorage::Sparse { dirty, .. } => dirty.len(),
        }
    }

    fn for_each_dirty(&self, mut visit: impl FnMut(&MvsGpuTileUpdate)) {
        match &self.storage {
            TileStorage::Dense {
                tiles,
                dirty_positions,
                ..
            } => {
                for &position in dirty_positions {
                    if let Some(update) = tiles.get(position) {
                        visit(update);
                    }
                }
            }
            TileStorage::Sparse { tiles, dirty } => {
                for key in dirty {
                    if let Some(update) = tiles.get(key) {
                        visit(update);
                    }
                }
            }
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
        | (MvsGpuTile::Dct(left), MvsGpuTile::Dct(right)) => {
            Arc::ptr_eq(left, right) || left == right
        }
        _ => false,
    }
}

impl FramePacket {
    fn from_mvs(frame: MvsGpuFrame, quality: ArdVideoQuality) -> Self {
        Self {
            width: frame.framebuffer_width,
            height: frame.framebuffer_height,
            quality,
            luminance_quantization: frame.luminance_quantization,
            chrominance_quantization: frame.chrominance_quantization,
            tiles: TileSet::from_updates(
                frame.framebuffer_width,
                frame.framebuffer_height,
                frame.tiles,
            ),
            rgba: None,
        }
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

struct ClipboardBridge {
    clipboard: Clipboard,
    local_snapshot: Option<String>,
    remote_snapshot: Option<String>,
    next_poll: Instant,
}

impl ClipboardBridge {
    fn new() -> Result<Self, String> {
        let mut clipboard = Clipboard::new().map_err(|error| error.to_string())?;
        let local_snapshot = clipboard.get_text().ok();
        Ok(Self {
            clipboard,
            local_snapshot,
            remote_snapshot: None,
            next_poll: Instant::now(),
        })
    }

    fn local_text(&mut self) -> Option<String> {
        self.clipboard.get_text().ok()
    }

    fn apply_remote(&mut self, text: &str) -> Result<(), String> {
        self.clipboard
            .set_text(text)
            .map_err(|error| error.to_string())?;
        self.local_snapshot = Some(text.to_owned());
        self.remote_snapshot = Some(text.to_owned());
        Ok(())
    }

    fn poll_local(&mut self, input: Option<&ArdClientInput>) {
        let now = Instant::now();
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + Duration::from_millis(250);
        let Some(text) = self.local_text() else {
            return;
        };
        let changed = self.local_snapshot.as_deref() != Some(text.as_str());
        let came_from_remote = self.remote_snapshot.as_deref() == Some(text.as_str());
        self.local_snapshot = Some(text.clone());
        if came_from_remote {
            self.remote_snapshot = None;
        } else if changed
            && let Some(input) = input
            && input.send_clipboard_text(&text).is_ok()
        {
            self.remote_snapshot = None;
        }
    }
}

struct ViewerApp {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    mailbox: SharedFrameMailbox,
    frame_event_pending: Arc<AtomicBool>,
    presentation_meter: PresentationMeter,
    redraw_pending: bool,
    status: String,
    server_name: Option<String>,
    input: Option<ArdClientInput>,
    clipboard: Option<ClipboardBridge>,
    pending_clipboard: Option<String>,
    button_mask: u8,
    cursor_position: Option<(u16, u16)>,
    pointer_inside: bool,
    modifiers: ModifiersState,
    pressed_keys: HashMap<PhysicalKey, u32>,
    ime_suppressed_keys: HashSet<PhysicalKey>,
    paste_suppressed_keys: HashSet<PhysicalKey>,
    ime_active: bool,
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
            input: None,
            clipboard: None,
            pending_clipboard: None,
            button_mask: 0,
            cursor_position: None,
            pointer_inside: false,
            modifiers: ModifiersState::default(),
            pressed_keys: HashMap::new(),
            ime_suppressed_keys: HashSet::new(),
            paste_suppressed_keys: HashSet::new(),
            ime_active: false,
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
        window.set_ime_allowed(true);
        if self.clipboard.is_none() {
            match ClipboardBridge::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => eprintln!("系统剪切板不可用：{error}"),
            }
        }
        if let Some(text) = self.pending_clipboard.take()
            && let Some(clipboard) = &mut self.clipboard
            && let Err(error) = clipboard.apply_remote(&text)
        {
            eprintln!("写入系统剪切板失败：{error}");
        }
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
            ViewerEvent::Connected { server_name, input } => {
                self.server_name = Some(server_name);
                self.input = Some(input);
                self.status = "已连接，等待首帧…".to_owned();
                self.update_title(None);
            }
            ViewerEvent::Clipboard(text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    if let Err(error) = clipboard.apply_remote(&text) {
                        eprintln!("写入系统剪切板失败：{error}");
                    }
                } else {
                    self.pending_clipboard = Some(text);
                }
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
            WindowEvent::Focused(false) => self.release_input_state(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_keyboard_input(event),
            WindowEvent::Ime(ime) => self.handle_ime_event(ime),
            WindowEvent::CursorMoved { position, .. } => self.handle_cursor_moved(position),
            WindowEvent::CursorLeft { .. } => self.pointer_inside = false,
            WindowEvent::MouseInput { state, button, .. } => self.handle_mouse_input(state, button),
            WindowEvent::MouseWheel { delta, phase, .. } => self.handle_mouse_wheel(delta, phase),
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(clipboard) = &mut self.clipboard {
            clipboard.poll_local(self.input.as_ref());
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(250),
        ));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.release_input_state();
        self.input = None;
        self.clipboard = None;
    }
}

impl ViewerApp {
    fn remote_position(&self, position: PhysicalPosition<f64>) -> Option<(u16, u16)> {
        self.renderer.as_ref()?.remote_position(position)
    }

    fn handle_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        let Some(remote) = self.remote_position(position) else {
            self.pointer_inside = false;
            return;
        };
        self.pointer_inside = true;
        self.cursor_position = Some(remote);
        if let Some(input) = &self.input {
            let _ = input.try_send_pointer_event(self.button_mask, remote.0, remote.1);
        }
    }

    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let Some(bit) = mouse_button_bit(button) else {
            return;
        };
        let Some((x, y)) = self.cursor_position else {
            return;
        };
        if state == ElementState::Pressed && !self.pointer_inside {
            return;
        }
        match state {
            ElementState::Pressed => self.button_mask |= bit,
            ElementState::Released => self.button_mask &= !bit,
        }
        if let Some(input) = &self.input
            && let Err(error) = input.send_pointer_event(self.button_mask, x, y)
        {
            eprintln!("发送鼠标事件失败：{error}");
        }
    }

    fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta, phase: TouchPhase) {
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            return;
        }
        let (horizontal, vertical) = match delta {
            MouseScrollDelta::LineDelta(horizontal, vertical) => {
                (f64::from(horizontal), f64::from(vertical))
            }
            MouseScrollDelta::PixelDelta(position) => (position.x / 40.0, position.y / 40.0),
        };
        if vertical != 0.0 {
            let button = if vertical.is_sign_negative() {
                0x08
            } else {
                0x10
            };
            self.send_scroll_clicks(button, vertical.abs().round().clamp(1.0, 8.0) as usize);
        }
        if horizontal != 0.0 {
            let button = if horizontal.is_sign_negative() {
                0x20
            } else {
                0x40
            };
            self.send_scroll_clicks(button, horizontal.abs().round().clamp(1.0, 8.0) as usize);
        }
    }

    fn send_scroll_clicks(&self, button: u8, count: usize) {
        if !self.pointer_inside {
            return;
        }
        let Some((x, y)) = self.cursor_position else {
            return;
        };
        let Some(input) = &self.input else { return };
        for _ in 0..count {
            if let Err(error) = input.send_pointer_event(self.button_mask | button, x, y) {
                eprintln!("发送滚轮事件失败：{error}");
                return;
            }
            if let Err(error) = input.send_pointer_event(self.button_mask, x, y) {
                eprintln!("发送滚轮释放事件失败：{error}");
                return;
            }
        }
    }

    fn handle_keyboard_input(&mut self, event: KeyEvent) {
        let physical_key = event.physical_key;
        if event.state == ElementState::Pressed && is_paste_shortcut(&event, self.modifiers) {
            self.paste_suppressed_keys.insert(physical_key);
            self.send_local_clipboard();
            return;
        }
        if event.state == ElementState::Released && self.paste_suppressed_keys.remove(&physical_key)
        {
            return;
        }
        if event.state == ElementState::Released && self.ime_suppressed_keys.remove(&physical_key) {
            return;
        }
        if self.ime_active && is_textual_key(&event) {
            if event.state == ElementState::Pressed {
                self.ime_suppressed_keys.insert(physical_key);
            }
            return;
        }
        let keysym = match event.state {
            ElementState::Pressed => key_event_keysym(&event),
            ElementState::Released => self.pressed_keys.remove(&physical_key),
        };
        let Some(keysym) = keysym else { return };
        if event.state == ElementState::Pressed {
            self.pressed_keys.insert(physical_key, keysym);
        }
        if let Some(input) = &self.input
            && let Err(error) = input.send_key_event(event.state == ElementState::Pressed, keysym)
        {
            eprintln!("发送键盘事件失败：{error}");
        }
    }

    fn handle_ime_event(&mut self, event: Ime) {
        match event {
            Ime::Enabled => self.ime_active = true,
            Ime::Preedit(text, _) => {
                if !text.is_empty() {
                    self.ime_active = true;
                }
            }
            Ime::Commit(text) => {
                self.ime_active = false;
                let Some(input) = &self.input else { return };
                for character in text.chars() {
                    let Some(keysym) = keysym_for_key(ArdKey::Character(character)) else {
                        continue;
                    };
                    if let Err(error) = input.send_key_event(true, keysym) {
                        eprintln!("发送输入法按键失败：{error}");
                        return;
                    }
                    if let Err(error) = input.send_key_event(false, keysym) {
                        eprintln!("发送输入法按键释放失败：{error}");
                        return;
                    }
                }
            }
            Ime::Disabled => self.ime_active = false,
        }
    }

    fn send_local_clipboard(&mut self) {
        let text = self
            .clipboard
            .as_mut()
            .and_then(ClipboardBridge::local_text);
        let Some(text) = text else { return };
        if let Some(input) = &self.input
            && let Err(error) = input.send_clipboard_text(&text)
        {
            eprintln!("发送剪切板失败：{error}");
        }
    }

    fn release_input_state(&mut self) {
        let pressed_keys = core::mem::take(&mut self.pressed_keys);
        self.ime_suppressed_keys.clear();
        self.paste_suppressed_keys.clear();
        if let Some(input) = &self.input {
            for keysym in pressed_keys.values().copied() {
                let _ = input.send_key_event(false, keysym);
            }
        }
        if self.button_mask != 0
            && let Some((x, y)) = self.cursor_position
            && let Some(input) = &self.input
        {
            let _ = input.send_pointer_event(0, x, y);
        }
        self.button_mask = 0;
        self.pointer_inside = false;
    }

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
        let Some(mut frame) = frame else { return };
        let changed = self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.upload(&mut frame));
        if self.status != "正在查看" {
            self.status = "正在查看".to_owned();
            self.update_title(Some(&frame));
        }
        if changed && let Some(window) = &self.window {
            self.redraw_pending = true;
            window.request_redraw();
        }
        let mut mailbox = self.mailbox.lock().expect("frame mailbox poisoned");
        if let Some(buffer) = frame.rgba
            && mailbox.rgba_pool.len() < MAX_RGBA_POOL
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
    records_scratch: Vec<u32>,
    payload_scratch: Vec<i32>,
    quantization_scratch: Vec<u32>,
    uploaded_quantization: Option<([u16; 64], [u16; 64])>,
    uploaded_mvs_tiles: Option<TileSet>,
    mvs_bind_group: Option<wgpu::BindGroup>,
    pending_mvs_decode: Option<u32>,
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
            records_scratch: Vec::new(),
            payload_scratch: Vec::new(),
            quantization_scratch: Vec::with_capacity(128),
            uploaded_quantization: None,
            uploaded_mvs_tiles: None,
            mvs_bind_group: None,
            pending_mvs_decode: None,
        })
    }

    fn ensure_decoded_texture(&mut self, width: u32, height: u32) -> bool {
        if self
            .decoded
            .as_ref()
            .is_some_and(|decoded| decoded.width == width && decoded.height == height)
        {
            return false;
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
        let Some(expected_len) = usize::try_from(bytes_per_row)
            .ok()
            .and_then(|row| row.checked_mul(usize::try_from(height).ok()?))
        else {
            return false;
        };
        if rgba.len() != expected_len {
            return false;
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
        true
    }

    fn upload_mvs(&mut self, frame: &mut FramePacket) -> bool {
        let incoming_tiles = std::mem::replace(&mut frame.tiles, TileSet::new(0, 0, 0));
        let texture_recreated =
            self.ensure_decoded_texture(u32::from(frame.width), u32::from(frame.height));
        let same_dimensions = self
            .uploaded_mvs_tiles
            .as_ref()
            .is_some_and(|tiles| tiles.matches_dimensions(frame.width, frame.height));
        let quantization = (frame.luminance_quantization, frame.chrominance_quantization);
        let quantization_changed =
            self.uploaded_quantization != Some(quantization) || self.quantization_buffer.is_none();
        let mut uploaded_tiles = if same_dimensions {
            let mut uploaded_tiles = self
                .uploaded_mvs_tiles
                .take()
                .expect("MVS tile dimensions checked");
            uploaded_tiles.merge(incoming_tiles, texture_recreated || quantization_changed);
            uploaded_tiles
        } else {
            // FramePacket construction marks every tile in a new MVS state
            // dirty, so the first upload can take ownership of its storage
            // directly instead of rebuilding a second TileSet.
            incoming_tiles
        };
        let dirty_tiles = uploaded_tiles.dirty_len();
        if dirty_tiles == 0 {
            uploaded_tiles.clear_dirty();
            self.uploaded_mvs_tiles = Some(uploaded_tiles);
            return false;
        }
        pack_dirty_gpu_tiles(
            &uploaded_tiles,
            &mut self.records_scratch,
            &mut self.payload_scratch,
        );
        let records_recreated = write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.records_buffer,
            "MVS tile records",
            &self.records_scratch,
        );
        let payload_recreated = write_storage_buffer(
            &self.device,
            &self.queue,
            &mut self.payload_buffer,
            "MVS tile payload",
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
            let recreated = write_storage_buffer(
                &self.device,
                &self.queue,
                &mut self.quantization_buffer,
                "MVS quantization tables",
                &self.quantization_scratch,
            );
            self.uploaded_quantization = Some(quantization);
            recreated
        } else {
            false
        };
        if records_recreated
            || payload_recreated
            || quantization_recreated
            || self.mvs_bind_group.is_none()
        {
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
            self.mvs_bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
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
            }));
        }
        self.pending_mvs_decode =
            Some(u32::try_from(dirty_tiles).expect("MVS tile count fits u32"));
        uploaded_tiles.clear_dirty();
        self.uploaded_mvs_tiles = Some(uploaded_tiles);
        true
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    fn remote_position(&self, position: PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let decoded = self.decoded.as_ref()?;
        let (viewport_x, viewport_y, viewport_width, viewport_height) = fitted_viewport(
            self.config.width,
            self.config.height,
            decoded.width,
            decoded.height,
        );
        let x = position.x as f32;
        let y = position.y as f32;
        if x < viewport_x
            || y < viewport_y
            || x >= viewport_x + viewport_width
            || y >= viewport_y + viewport_height
        {
            return None;
        }
        let remote_x = (((x - viewport_x) / viewport_width) * decoded.width as f32)
            .floor()
            .clamp(0.0, decoded.width.saturating_sub(1) as f32) as u16;
        let remote_y = (((y - viewport_y) / viewport_height) * decoded.height as f32)
            .floor()
            .clamp(0.0, decoded.height.saturating_sub(1) as f32) as u16;
        Some((remote_x, remote_y))
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
        if let Some(workgroups) = self.pending_mvs_decode.take() {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("GPU MVS tile decode"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.compute_pipeline);
            pass.set_bind_group(
                0,
                self.mvs_bind_group
                    .as_ref()
                    .expect("MVS bind group created"),
                &[],
            );
            pass.dispatch_workgroups(workgroups, 1, 1);
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
) -> bool {
    let bytes = bytemuck::cast_slice(values);
    let needed = u64::try_from(bytes.len())
        .expect("GPU upload length fits u64")
        .max(4);
    let recreated = slot.as_ref().is_none_or(|upload| upload.capacity < needed);
    if recreated {
        // Keep a small growth margin for changing tile counts without
        // reserving nearly twice the live upload size as power-of-two growth
        // would do for a frame just over a bucket boundary.
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
    queue.write_buffer(
        &slot.as_ref().expect("upload buffer created").buffer,
        0,
        bytes,
    );
    recreated
}

fn buffer_entry<'a>(binding: u32, buffer: &'a wgpu::Buffer) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

#[cfg(test)]
fn pack_gpu_tiles<'a>(
    tiles: impl Iterator<Item = &'a MvsGpuTileUpdate>,
    records: &mut Vec<u32>,
    payload: &mut Vec<i32>,
) {
    let tiles = tiles;
    let (tile_count, _) = tiles.size_hint();
    records.clear();
    payload.clear();
    records.reserve(tile_count.saturating_mul(8));
    for update in tiles {
        pack_one_gpu_tile(update, records, payload);
    }
    if payload.is_empty() {
        payload.push(0);
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

fn mouse_button_bit(button: MouseButton) -> Option<u8> {
    match button {
        MouseButton::Left => Some(0x01),
        MouseButton::Middle => Some(0x02),
        MouseButton::Right => Some(0x04),
        MouseButton::Back | MouseButton::Other(1) => Some(0x20),
        MouseButton::Forward | MouseButton::Other(2) => Some(0x40),
        MouseButton::Other(_) => None,
    }
}

fn is_paste_shortcut(event: &KeyEvent, modifiers: ModifiersState) -> bool {
    if matches!(event.logical_key, Key::Named(NamedKey::Paste)) {
        return true;
    }
    if !modifiers.control_key() && !modifiers.super_key() {
        return false;
    }
    if modifiers.alt_key() {
        return false;
    }
    let Key::Character(text) = event.logical_key.as_ref() else {
        return false;
    };
    text.chars()
        .next()
        .is_some_and(|character| character.eq_ignore_ascii_case(&'v'))
}

fn is_textual_key(event: &KeyEvent) -> bool {
    matches!(
        event.logical_key,
        Key::Character(_) | Key::Dead(Some(_) | None)
    )
}

fn key_event_keysym(event: &KeyEvent) -> Option<u32> {
    match event.logical_key.as_ref() {
        Key::Character(text) => text
            .chars()
            .next()
            .and_then(|character| keysym_for_key(ArdKey::Character(character))),
        Key::Named(key) => {
            named_key_to_ard(key, event.location).and_then(|key| keysym_for_key(ArdKey::Named(key)))
        }
        Key::Dead(Some(character)) => keysym_for_key(ArdKey::Character(character)),
        Key::Dead(None) | Key::Unidentified(_) => None,
    }
    .or_else(|| physical_key_to_keysym(event.physical_key))
}

fn named_key_to_ard(key: NamedKey, location: KeyLocation) -> Option<ArdNamedKey> {
    let left = matches!(location, KeyLocation::Left | KeyLocation::Standard);
    Some(match key {
        NamedKey::Backspace => ArdNamedKey::Backspace,
        NamedKey::Tab => ArdNamedKey::Tab,
        NamedKey::Enter => ArdNamedKey::Enter,
        NamedKey::Space => ArdNamedKey::Space,
        NamedKey::Escape => ArdNamedKey::Escape,
        NamedKey::Delete => ArdNamedKey::Delete,
        NamedKey::Insert => ArdNamedKey::Insert,
        NamedKey::Home => ArdNamedKey::Home,
        NamedKey::End => ArdNamedKey::End,
        NamedKey::PageUp => ArdNamedKey::PageUp,
        NamedKey::PageDown => ArdNamedKey::PageDown,
        NamedKey::ArrowLeft => ArdNamedKey::ArrowLeft,
        NamedKey::ArrowUp => ArdNamedKey::ArrowUp,
        NamedKey::ArrowRight => ArdNamedKey::ArrowRight,
        NamedKey::ArrowDown => ArdNamedKey::ArrowDown,
        NamedKey::Shift => {
            if left {
                ArdNamedKey::ShiftLeft
            } else {
                ArdNamedKey::ShiftRight
            }
        }
        NamedKey::Control => {
            if left {
                ArdNamedKey::ControlLeft
            } else {
                ArdNamedKey::ControlRight
            }
        }
        NamedKey::Alt | NamedKey::AltGraph => {
            if left && !matches!(key, NamedKey::AltGraph) {
                ArdNamedKey::AltLeft
            } else {
                ArdNamedKey::AltRight
            }
        }
        NamedKey::Super => {
            if left {
                ArdNamedKey::SuperLeft
            } else {
                ArdNamedKey::SuperRight
            }
        }
        NamedKey::Meta => {
            if left {
                ArdNamedKey::MetaLeft
            } else {
                ArdNamedKey::MetaRight
            }
        }
        NamedKey::CapsLock => ArdNamedKey::CapsLock,
        NamedKey::NumLock => ArdNamedKey::NumLock,
        NamedKey::ScrollLock => ArdNamedKey::ScrollLock,
        NamedKey::PrintScreen => ArdNamedKey::PrintScreen,
        NamedKey::Pause => ArdNamedKey::Pause,
        NamedKey::ContextMenu => ArdNamedKey::ContextMenu,
        NamedKey::F1 => ArdNamedKey::Function(1),
        NamedKey::F2 => ArdNamedKey::Function(2),
        NamedKey::F3 => ArdNamedKey::Function(3),
        NamedKey::F4 => ArdNamedKey::Function(4),
        NamedKey::F5 => ArdNamedKey::Function(5),
        NamedKey::F6 => ArdNamedKey::Function(6),
        NamedKey::F7 => ArdNamedKey::Function(7),
        NamedKey::F8 => ArdNamedKey::Function(8),
        NamedKey::F9 => ArdNamedKey::Function(9),
        NamedKey::F10 => ArdNamedKey::Function(10),
        NamedKey::F11 => ArdNamedKey::Function(11),
        NamedKey::F12 => ArdNamedKey::Function(12),
        NamedKey::F13 => ArdNamedKey::Function(13),
        NamedKey::F14 => ArdNamedKey::Function(14),
        NamedKey::F15 => ArdNamedKey::Function(15),
        NamedKey::F16 => ArdNamedKey::Function(16),
        NamedKey::F17 => ArdNamedKey::Function(17),
        NamedKey::F18 => ArdNamedKey::Function(18),
        NamedKey::F19 => ArdNamedKey::Function(19),
        NamedKey::F20 => ArdNamedKey::Function(20),
        NamedKey::F21 => ArdNamedKey::Function(21),
        NamedKey::F22 => ArdNamedKey::Function(22),
        NamedKey::F23 => ArdNamedKey::Function(23),
        NamedKey::F24 => ArdNamedKey::Function(24),
        NamedKey::F25 => ArdNamedKey::Function(25),
        NamedKey::F26 => ArdNamedKey::Function(26),
        NamedKey::F27 => ArdNamedKey::Function(27),
        NamedKey::F28 => ArdNamedKey::Function(28),
        NamedKey::F29 => ArdNamedKey::Function(29),
        NamedKey::F30 => ArdNamedKey::Function(30),
        NamedKey::F31 => ArdNamedKey::Function(31),
        NamedKey::F32 => ArdNamedKey::Function(32),
        NamedKey::F33 => ArdNamedKey::Function(33),
        NamedKey::F34 => ArdNamedKey::Function(34),
        NamedKey::F35 => ArdNamedKey::Function(35),
        _ => return None,
    })
}

fn physical_key_to_keysym(key: PhysicalKey) -> Option<u32> {
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    let neutral = match code {
        KeyCode::Backquote => ArdKey::Character('`'),
        KeyCode::Backslash | KeyCode::IntlBackslash | KeyCode::IntlRo | KeyCode::IntlYen => {
            ArdKey::Character('\\')
        }
        KeyCode::BracketLeft => ArdKey::Character('['),
        KeyCode::BracketRight => ArdKey::Character(']'),
        KeyCode::Comma => ArdKey::Character(','),
        KeyCode::Digit0 => ArdKey::Character('0'),
        KeyCode::Digit1 => ArdKey::Character('1'),
        KeyCode::Digit2 => ArdKey::Character('2'),
        KeyCode::Digit3 => ArdKey::Character('3'),
        KeyCode::Digit4 => ArdKey::Character('4'),
        KeyCode::Digit5 => ArdKey::Character('5'),
        KeyCode::Digit6 => ArdKey::Character('6'),
        KeyCode::Digit7 => ArdKey::Character('7'),
        KeyCode::Digit8 => ArdKey::Character('8'),
        KeyCode::Digit9 => ArdKey::Character('9'),
        KeyCode::Equal => ArdKey::Character('='),
        KeyCode::KeyA => ArdKey::Character('a'),
        KeyCode::KeyB => ArdKey::Character('b'),
        KeyCode::KeyC => ArdKey::Character('c'),
        KeyCode::KeyD => ArdKey::Character('d'),
        KeyCode::KeyE => ArdKey::Character('e'),
        KeyCode::KeyF => ArdKey::Character('f'),
        KeyCode::KeyG => ArdKey::Character('g'),
        KeyCode::KeyH => ArdKey::Character('h'),
        KeyCode::KeyI => ArdKey::Character('i'),
        KeyCode::KeyJ => ArdKey::Character('j'),
        KeyCode::KeyK => ArdKey::Character('k'),
        KeyCode::KeyL => ArdKey::Character('l'),
        KeyCode::KeyM => ArdKey::Character('m'),
        KeyCode::KeyN => ArdKey::Character('n'),
        KeyCode::KeyO => ArdKey::Character('o'),
        KeyCode::KeyP => ArdKey::Character('p'),
        KeyCode::KeyQ => ArdKey::Character('q'),
        KeyCode::KeyR => ArdKey::Character('r'),
        KeyCode::KeyS => ArdKey::Character('s'),
        KeyCode::KeyT => ArdKey::Character('t'),
        KeyCode::KeyU => ArdKey::Character('u'),
        KeyCode::KeyV => ArdKey::Character('v'),
        KeyCode::KeyW => ArdKey::Character('w'),
        KeyCode::KeyX => ArdKey::Character('x'),
        KeyCode::KeyY => ArdKey::Character('y'),
        KeyCode::KeyZ => ArdKey::Character('z'),
        KeyCode::Minus => ArdKey::Character('-'),
        KeyCode::Period => ArdKey::Character('.'),
        KeyCode::Quote => ArdKey::Character('\''),
        KeyCode::Semicolon => ArdKey::Character(';'),
        KeyCode::Slash => ArdKey::Character('/'),
        KeyCode::Space => ArdKey::Named(ArdNamedKey::Space),
        KeyCode::Backspace => ArdKey::Named(ArdNamedKey::Backspace),
        KeyCode::CapsLock => ArdKey::Named(ArdNamedKey::CapsLock),
        KeyCode::ControlLeft => ArdKey::Named(ArdNamedKey::ControlLeft),
        KeyCode::ControlRight => ArdKey::Named(ArdNamedKey::ControlRight),
        KeyCode::AltLeft => ArdKey::Named(ArdNamedKey::AltLeft),
        KeyCode::AltRight => ArdKey::Named(ArdNamedKey::AltRight),
        KeyCode::ShiftLeft => ArdKey::Named(ArdNamedKey::ShiftLeft),
        KeyCode::ShiftRight => ArdKey::Named(ArdNamedKey::ShiftRight),
        KeyCode::SuperLeft => ArdKey::Named(ArdNamedKey::SuperLeft),
        KeyCode::SuperRight => ArdKey::Named(ArdNamedKey::SuperRight),
        KeyCode::Enter => ArdKey::Named(ArdNamedKey::Enter),
        KeyCode::Tab => ArdKey::Named(ArdNamedKey::Tab),
        KeyCode::Escape => ArdKey::Named(ArdNamedKey::Escape),
        KeyCode::Delete => ArdKey::Named(ArdNamedKey::Delete),
        KeyCode::End => ArdKey::Named(ArdNamedKey::End),
        KeyCode::Home => ArdKey::Named(ArdNamedKey::Home),
        KeyCode::Insert => ArdKey::Named(ArdNamedKey::Insert),
        KeyCode::PageDown => ArdKey::Named(ArdNamedKey::PageDown),
        KeyCode::PageUp => ArdKey::Named(ArdNamedKey::PageUp),
        KeyCode::ArrowDown => ArdKey::Named(ArdNamedKey::ArrowDown),
        KeyCode::ArrowLeft => ArdKey::Named(ArdNamedKey::ArrowLeft),
        KeyCode::ArrowRight => ArdKey::Named(ArdNamedKey::ArrowRight),
        KeyCode::ArrowUp => ArdKey::Named(ArdNamedKey::ArrowUp),
        KeyCode::NumLock => ArdKey::Named(ArdNamedKey::NumLock),
        KeyCode::PrintScreen => ArdKey::Named(ArdNamedKey::PrintScreen),
        KeyCode::ScrollLock => ArdKey::Named(ArdNamedKey::ScrollLock),
        KeyCode::Pause => ArdKey::Named(ArdNamedKey::Pause),
        KeyCode::ContextMenu => ArdKey::Named(ArdNamedKey::ContextMenu),
        KeyCode::F1 => ArdKey::Named(ArdNamedKey::Function(1)),
        KeyCode::F2 => ArdKey::Named(ArdNamedKey::Function(2)),
        KeyCode::F3 => ArdKey::Named(ArdNamedKey::Function(3)),
        KeyCode::F4 => ArdKey::Named(ArdNamedKey::Function(4)),
        KeyCode::F5 => ArdKey::Named(ArdNamedKey::Function(5)),
        KeyCode::F6 => ArdKey::Named(ArdNamedKey::Function(6)),
        KeyCode::F7 => ArdKey::Named(ArdNamedKey::Function(7)),
        KeyCode::F8 => ArdKey::Named(ArdNamedKey::Function(8)),
        KeyCode::F9 => ArdKey::Named(ArdNamedKey::Function(9)),
        KeyCode::F10 => ArdKey::Named(ArdNamedKey::Function(10)),
        KeyCode::F11 => ArdKey::Named(ArdNamedKey::Function(11)),
        KeyCode::F12 => ArdKey::Named(ArdNamedKey::Function(12)),
        KeyCode::F13 => ArdKey::Named(ArdNamedKey::Function(13)),
        KeyCode::F14 => ArdKey::Named(ArdNamedKey::Function(14)),
        KeyCode::F15 => ArdKey::Named(ArdNamedKey::Function(15)),
        KeyCode::F16 => ArdKey::Named(ArdNamedKey::Function(16)),
        KeyCode::F17 => ArdKey::Named(ArdNamedKey::Function(17)),
        KeyCode::F18 => ArdKey::Named(ArdNamedKey::Function(18)),
        KeyCode::F19 => ArdKey::Named(ArdNamedKey::Function(19)),
        KeyCode::F20 => ArdKey::Named(ArdNamedKey::Function(20)),
        KeyCode::F21 => ArdKey::Named(ArdNamedKey::Function(21)),
        KeyCode::F22 => ArdKey::Named(ArdNamedKey::Function(22)),
        KeyCode::F23 => ArdKey::Named(ArdNamedKey::Function(23)),
        KeyCode::F24 => ArdKey::Named(ArdNamedKey::Function(24)),
        KeyCode::F25 => ArdKey::Named(ArdNamedKey::Function(25)),
        KeyCode::F26 => ArdKey::Named(ArdNamedKey::Function(26)),
        KeyCode::F27 => ArdKey::Named(ArdNamedKey::Function(27)),
        KeyCode::F28 => ArdKey::Named(ArdNamedKey::Function(28)),
        KeyCode::F29 => ArdKey::Named(ArdNamedKey::Function(29)),
        KeyCode::F30 => ArdKey::Named(ArdNamedKey::Function(30)),
        KeyCode::F31 => ArdKey::Named(ArdNamedKey::Function(31)),
        KeyCode::F32 => ArdKey::Named(ArdNamedKey::Function(32)),
        KeyCode::F33 => ArdKey::Named(ArdNamedKey::Function(33)),
        KeyCode::F34 => ArdKey::Named(ArdNamedKey::Function(34)),
        KeyCode::F35 => ArdKey::Named(ArdNamedKey::Function(35)),
        KeyCode::Numpad0 => ArdKey::Named(ArdNamedKey::Numpad(0)),
        KeyCode::Numpad1 => ArdKey::Named(ArdNamedKey::Numpad(1)),
        KeyCode::Numpad2 => ArdKey::Named(ArdNamedKey::Numpad(2)),
        KeyCode::Numpad3 => ArdKey::Named(ArdNamedKey::Numpad(3)),
        KeyCode::Numpad4 => ArdKey::Named(ArdNamedKey::Numpad(4)),
        KeyCode::Numpad5 => ArdKey::Named(ArdNamedKey::Numpad(5)),
        KeyCode::Numpad6 => ArdKey::Named(ArdNamedKey::Numpad(6)),
        KeyCode::Numpad7 => ArdKey::Named(ArdNamedKey::Numpad(7)),
        KeyCode::Numpad8 => ArdKey::Named(ArdNamedKey::Numpad(8)),
        KeyCode::Numpad9 => ArdKey::Named(ArdNamedKey::Numpad(9)),
        KeyCode::NumpadAdd => ArdKey::Named(ArdNamedKey::NumpadAdd),
        KeyCode::NumpadSubtract => ArdKey::Named(ArdNamedKey::NumpadSubtract),
        KeyCode::NumpadMultiply => ArdKey::Named(ArdNamedKey::NumpadMultiply),
        KeyCode::NumpadDivide => ArdKey::Named(ArdNamedKey::NumpadDivide),
        KeyCode::NumpadDecimal | KeyCode::NumpadComma => ArdKey::Named(ArdNamedKey::NumpadDecimal),
        KeyCode::NumpadEnter => ArdKey::Named(ArdNamedKey::NumpadEnter),
        KeyCode::NumpadEqual => ArdKey::Named(ArdNamedKey::NumpadEqual),
        _ => return None,
    };
    keysym_for_key(neutral)
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
        let _ = proxy.send_event(ViewerEvent::Connected {
            server_name: client.server_name().to_owned(),
            input: client.input(),
        });
        let mut rate_meter = RateMeter::new();
        loop {
            let info = match client.next_event() {
                Ok(ArdClientEvent::Frame(info)) => info,
                Ok(ArdClientEvent::Clipboard(text)) => {
                    let _ = proxy.send_event(ViewerEvent::Clipboard(text));
                    continue;
                }
                Ok(ArdClientEvent::Bell | ArdClientEvent::StateChange) => continue,
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
            let mut queued = mailbox.lock().expect("frame mailbox poisoned");
            let mut has_gpu_frames = false;
            client.drain_gpu_mvs_frames(|frame| {
                has_gpu_frames = true;
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
                        && queued.rgba_pool.len() < MAX_RGBA_POOL
                    {
                        queued.rgba_pool.push(buffer);
                    }
                }
            });
            if !has_gpu_frames {
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
                    && queued.rgba_pool.len() < MAX_RGBA_POOL
                {
                    queued.rgba_pool.push(buffer);
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

    use super::{TileSet, fitted_viewport, pack_dirty_gpu_tiles, pack_gpu_tiles, parse_cli_args};
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
        let mut records = Vec::new();
        let mut payload = Vec::new();
        pack_gpu_tiles([&tile].into_iter(), &mut records, &mut payload);
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
    fn tile_set_tracks_only_changed_tiles_for_incremental_uploads() {
        let mut tiles = TileSet::new(16, 16, 2);
        let update = MvsGpuTileUpdate {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        };
        tiles.insert(update.clone());
        tiles.insert(update.clone());
        assert_eq!(tiles.dirty_len(), 1);

        tiles.insert(MvsGpuTileUpdate {
            x: 8,
            y: 0,
            width: 8,
            height: 8,
            tile: update.tile,
        });
        assert_eq!(tiles.dirty_len(), 2);

        let mut records = Vec::new();
        let mut payload = Vec::new();
        pack_dirty_gpu_tiles(&tiles, &mut records, &mut payload);
        assert_eq!(records.len(), 2 * 8);
    }

    #[test]
    fn tile_set_from_updates_preserves_unique_and_duplicate_updates() {
        let first = MvsGpuTileUpdate {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        };
        let second = MvsGpuTileUpdate {
            x: 8,
            y: 0,
            width: 8,
            height: 8,
            tile: MvsGpuTile::SolidRgba([4, 5, 6, 255]),
        };
        let unique = TileSet::from_updates(16, 8, vec![first.clone(), second.clone()]);
        assert_eq!(unique.len(), 2);
        assert_eq!(unique.dirty_len(), 2);

        let replacement = MvsGpuTileUpdate {
            tile: MvsGpuTile::SolidRgba([7, 8, 9, 255]),
            ..first.clone()
        };
        let duplicate = TileSet::from_updates(16, 8, vec![first, replacement.clone()]);
        assert_eq!(duplicate.len(), 1);
        assert_eq!(duplicate.iter().next(), Some(&replacement));
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
