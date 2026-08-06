use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::i18n::Language;
use ard_rs::{
    ArdClient, ArdClientConfig, ArdClientEvent, ArdClientInput, ArdKey, ArdNamedKey,
    ArdVideoQuality, Framebuffer, MvsGpuFrame, MvsGpuTile, MvsGpuTileUpdate, keysym_for_key,
};
use iced::futures::StreamExt;
use iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use iced::futures::stream::BoxStream;
use iced::keyboard::key::{Code, Named, Physical};
use iced::keyboard::{Key, Location, Modifiers};
use iced::mouse::{Button, ScrollDelta};
use iced::{Point, Rectangle, Size, Subscription};

const MAX_EVENTS: usize = 32;
const MAX_INPUT_COMMANDS: usize = 128;
const MAX_RGBA_POOL: usize = 2;
const MAX_RECONNECT_ATTEMPTS: usize = 5;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const PRECISE_SCROLL_PIXELS_PER_LINE: f64 = 20.0;
const MAX_SCROLL_CLICKS_PER_EVENT: f64 = 64.0;
const EMPTY_TILE_SLOT: u32 = u32::MAX;
const MAX_DENSE_TILE_SLOTS: usize = 1_048_576;

#[derive(Clone)]
pub struct SessionConfig {
    pub address: String,
    pub username: String,
    pub password: Vec<u8>,
    pub quality: ArdVideoQuality,
    pub frame_interval: Duration,
}

impl Drop for SessionConfig {
    fn drop(&mut self) {
        self.password.fill(0);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Idle,
    Connecting,
    Connected,
    Reconnecting { attempt: usize },
    Disconnected(String),
    Failed(String),
}

impl ConnectionState {
    pub fn label(&self, language: Language) -> String {
        match (language, self) {
            (Language::English, Self::Idle) => "Not connected".into(),
            (Language::English, Self::Connecting) => "Connecting...".into(),
            (Language::English, Self::Connected) => "Connected".into(),
            (Language::English, Self::Reconnecting { attempt }) => {
                format!("Reconnecting ({attempt}/{MAX_RECONNECT_ATTEMPTS})...")
            }
            (Language::English, Self::Disconnected(error)) => {
                format!("Disconnected: {}", language.tr(error))
            }
            (Language::English, Self::Failed(error)) => {
                format!(
                    "Connection or authentication failed: {}",
                    language.tr(error)
                )
            }
            (_, Self::Idle) => "未连接".into(),
            (_, Self::Connecting) => "正在连接…".into(),
            (_, Self::Connected) => "已连接".into(),
            (_, Self::Reconnecting { attempt }) => {
                format!("正在重连（{attempt}/{MAX_RECONNECT_ATTEMPTS}）…")
            }
            (_, Self::Disconnected(error)) => format!("连接已断开：{error}"),
            (_, Self::Failed(error)) => format!("连接或认证失败：{error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StreamMetrics {
    pub frames_per_second: f64,
    pub megabits_per_second: f64,
    pub width: u16,
    pub height: u16,
    pub gpu_mvs: bool,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    State(ConnectionState),
    Connected {
        server_name: String,
        input: ArdClientInput,
    },
    Clipboard(String),
    Metrics(StreamMetrics),
    RenderFailed(String),
}

pub struct FramePacket {
    pub width: u16,
    pub height: u16,
    pub quality: ArdVideoQuality,
    pub luminance_quantization: [u16; 64],
    pub chrominance_quantization: [u16; 64],
    pub tiles: TileSet,
    pub rgba: Option<Vec<u8>>,
}

impl std::fmt::Debug for FramePacket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FramePacket")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("quality", &self.quality)
            .field("gpu_mvs", &self.rgba.is_none())
            .finish()
    }
}

impl FramePacket {
    pub(crate) fn from_mvs(frame: MvsGpuFrame, quality: ArdVideoQuality) -> Self {
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

    pub(crate) fn from_rgba(
        width: u16,
        height: u16,
        rgba: Vec<u8>,
        quality: ArdVideoQuality,
    ) -> Self {
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

#[derive(Debug, Default)]
pub struct FrameMailbox {
    pub latest: Option<FramePacket>,
    rgba_pool: Vec<Vec<u8>>,
    events: VecDeque<SessionEvent>,
    metrics: StreamMetrics,
    metrics_dirty: bool,
    pub generation: u64,
}

impl FrameMailbox {
    pub fn push_event(&mut self, event: SessionEvent) {
        self.events.retain(|queued| {
            !matches!(
                (&event, queued),
                (SessionEvent::State(_), SessionEvent::State(_))
                    | (
                        SessionEvent::Connected { .. },
                        SessionEvent::Connected { .. }
                    )
                    | (SessionEvent::Clipboard(_), SessionEvent::Clipboard(_))
                    | (SessionEvent::RenderFailed(_), SessionEvent::RenderFailed(_))
            )
        });
        if self.events.len() == MAX_EVENTS {
            if matches!(event, SessionEvent::Clipboard(_)) {
                return;
            }
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub fn drain_events(&mut self) -> Vec<SessionEvent> {
        let mut events: Vec<_> = self.events.drain(..).collect();
        if self.metrics_dirty {
            events.push(SessionEvent::Metrics(self.metrics));
            self.metrics_dirty = false;
        }
        events
    }

    fn replace_latest(&mut self, packet: FramePacket) {
        if let Some(old) = self.latest.replace(packet)
            && let Some(buffer) = old.rgba
            && self.rgba_pool.len() < MAX_RGBA_POOL
        {
            self.rgba_pool.push(buffer);
        }
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn recycle_rgba(&mut self, buffer: Vec<u8>) {
        if self.rgba_pool.len() < MAX_RGBA_POOL {
            self.rgba_pool.push(buffer);
        }
    }
}

pub type SharedMailbox = Arc<Mutex<FrameMailbox>>;

#[derive(Debug)]
struct FrameWake {
    sender: UnboundedSender<()>,
    receiver: Mutex<Option<UnboundedReceiver<()>>>,
    pending: AtomicBool,
}

impl FrameWake {
    fn new() -> Arc<Self> {
        let (sender, receiver) = unbounded();
        Arc::new(Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            pending: AtomicBool::new(false),
        })
    }

    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.sender.unbounded_send(());
        }
    }

    fn acknowledge(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct FrameWakeSubscription(Arc<FrameWake>);

impl Hash for FrameWakeSubscription {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

fn frame_wake_stream(wake: &FrameWakeSubscription) -> BoxStream<'static, ()> {
    wake.0
        .receiver
        .lock()
        .ok()
        .and_then(|mut receiver| receiver.take())
        .map_or_else(
            || iced::futures::stream::pending().boxed(),
            StreamExt::boxed,
        )
}

pub struct SessionRuntime {
    mailbox: SharedMailbox,
    frame_wake: Arc<FrameWake>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SessionRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionRuntime")
            .finish_non_exhaustive()
    }
}

impl SessionRuntime {
    pub fn start(config: SessionConfig) -> Self {
        let mailbox = Arc::new(Mutex::new(FrameMailbox::default()));
        let frame_wake = FrameWake::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_mailbox = Arc::clone(&mailbox);
        let worker_frame_wake = Arc::clone(&frame_wake);
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("ard-session".into())
            .spawn(move || run_receiver(config, worker_mailbox, worker_frame_wake, worker_cancel))
            .expect("ARD session worker thread should start");
        Self {
            mailbox,
            frame_wake,
            cancel,
            worker: Some(worker),
        }
    }

    pub fn mailbox(&self) -> SharedMailbox {
        Arc::clone(&self.mailbox)
    }

    pub fn drain_events(&self) -> Vec<SessionEvent> {
        self.frame_wake.acknowledge();
        self.mailbox.lock().map_or_else(
            |_| vec![SessionEvent::RenderFailed("会话缓存已损坏".into())],
            |mut mailbox| mailbox.drain_events(),
        )
    }

    pub fn frame_subscription(&self) -> Subscription<()> {
        Subscription::run_with(
            FrameWakeSubscription(Arc::clone(&self.frame_wake)),
            frame_wake_stream,
        )
    }

    pub fn disconnect(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.worker.take();
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn run_receiver(
    config: SessionConfig,
    mailbox: SharedMailbox,
    frame_wake: Arc<FrameWake>,
    cancel: Arc<AtomicBool>,
) {
    let mut reconnecting = false;
    let mut attempts = 0;
    while !cancel.load(Ordering::Acquire) {
        push_event(
            &mailbox,
            &frame_wake,
            SessionEvent::State(if reconnecting {
                ConnectionState::Reconnecting {
                    attempt: attempts + 1,
                }
            } else {
                ConnectionState::Connecting
            }),
        );
        let mut client_config = ArdClientConfig::new(
            config.address.clone(),
            config.username.as_bytes().to_vec(),
            config.password.clone(),
        );
        client_config.video_quality = config.quality;
        client_config.timeout = Duration::from_secs(2);
        client_config.frame_interval = config.frame_interval;
        let mut client = match ArdClient::connect(client_config) {
            Ok(client) => client,
            Err(error) => {
                attempts += 1;
                if attempts >= MAX_RECONNECT_ATTEMPTS {
                    push_event(
                        &mailbox,
                        &frame_wake,
                        SessionEvent::State(ConnectionState::Failed(error.to_string())),
                    );
                    return;
                }
                reconnecting = true;
                if wait_for_retry(&cancel) {
                    return;
                }
                continue;
            }
        };
        attempts = 0;
        reconnecting = true;
        push_event(
            &mailbox,
            &frame_wake,
            SessionEvent::Connected {
                server_name: client.server_name().to_owned(),
                input: client.input(),
            },
        );
        push_event(
            &mailbox,
            &frame_wake,
            SessionEvent::State(ConnectionState::Connected),
        );
        let mut meter = RateMeter::new();
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            match client.next_event() {
                Ok(ArdClientEvent::Frame(info)) => {
                    let gpu_mvs = queue_frame(&mailbox, &frame_wake, &mut client, config.quality);
                    let metrics = meter.record(
                        info.framebuffer_updates,
                        info.wire_bytes,
                        client.framebuffer().width(),
                        client.framebuffer().height(),
                        gpu_mvs,
                    );
                    if let Ok(mut mailbox) = mailbox.lock() {
                        if let Some(metrics) = metrics {
                            mailbox.metrics = metrics;
                        } else {
                            mailbox.metrics.width = client.framebuffer().width();
                            mailbox.metrics.height = client.framebuffer().height();
                            mailbox.metrics.gpu_mvs = gpu_mvs;
                        }
                        mailbox.metrics_dirty = true;
                    }
                }
                Ok(ArdClientEvent::Clipboard(text)) => {
                    push_event(&mailbox, &frame_wake, SessionEvent::Clipboard(text));
                }
                Ok(ArdClientEvent::Bell | ArdClientEvent::StateChange) => {}
                Ok(ArdClientEvent::Reconnected) => unreachable!("automatic reconnect is disabled"),
                Err(error) => {
                    push_event(
                        &mailbox,
                        &frame_wake,
                        SessionEvent::State(ConnectionState::Disconnected(error.to_string())),
                    );
                    if wait_for_retry(&cancel) {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

fn wait_for_retry(cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + RECONNECT_DELAY;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Acquire) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn push_event(mailbox: &SharedMailbox, frame_wake: &FrameWake, event: SessionEvent) {
    if let Ok(mut mailbox) = mailbox.lock() {
        mailbox.push_event(event);
    }
    frame_wake.notify();
}

fn queue_frame(
    mailbox: &SharedMailbox,
    frame_wake: &FrameWake,
    client: &mut ArdClient,
    quality: ArdVideoQuality,
) -> bool {
    let gpu_frames = client.take_gpu_mvs_frames();
    if !gpu_frames.is_empty() {
        let Ok(mut queued) = mailbox.lock() else {
            return true;
        };
        for frame in gpu_frames {
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
                queued.generation = queued.generation.wrapping_add(1);
            } else {
                queued.replace_latest(FramePacket::from_mvs(frame, quality));
            }
        }
        drop(queued);
        frame_wake.notify();
        return true;
    }
    let framebuffer = client.framebuffer();
    let mut rgba = mailbox
        .lock()
        .ok()
        .and_then(|mut queued| queued.rgba_pool.pop())
        .unwrap_or_default();
    if framebuffer_to_rgba(framebuffer, &mut rgba) {
        if let Ok(mut queued) = mailbox.lock() {
            queued.replace_latest(FramePacket::from_rgba(
                framebuffer.width(),
                framebuffer.height(),
                rgba,
                quality,
            ));
        }
    } else {
        if let Ok(mut queued) = mailbox.lock() {
            queued.recycle_rgba(rgba);
            queued.push_event(SessionEvent::RenderFailed(
                "无法将远程 framebuffer 转换为 RGBA 显示数据".into(),
            ));
        }
    }
    frame_wake.notify();
    false
}

struct RateMeter {
    started: Instant,
    updates: usize,
    wire_bytes: usize,
}

impl RateMeter {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            updates: 0,
            wire_bytes: 0,
        }
    }

    fn record(
        &mut self,
        updates: usize,
        wire_bytes: usize,
        width: u16,
        height: u16,
        gpu_mvs: bool,
    ) -> Option<StreamMetrics> {
        self.updates = self.updates.saturating_add(updates);
        self.wire_bytes = self.wire_bytes.saturating_add(wire_bytes);
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        let seconds = elapsed.as_secs_f64();
        let metrics = StreamMetrics {
            frames_per_second: self.updates as f64 / seconds,
            megabits_per_second: self.wire_bytes as f64 * 8.0 / seconds / 1_000_000.0,
            width,
            height,
            gpu_mvs,
        };
        self.started = Instant::now();
        self.updates = 0;
        self.wire_bytes = 0;
        Some(metrics)
    }
}

#[derive(Debug)]
pub struct TileSet {
    width: u16,
    height: u16,
    tiles_wide: usize,
    storage: TileStorage,
}

#[derive(Debug)]
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

impl TileSet {
    pub fn new(width: u16, height: u16, expected_tiles: usize) -> Self {
        let tiles_wide = usize::from(width).div_ceil(8);
        let tile_count = tiles_wide.saturating_mul(usize::from(height).div_ceil(8));
        let storage = if tile_count <= MAX_DENSE_TILE_SLOTS {
            TileStorage::Dense {
                slots: vec![EMPTY_TILE_SLOT; tile_count],
                tiles: Vec::with_capacity(expected_tiles),
                dirty_bits: vec![0; tile_count.div_ceil(64)],
                dirty_positions: Vec::with_capacity(expected_tiles),
            }
        } else {
            TileStorage::Sparse {
                tiles: HashMap::with_capacity(expected_tiles),
                dirty: HashSet::with_capacity(expected_tiles),
            }
        };
        Self {
            width,
            height,
            tiles_wide,
            storage,
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

    pub fn insert(&mut self, update: MvsGpuTileUpdate) {
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
                let Some(position) = slots.get_mut(slot) else {
                    return;
                };
                let mut changed = force_dirty;
                let index = if *position == EMPTY_TILE_SLOT {
                    let index = tiles.len();
                    *position = u32::try_from(index).expect("MVS tile count fits u32");
                    tiles.push(update);
                    changed = true;
                    index
                } else {
                    let index = *position as usize;
                    let current = &tiles[index];
                    if force_dirty
                        || current.x != update.x
                        || current.y != update.y
                        || current.width != update.width
                        || current.height != update.height
                        || !same_mvs_tile(&current.tile, &update.tile)
                    {
                        tiles[index] = update;
                        changed = true;
                    }
                    index
                };
                if changed {
                    let word = slot / 64;
                    let mask = 1_u64 << (slot % 64);
                    if dirty_bits[word] & mask == 0 {
                        dirty_bits[word] |= mask;
                        dirty_positions.push(index);
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
                tiles.insert(key, update);
                if changed {
                    dirty.insert(key);
                }
            }
        }
    }

    pub fn merge(&mut self, other: Self, force_dirty: bool) {
        match other.storage {
            TileStorage::Dense { tiles, .. } => {
                for tile in tiles {
                    self.insert_inner(tile, force_dirty);
                }
            }
            TileStorage::Sparse { tiles, .. } => {
                for (_, tile) in tiles {
                    self.insert_inner(tile, force_dirty);
                }
            }
        }
    }

    pub fn clear_dirty(&mut self) {
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

    pub fn matches_dimensions(&self, width: u16, height: u16) -> bool {
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
    fn first(&self) -> Option<&MvsGpuTileUpdate> {
        match &self.storage {
            TileStorage::Dense { tiles, .. } => tiles.first(),
            TileStorage::Sparse { tiles, .. } => tiles.values().next(),
        }
    }

    pub fn dirty_len(&self) -> usize {
        match &self.storage {
            TileStorage::Dense {
                dirty_positions, ..
            } => dirty_positions.len(),
            TileStorage::Sparse { dirty, .. } => dirty.len(),
        }
    }

    pub fn for_each_dirty(&self, mut visit: impl FnMut(&MvsGpuTileUpdate)) {
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
        (MvsGpuTile::SolidYcbcr(a), MvsGpuTile::SolidYcbcr(b)) => a == b,
        (MvsGpuTile::SolidRgba(a), MvsGpuTile::SolidRgba(b)) => a == b,
        (MvsGpuTile::PixelsYcbcr(a), MvsGpuTile::PixelsYcbcr(b)) => a == b,
        (MvsGpuTile::PixelsRgba(a), MvsGpuTile::PixelsRgba(b)) => a == b,
        (MvsGpuTile::RiceDct(a), MvsGpuTile::RiceDct(b))
        | (MvsGpuTile::Dct(a), MvsGpuTile::Dct(b)) => Arc::ptr_eq(a, b) || a == b,
        _ => false,
    }
}

pub fn framebuffer_to_rgba(framebuffer: &Framebuffer, output: &mut Vec<u8>) -> bool {
    let format = framebuffer.pixel_format();
    let Ok(bytes_per_pixel) = format.bytes_per_pixel() else {
        return false;
    };
    let Some(pixel_count) =
        usize::from(framebuffer.width()).checked_mul(usize::from(framebuffer.height()))
    else {
        return false;
    };
    let Some(expected_len) = pixel_count.checked_mul(bytes_per_pixel) else {
        return false;
    };
    if framebuffer.pixels().len() != expected_len
        || !format.true_color
        || format.red_max == 0
        || format.green_max == 0
        || format.blue_max == 0
        || format.red_shift >= 32
        || format.green_shift >= 32
        || format.blue_shift >= 32
    {
        return false;
    }
    let Some(output_len) = pixel_count.checked_mul(4) else {
        return false;
    };
    output.clear();
    output.reserve(output_len);
    for bytes in framebuffer.pixels().chunks_exact(bytes_per_pixel) {
        let value = match (bytes_per_pixel, format.big_endian) {
            (1, _) => u32::from(bytes[0]),
            (2, true) => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
            (2, false) => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            (4, true) => u32::from_be_bytes(bytes.try_into().expect("pixel width checked")),
            (4, false) => u32::from_le_bytes(bytes.try_into().expect("pixel width checked")),
            _ => return false,
        };
        output.extend_from_slice(&[
            scale_channel(value, format.red_shift, format.red_max),
            scale_channel(value, format.green_shift, format.green_max),
            scale_channel(value, format.blue_shift, format.blue_max),
            255,
        ]);
    }
    true
}

fn scale_channel(value: u32, shift: u8, max: u16) -> u8 {
    ((((value >> shift) & u32::from(max)) * 255 + u32::from(max) / 2) / u32::from(max)) as u8
}

pub fn fitted_viewport(bounds: Rectangle, frame: Size<u16>, zoom: f32) -> Rectangle {
    if frame.width == 0 || frame.height == 0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Rectangle::new(bounds.position(), Size::ZERO);
    }
    let fit = (bounds.width / f32::from(frame.width)).min(bounds.height / f32::from(frame.height));
    let scale = fit * zoom.clamp(0.25, 4.0);
    let width = (f32::from(frame.width) * scale).round().max(1.0);
    let height = (f32::from(frame.height) * scale).round().max(1.0);
    Rectangle::new(
        Point::new(
            bounds.center_x() - width / 2.0,
            bounds.center_y() - height / 2.0,
        ),
        Size::new(width, height),
    )
}

pub fn map_remote_position(
    bounds: Rectangle,
    point: Point,
    frame: Size<u16>,
    zoom: f32,
) -> Option<(u16, u16)> {
    let viewport = fitted_viewport(bounds, frame, zoom);
    if !viewport.contains(point) {
        return None;
    }
    let x = (((point.x - viewport.x) / viewport.width) * f32::from(frame.width))
        .floor()
        .clamp(0.0, f32::from(frame.width.saturating_sub(1))) as u16;
    let y = (((point.y - viewport.y) / viewport.height) * f32::from(frame.height))
        .floor()
        .clamp(0.0, f32::from(frame.height.saturating_sub(1))) as u16;
    Some((x, y))
}

#[derive(Debug, Default)]
pub struct ClipboardSync {
    local: Option<String>,
    remote: Option<String>,
    initialized: bool,
}

impl ClipboardSync {
    pub fn apply_remote(&mut self, text: String) -> String {
        self.initialized = true;
        self.local = Some(text.clone());
        self.remote = Some(text.clone());
        text
    }

    pub fn observe_local(&mut self, text: Option<String>) -> Option<String> {
        let text = text?;
        if !self.initialized {
            self.initialized = true;
            self.local = Some(text);
            return None;
        }
        let changed = self.local.as_deref() != Some(text.as_str());
        let from_remote = self.remote.as_deref() == Some(text.as_str());
        self.local = Some(text.clone());
        if from_remote {
            self.remote = None;
            None
        } else if changed {
            self.remote = None;
            Some(text)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    CursorMoved(Option<(u16, u16)>),
    ButtonPressed(Button),
    ButtonReleased(Button),
    Wheel(ScrollDelta),
    KeyPressed {
        key: Key,
        physical: Physical,
        location: Location,
        modifiers: Modifiers,
    },
    KeyReleased {
        key: Key,
        physical: Physical,
        location: Location,
        modifiers: Modifiers,
    },
    ModifiersChanged(Modifiers),
    ImeOpened,
    ImePreedit(String),
    ImeCommit(String),
    ImeClosed,
    FocusLost,
}

#[derive(Debug)]
pub struct InputState {
    input: Option<ArdClientInput>,
    dispatcher: InputDispatcher,
    button_mask: u8,
    pressed_buttons: HashMap<Button, u8>,
    cursor: Option<(u16, u16)>,
    scroll: ScrollAccumulator,
    modifiers: Modifiers,
    pressed_keys: HashMap<Physical, u32>,
    ime_suppressed: HashSet<Physical>,
    shortcut_suppressed: HashSet<Physical>,
    paste_suppressed: HashSet<Physical>,
    ime_active: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            input: None,
            dispatcher: InputDispatcher::new(),
            button_mask: 0,
            pressed_buttons: HashMap::new(),
            cursor: None,
            scroll: ScrollAccumulator::default(),
            modifiers: Modifiers::default(),
            pressed_keys: HashMap::new(),
            ime_suppressed: HashSet::new(),
            shortcut_suppressed: HashSet::new(),
            paste_suppressed: HashSet::new(),
            ime_active: false,
        }
    }
}

#[derive(Debug)]
enum InputCommand {
    Key { pressed: bool, keysym: u32 },
    Pointer { mask: u8, x: u16, y: u16 },
    PointerBatch(Vec<(u8, u16, u16)>),
    Clipboard(String),
}

#[derive(Debug)]
struct InputDispatcher {
    sender: SyncSender<InputCommand>,
    input: Arc<Mutex<Option<ArdClientInput>>>,
}

impl InputDispatcher {
    fn new() -> Self {
        let (sender, receiver) = sync_channel(MAX_INPUT_COMMANDS);
        let input: Arc<Mutex<Option<ArdClientInput>>> = Arc::new(Mutex::new(None));
        let worker_input = Arc::clone(&input);
        thread::Builder::new()
            .name("ard-input-dispatch".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let input = worker_input.lock().ok().and_then(|input| input.clone());
                    match command {
                        InputCommand::Key { pressed, keysym } => {
                            if let Some(input) = &input {
                                let _ = input.send_key_event(pressed, keysym);
                            }
                        }
                        InputCommand::Pointer { mask, x, y } => {
                            if let Some(input) = &input {
                                let _ = input.send_pointer_event(mask, x, y);
                            }
                        }
                        InputCommand::PointerBatch(events) => {
                            if let Some(input) = &input {
                                let _ = input.send_pointer_events(&events);
                            }
                        }
                        InputCommand::Clipboard(text) => {
                            if let Some(input) = &input {
                                let _ = input.send_clipboard_text(&text);
                            }
                        }
                    }
                }
            })
            .expect("ARD input dispatcher should start");
        Self { sender, input }
    }

    fn set_input(&self, input: Option<ArdClientInput>) {
        if let Ok(mut current) = self.input.lock() {
            *current = input;
        }
    }

    fn submit(&self, command: InputCommand) -> Result<(), String> {
        self.sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => "远程输入缓存已满".to_owned(),
            TrySendError::Disconnected(_) => "远程输入调度器已停止".to_owned(),
        })
    }
}

impl InputState {
    pub fn set_input(&mut self, input: ArdClientInput) {
        self.input = Some(input.clone());
        self.dispatcher.set_input(Some(input));
    }
    pub fn clear_input(&mut self) {
        self.release_all();
        self.input = None;
        self.dispatcher.set_input(None);
    }
    pub fn is_ready(&self) -> bool {
        self.input.is_some()
    }
    pub fn suppress_paste(&mut self, physical: Physical) {
        self.paste_suppressed.insert(physical);
    }
    pub fn send_clipboard(&self, text: &str) -> Result<(), String> {
        if self.input.is_none() {
            return Err("远程输入尚未就绪".to_owned());
        }
        self.dispatcher
            .submit(InputCommand::Clipboard(text.to_owned()))
    }

    pub fn handle(&mut self, event: InputEvent, capture_shortcuts: bool) -> Result<(), String> {
        match event {
            InputEvent::CursorMoved(position) => {
                self.cursor = position;
                if let (Some(input), Some((x, y))) = (&self.input, position) {
                    let _ = input.try_send_pointer_event(self.button_mask, x, y);
                }
            }
            InputEvent::ButtonPressed(button) => self.handle_button(button, true)?,
            InputEvent::ButtonReleased(button) => self.handle_button(button, false)?,
            InputEvent::Wheel(delta) => self.handle_wheel(delta)?,
            InputEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers,
            InputEvent::KeyPressed {
                key,
                physical,
                location,
                modifiers,
            } => {
                self.modifiers = modifiers;
                if is_paste_shortcut(&key, modifiers) {
                    return Ok(());
                }
                if !capture_shortcuts && is_system_shortcut(physical, &key, modifiers) {
                    self.suppress_shortcut(physical)?;
                    return Ok(());
                }
                if self.ime_active && is_textual_key(&key) {
                    self.ime_suppressed.insert(physical);
                    return Ok(());
                }
                if let Some(keysym) = key_event_keysym(&key, physical, location) {
                    self.pressed_keys.insert(physical, keysym);
                    self.send_key(true, keysym)?;
                }
            }
            InputEvent::KeyReleased {
                key,
                physical,
                location,
                modifiers,
            } => {
                self.modifiers = modifiers;
                if self.paste_suppressed.remove(&physical)
                    || self.shortcut_suppressed.remove(&physical)
                    || self.ime_suppressed.remove(&physical)
                {
                    return Ok(());
                }
                if let Some(keysym) = self
                    .pressed_keys
                    .remove(&physical)
                    .or_else(|| key_event_keysym(&key, physical, location))
                {
                    self.send_key(false, keysym)?;
                }
            }
            InputEvent::ImeOpened => self.ime_active = true,
            InputEvent::ImePreedit(text) => self.ime_active = !text.is_empty(),
            InputEvent::ImeCommit(text) => {
                self.ime_active = false;
                for character in text.chars() {
                    if let Some(keysym) = keysym_for_key(ArdKey::Character(character)) {
                        self.send_key(true, keysym)?;
                        self.send_key(false, keysym)?;
                    }
                }
            }
            InputEvent::ImeClosed => self.ime_active = false,
            InputEvent::FocusLost => self.release_all(),
        }
        Ok(())
    }

    fn send_key(&self, pressed: bool, keysym: u32) -> Result<(), String> {
        self.dispatcher
            .submit(InputCommand::Key { pressed, keysym })
    }

    fn handle_button(&mut self, button: Button, pressed: bool) -> Result<(), String> {
        let bit = if pressed {
            mouse_button_bit(button, self.modifiers)
        } else {
            self.pressed_buttons
                .get(&button)
                .copied()
                .or_else(|| mouse_button_bit(button, self.modifiers))
        };
        let (Some(bit), Some((x, y))) = (bit, self.cursor) else {
            return Ok(());
        };
        if pressed {
            self.pressed_buttons.insert(button, bit);
            self.button_mask |= bit;
        } else {
            self.pressed_buttons.remove(&button);
            self.button_mask &= !bit;
        }
        self.dispatcher.submit(InputCommand::Pointer {
            mask: self.button_mask,
            x,
            y,
        })?;
        Ok(())
    }

    fn handle_wheel(&mut self, delta: ScrollDelta) -> Result<(), String> {
        let Some((x, y)) = self.cursor else {
            self.scroll.reset();
            return Ok(());
        };
        let (horizontal, vertical) = self.scroll.update(delta);
        if self.input.is_none() {
            return Ok(());
        }
        for (clicks, negative, positive) in [(vertical, 0x08, 0x10), (horizontal, 0x20, 0x40)] {
            if clicks == 0 {
                continue;
            }
            let bit = if clicks.is_negative() {
                negative
            } else {
                positive
            };
            let mut events = Vec::with_capacity(clicks.unsigned_abs() as usize * 2);
            for _ in 0..clicks.unsigned_abs() {
                events.push((self.button_mask | bit, x, y));
                events.push((self.button_mask, x, y));
            }
            self.dispatcher.submit(InputCommand::PointerBatch(events))?;
        }
        Ok(())
    }

    fn suppress_shortcut(&mut self, physical: Physical) -> Result<(), String> {
        self.shortcut_suppressed.insert(physical);
        let modifiers: Vec<_> = self
            .pressed_keys
            .keys()
            .copied()
            .filter(|key| is_modifier_key(*key))
            .collect();
        for key in modifiers {
            self.shortcut_suppressed.insert(key);
            if let Some(keysym) = self.pressed_keys.remove(&key) {
                self.send_key(false, keysym)?;
            }
        }
        Ok(())
    }

    pub fn release_all(&mut self) {
        let keys = std::mem::take(&mut self.pressed_keys);
        if self.input.is_some() {
            for keysym in keys.values().copied() {
                let _ = self.dispatcher.submit(InputCommand::Key {
                    pressed: false,
                    keysym,
                });
            }
            if self.button_mask != 0
                && let Some((x, y)) = self.cursor
            {
                let _ = self
                    .dispatcher
                    .submit(InputCommand::Pointer { mask: 0, x, y });
            }
        }
        self.button_mask = 0;
        self.pressed_buttons.clear();
        self.ime_suppressed.clear();
        self.shortcut_suppressed.clear();
        self.paste_suppressed.clear();
        self.scroll.reset();
    }
}

#[derive(Debug, Default)]
struct ScrollAccumulator {
    horizontal: f64,
    vertical: f64,
}
impl ScrollAccumulator {
    fn update(&mut self, delta: ScrollDelta) -> (i32, i32) {
        let (x, y) = match delta {
            ScrollDelta::Lines { x, y } => (f64::from(x), f64::from(y)),
            ScrollDelta::Pixels { x, y } => (
                f64::from(x) / PRECISE_SCROLL_PIXELS_PER_LINE,
                f64::from(y) / PRECISE_SCROLL_PIXELS_PER_LINE,
            ),
        };
        if !x.is_finite() || !y.is_finite() {
            return (0, 0);
        }
        self.horizontal += x;
        self.vertical += y;
        (
            take_scroll(&mut self.horizontal),
            take_scroll(&mut self.vertical),
        )
    }
    fn reset(&mut self) {
        self.horizontal = 0.0;
        self.vertical = 0.0;
    }
}

pub fn reverse_scroll_delta(delta: ScrollDelta) -> ScrollDelta {
    match delta {
        ScrollDelta::Lines { x, y } => ScrollDelta::Lines { x: -x, y: -y },
        ScrollDelta::Pixels { x, y } => ScrollDelta::Pixels { x: -x, y: -y },
    }
}

pub fn scale_scroll_delta(delta: ScrollDelta, multiplier: f32) -> ScrollDelta {
    match delta {
        ScrollDelta::Lines { x, y } => ScrollDelta::Lines {
            x: x * multiplier,
            y: y * multiplier,
        },
        ScrollDelta::Pixels { x, y } => ScrollDelta::Pixels {
            x: x * multiplier,
            y: y * multiplier,
        },
    }
}

fn take_scroll(value: &mut f64) -> i32 {
    let clicks = value
        .trunc()
        .clamp(-MAX_SCROLL_CLICKS_PER_EVENT, MAX_SCROLL_CLICKS_PER_EVENT);
    *value -= clicks;
    clicks as i32
}

pub fn mouse_button_bit(button: Button, modifiers: Modifiers) -> Option<u8> {
    if cfg!(target_os = "macos") && button == Button::Left && modifiers.control() {
        return Some(0x04);
    }
    match button {
        Button::Left => Some(0x01),
        Button::Middle => Some(0x02),
        Button::Right => Some(0x04),
        Button::Back => Some(0x20),
        Button::Forward => Some(0x40),
        Button::Other(_) => None,
    }
}

pub fn is_paste_shortcut(key: &Key, modifiers: Modifiers) -> bool {
    if modifiers.logo() && !cfg!(target_os = "macos") {
        return false;
    }
    if matches!(key, Key::Named(Named::Paste)) {
        return true;
    }
    if (!modifiers.control() && !modifiers.logo()) || modifiers.alt() {
        return false;
    }
    matches!(key.as_ref(), Key::Character(text) if text.chars().next().is_some_and(|c| c.eq_ignore_ascii_case(&'v')))
}

pub fn is_system_shortcut(physical: Physical, key: &Key, modifiers: Modifiers) -> bool {
    let super_key = matches!(physical, Physical::Code(Code::SuperLeft | Code::SuperRight))
        || matches!(key, Key::Named(Named::Super));
    if super_key || modifiers.logo() {
        return true;
    }
    let named = |expected| matches!(key, Key::Named(actual) if *actual == expected);
    (modifiers.alt() && (named(Named::Tab) || named(Named::F4) || named(Named::Space)))
        || (modifiers.control()
            && (named(Named::Escape) || (modifiers.alt() && named(Named::Delete))))
}

fn is_modifier_key(key: Physical) -> bool {
    matches!(
        key,
        Physical::Code(
            Code::AltLeft
                | Code::AltRight
                | Code::ControlLeft
                | Code::ControlRight
                | Code::ShiftLeft
                | Code::ShiftRight
                | Code::SuperLeft
                | Code::SuperRight
        )
    )
}
fn is_textual_key(key: &Key) -> bool {
    matches!(key, Key::Character(_))
}

fn key_event_keysym(key: &Key, physical: Physical, location: Location) -> Option<u32> {
    match key.as_ref() {
        Key::Character(text) => text
            .chars()
            .next()
            .and_then(|c| keysym_for_key(ArdKey::Character(c))),
        Key::Named(key) => {
            named_key_to_ard(key, location).and_then(|key| keysym_for_key(ArdKey::Named(key)))
        }
        Key::Unidentified => None,
    }
    .or_else(|| physical_key_to_keysym(physical))
}

fn named_key_to_ard(key: Named, location: Location) -> Option<ArdNamedKey> {
    let left = matches!(location, Location::Left | Location::Standard);
    Some(match key {
        Named::Backspace => ArdNamedKey::Backspace,
        Named::Tab => ArdNamedKey::Tab,
        Named::Enter => ArdNamedKey::Enter,
        Named::Space => ArdNamedKey::Space,
        Named::Escape => ArdNamedKey::Escape,
        Named::Delete => ArdNamedKey::Delete,
        Named::Insert => ArdNamedKey::Insert,
        Named::Home => ArdNamedKey::Home,
        Named::End => ArdNamedKey::End,
        Named::PageUp => ArdNamedKey::PageUp,
        Named::PageDown => ArdNamedKey::PageDown,
        Named::ArrowLeft => ArdNamedKey::ArrowLeft,
        Named::ArrowUp => ArdNamedKey::ArrowUp,
        Named::ArrowRight => ArdNamedKey::ArrowRight,
        Named::ArrowDown => ArdNamedKey::ArrowDown,
        Named::Shift => {
            if left {
                ArdNamedKey::ShiftLeft
            } else {
                ArdNamedKey::ShiftRight
            }
        }
        Named::Control => {
            if left {
                ArdNamedKey::ControlLeft
            } else {
                ArdNamedKey::ControlRight
            }
        }
        Named::Alt | Named::AltGraph => {
            if left && key != Named::AltGraph {
                ArdNamedKey::AltLeft
            } else {
                ArdNamedKey::AltRight
            }
        }
        Named::Super => {
            if left {
                ArdNamedKey::SuperLeft
            } else {
                ArdNamedKey::SuperRight
            }
        }
        Named::Meta => {
            if left {
                ArdNamedKey::MetaLeft
            } else {
                ArdNamedKey::MetaRight
            }
        }
        Named::CapsLock => ArdNamedKey::CapsLock,
        Named::NumLock => ArdNamedKey::NumLock,
        Named::ScrollLock => ArdNamedKey::ScrollLock,
        Named::PrintScreen => ArdNamedKey::PrintScreen,
        Named::Pause => ArdNamedKey::Pause,
        Named::ContextMenu => ArdNamedKey::ContextMenu,
        Named::F1 => ArdNamedKey::Function(1),
        Named::F2 => ArdNamedKey::Function(2),
        Named::F3 => ArdNamedKey::Function(3),
        Named::F4 => ArdNamedKey::Function(4),
        Named::F5 => ArdNamedKey::Function(5),
        Named::F6 => ArdNamedKey::Function(6),
        Named::F7 => ArdNamedKey::Function(7),
        Named::F8 => ArdNamedKey::Function(8),
        Named::F9 => ArdNamedKey::Function(9),
        Named::F10 => ArdNamedKey::Function(10),
        Named::F11 => ArdNamedKey::Function(11),
        Named::F12 => ArdNamedKey::Function(12),
        Named::F13 => ArdNamedKey::Function(13),
        Named::F14 => ArdNamedKey::Function(14),
        Named::F15 => ArdNamedKey::Function(15),
        Named::F16 => ArdNamedKey::Function(16),
        Named::F17 => ArdNamedKey::Function(17),
        Named::F18 => ArdNamedKey::Function(18),
        Named::F19 => ArdNamedKey::Function(19),
        Named::F20 => ArdNamedKey::Function(20),
        Named::F21 => ArdNamedKey::Function(21),
        Named::F22 => ArdNamedKey::Function(22),
        Named::F23 => ArdNamedKey::Function(23),
        Named::F24 => ArdNamedKey::Function(24),
        Named::F25 => ArdNamedKey::Function(25),
        Named::F26 => ArdNamedKey::Function(26),
        Named::F27 => ArdNamedKey::Function(27),
        Named::F28 => ArdNamedKey::Function(28),
        Named::F29 => ArdNamedKey::Function(29),
        Named::F30 => ArdNamedKey::Function(30),
        Named::F31 => ArdNamedKey::Function(31),
        Named::F32 => ArdNamedKey::Function(32),
        Named::F33 => ArdNamedKey::Function(33),
        Named::F34 => ArdNamedKey::Function(34),
        Named::F35 => ArdNamedKey::Function(35),
        _ => return None,
    })
}

fn physical_key_to_keysym(key: Physical) -> Option<u32> {
    let Physical::Code(code) = key else {
        return None;
    };
    let ard = match code {
        Code::Backquote => ArdKey::Character('`'),
        Code::Backslash | Code::IntlBackslash | Code::IntlRo | Code::IntlYen => {
            ArdKey::Character('\\')
        }
        Code::BracketLeft => ArdKey::Character('['),
        Code::BracketRight => ArdKey::Character(']'),
        Code::Comma => ArdKey::Character(','),
        Code::Digit0 => ArdKey::Character('0'),
        Code::Digit1 => ArdKey::Character('1'),
        Code::Digit2 => ArdKey::Character('2'),
        Code::Digit3 => ArdKey::Character('3'),
        Code::Digit4 => ArdKey::Character('4'),
        Code::Digit5 => ArdKey::Character('5'),
        Code::Digit6 => ArdKey::Character('6'),
        Code::Digit7 => ArdKey::Character('7'),
        Code::Digit8 => ArdKey::Character('8'),
        Code::Digit9 => ArdKey::Character('9'),
        Code::Equal => ArdKey::Character('='),
        Code::Minus => ArdKey::Character('-'),
        Code::Period => ArdKey::Character('.'),
        Code::Quote => ArdKey::Character('\''),
        Code::Semicolon => ArdKey::Character(';'),
        Code::Slash => ArdKey::Character('/'),
        Code::KeyA => ArdKey::Character('a'),
        Code::KeyB => ArdKey::Character('b'),
        Code::KeyC => ArdKey::Character('c'),
        Code::KeyD => ArdKey::Character('d'),
        Code::KeyE => ArdKey::Character('e'),
        Code::KeyF => ArdKey::Character('f'),
        Code::KeyG => ArdKey::Character('g'),
        Code::KeyH => ArdKey::Character('h'),
        Code::KeyI => ArdKey::Character('i'),
        Code::KeyJ => ArdKey::Character('j'),
        Code::KeyK => ArdKey::Character('k'),
        Code::KeyL => ArdKey::Character('l'),
        Code::KeyM => ArdKey::Character('m'),
        Code::KeyN => ArdKey::Character('n'),
        Code::KeyO => ArdKey::Character('o'),
        Code::KeyP => ArdKey::Character('p'),
        Code::KeyQ => ArdKey::Character('q'),
        Code::KeyR => ArdKey::Character('r'),
        Code::KeyS => ArdKey::Character('s'),
        Code::KeyT => ArdKey::Character('t'),
        Code::KeyU => ArdKey::Character('u'),
        Code::KeyV => ArdKey::Character('v'),
        Code::KeyW => ArdKey::Character('w'),
        Code::KeyX => ArdKey::Character('x'),
        Code::KeyY => ArdKey::Character('y'),
        Code::KeyZ => ArdKey::Character('z'),
        Code::Space => ArdKey::Named(ArdNamedKey::Space),
        Code::Backspace => ArdKey::Named(ArdNamedKey::Backspace),
        Code::CapsLock => ArdKey::Named(ArdNamedKey::CapsLock),
        Code::ControlLeft => ArdKey::Named(ArdNamedKey::ControlLeft),
        Code::ControlRight => ArdKey::Named(ArdNamedKey::ControlRight),
        Code::AltLeft => ArdKey::Named(ArdNamedKey::AltLeft),
        Code::AltRight => ArdKey::Named(ArdNamedKey::AltRight),
        Code::ShiftLeft => ArdKey::Named(ArdNamedKey::ShiftLeft),
        Code::ShiftRight => ArdKey::Named(ArdNamedKey::ShiftRight),
        Code::SuperLeft => ArdKey::Named(ArdNamedKey::SuperLeft),
        Code::SuperRight => ArdKey::Named(ArdNamedKey::SuperRight),
        Code::Enter => ArdKey::Named(ArdNamedKey::Enter),
        Code::Tab => ArdKey::Named(ArdNamedKey::Tab),
        Code::Escape => ArdKey::Named(ArdNamedKey::Escape),
        Code::Delete => ArdKey::Named(ArdNamedKey::Delete),
        Code::End => ArdKey::Named(ArdNamedKey::End),
        Code::Home => ArdKey::Named(ArdNamedKey::Home),
        Code::Insert => ArdKey::Named(ArdNamedKey::Insert),
        Code::PageDown => ArdKey::Named(ArdNamedKey::PageDown),
        Code::PageUp => ArdKey::Named(ArdNamedKey::PageUp),
        Code::ArrowDown => ArdKey::Named(ArdNamedKey::ArrowDown),
        Code::ArrowLeft => ArdKey::Named(ArdNamedKey::ArrowLeft),
        Code::ArrowRight => ArdKey::Named(ArdNamedKey::ArrowRight),
        Code::ArrowUp => ArdKey::Named(ArdNamedKey::ArrowUp),
        Code::NumLock => ArdKey::Named(ArdNamedKey::NumLock),
        Code::PrintScreen => ArdKey::Named(ArdNamedKey::PrintScreen),
        Code::ScrollLock => ArdKey::Named(ArdNamedKey::ScrollLock),
        Code::Pause => ArdKey::Named(ArdNamedKey::Pause),
        Code::ContextMenu => ArdKey::Named(ArdNamedKey::ContextMenu),
        Code::F1 => ArdKey::Named(ArdNamedKey::Function(1)),
        Code::F2 => ArdKey::Named(ArdNamedKey::Function(2)),
        Code::F3 => ArdKey::Named(ArdNamedKey::Function(3)),
        Code::F4 => ArdKey::Named(ArdNamedKey::Function(4)),
        Code::F5 => ArdKey::Named(ArdNamedKey::Function(5)),
        Code::F6 => ArdKey::Named(ArdNamedKey::Function(6)),
        Code::F7 => ArdKey::Named(ArdNamedKey::Function(7)),
        Code::F8 => ArdKey::Named(ArdNamedKey::Function(8)),
        Code::F9 => ArdKey::Named(ArdNamedKey::Function(9)),
        Code::F10 => ArdKey::Named(ArdNamedKey::Function(10)),
        Code::F11 => ArdKey::Named(ArdNamedKey::Function(11)),
        Code::F12 => ArdKey::Named(ArdNamedKey::Function(12)),
        Code::F13 => ArdKey::Named(ArdNamedKey::Function(13)),
        Code::F14 => ArdKey::Named(ArdNamedKey::Function(14)),
        Code::F15 => ArdKey::Named(ArdNamedKey::Function(15)),
        Code::F16 => ArdKey::Named(ArdNamedKey::Function(16)),
        Code::F17 => ArdKey::Named(ArdNamedKey::Function(17)),
        Code::F18 => ArdKey::Named(ArdNamedKey::Function(18)),
        Code::F19 => ArdKey::Named(ArdNamedKey::Function(19)),
        Code::F20 => ArdKey::Named(ArdNamedKey::Function(20)),
        Code::F21 => ArdKey::Named(ArdNamedKey::Function(21)),
        Code::F22 => ArdKey::Named(ArdNamedKey::Function(22)),
        Code::F23 => ArdKey::Named(ArdNamedKey::Function(23)),
        Code::F24 => ArdKey::Named(ArdNamedKey::Function(24)),
        Code::F25 => ArdKey::Named(ArdNamedKey::Function(25)),
        Code::F26 => ArdKey::Named(ArdNamedKey::Function(26)),
        Code::F27 => ArdKey::Named(ArdNamedKey::Function(27)),
        Code::F28 => ArdKey::Named(ArdNamedKey::Function(28)),
        Code::F29 => ArdKey::Named(ArdNamedKey::Function(29)),
        Code::F30 => ArdKey::Named(ArdNamedKey::Function(30)),
        Code::F31 => ArdKey::Named(ArdNamedKey::Function(31)),
        Code::F32 => ArdKey::Named(ArdNamedKey::Function(32)),
        Code::F33 => ArdKey::Named(ArdNamedKey::Function(33)),
        Code::F34 => ArdKey::Named(ArdNamedKey::Function(34)),
        Code::F35 => ArdKey::Named(ArdNamedKey::Function(35)),
        Code::Numpad0 => ArdKey::Named(ArdNamedKey::Numpad(0)),
        Code::Numpad1 => ArdKey::Named(ArdNamedKey::Numpad(1)),
        Code::Numpad2 => ArdKey::Named(ArdNamedKey::Numpad(2)),
        Code::Numpad3 => ArdKey::Named(ArdNamedKey::Numpad(3)),
        Code::Numpad4 => ArdKey::Named(ArdNamedKey::Numpad(4)),
        Code::Numpad5 => ArdKey::Named(ArdNamedKey::Numpad(5)),
        Code::Numpad6 => ArdKey::Named(ArdNamedKey::Numpad(6)),
        Code::Numpad7 => ArdKey::Named(ArdNamedKey::Numpad(7)),
        Code::Numpad8 => ArdKey::Named(ArdNamedKey::Numpad(8)),
        Code::Numpad9 => ArdKey::Named(ArdNamedKey::Numpad(9)),
        Code::NumpadAdd => ArdKey::Named(ArdNamedKey::NumpadAdd),
        Code::NumpadSubtract => ArdKey::Named(ArdNamedKey::NumpadSubtract),
        Code::NumpadMultiply => ArdKey::Named(ArdNamedKey::NumpadMultiply),
        Code::NumpadDivide => ArdKey::Named(ArdNamedKey::NumpadDivide),
        Code::NumpadDecimal | Code::NumpadComma => ArdKey::Named(ArdNamedKey::NumpadDecimal),
        Code::NumpadEnter => ArdKey::Named(ArdNamedKey::NumpadEnter),
        Code::NumpadEqual => ArdKey::Named(ArdNamedKey::NumpadEqual),
        _ => return None,
    };
    keysym_for_key(ard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ard_rs::PixelFormat;

    #[test]
    fn connection_states_have_real_labels() {
        assert_eq!(
            ConnectionState::Connecting.label(Language::SimplifiedChinese),
            "正在连接…"
        );
        assert!(
            ConnectionState::Reconnecting { attempt: 2 }
                .label(Language::SimplifiedChinese)
                .contains("2/5")
        );
        assert!(
            ConnectionState::Disconnected("timeout".into())
                .label(Language::SimplifiedChinese)
                .contains("timeout")
        );
    }

    #[test]
    fn scroll_direction_can_be_reversed_on_both_axes() {
        assert_eq!(
            reverse_scroll_delta(ScrollDelta::Lines { x: 2.0, y: -3.0 }),
            ScrollDelta::Lines { x: -2.0, y: 3.0 }
        );
        assert_eq!(
            reverse_scroll_delta(ScrollDelta::Pixels { x: -4.0, y: 5.0 }),
            ScrollDelta::Pixels { x: 4.0, y: -5.0 }
        );
    }

    #[test]
    fn scroll_delta_can_be_scaled_on_both_axes() {
        assert_eq!(
            scale_scroll_delta(ScrollDelta::Lines { x: 2.0, y: -3.0 }, 3.0),
            ScrollDelta::Lines { x: 6.0, y: -9.0 }
        );
        assert_eq!(
            scale_scroll_delta(ScrollDelta::Pixels { x: -4.0, y: 5.0 }, 2.0),
            ScrollDelta::Pixels { x: -8.0, y: 10.0 }
        );
    }

    #[test]
    fn mailbox_is_latest_only_and_event_queue_is_bounded() {
        let mut mailbox = FrameMailbox::default();
        for value in 0..100 {
            mailbox.replace_latest(FramePacket::from_rgba(
                1,
                1,
                vec![value, 0, 0, 255],
                ArdVideoQuality::Full,
            ));
            mailbox.push_event(SessionEvent::State(ConnectionState::Connected));
        }
        assert_eq!(
            mailbox.latest.as_ref().unwrap().rgba.as_ref().unwrap()[0],
            99
        );
        assert!(mailbox.events.len() <= MAX_EVENTS);
        assert!(matches!(
            mailbox.events.back(),
            Some(SessionEvent::State(ConnectionState::Connected))
        ));
        assert!(mailbox.rgba_pool.len() <= MAX_RGBA_POOL);
    }

    #[test]
    fn tile_set_coalesces_same_tile_without_stale_geometry() {
        let mut tiles = TileSet::new(16, 16, 2);
        tiles.insert(MvsGpuTileUpdate {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        });
        tiles.insert(MvsGpuTileUpdate {
            x: 1,
            y: 1,
            width: 8,
            height: 8,
            tile: MvsGpuTile::SolidRgba([1, 2, 3, 255]),
        });

        let update = tiles.first().expect("tile was inserted");
        assert_eq!(tiles.len(), 1);
        assert_eq!((update.x, update.y), (1, 1));
        assert_eq!((update.width, update.height), (8, 8));
    }

    #[test]
    fn tile_set_from_updates_preserves_known_good_coalescing() {
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
        let unique = TileSet::from_updates(16, 8, vec![first.clone(), second]);
        assert_eq!(unique.len(), 2);
        assert_eq!(unique.dirty_len(), 2);

        let replacement = MvsGpuTileUpdate {
            tile: MvsGpuTile::SolidRgba([7, 8, 9, 255]),
            ..first.clone()
        };
        let duplicate = TileSet::from_updates(16, 8, vec![first, replacement.clone()]);
        assert_eq!(duplicate.len(), 1);
        assert_eq!(duplicate.first(), Some(&replacement));
    }

    #[test]
    fn native_framebuffer_converts_to_rgba() {
        let mut framebuffer = Framebuffer::new_native(1, 1, PixelFormat::XRGB8888).unwrap();
        framebuffer.pixels_mut().copy_from_slice(&[192, 96, 32, 0]);
        let mut rgba = Vec::new();
        assert!(framebuffer_to_rgba(&framebuffer, &mut rgba));
        assert_eq!(rgba, [32, 96, 192, 255]);
    }

    #[test]
    fn coordinates_respect_letterboxing_and_zoom() {
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(1000.0, 1000.0));
        assert_eq!(
            map_remote_position(bounds, Point::new(500.0, 500.0), Size::new(1920, 1080), 1.0),
            Some((960, 540))
        );
        assert_eq!(
            map_remote_position(bounds, Point::new(500.0, 100.0), Size::new(1920, 1080), 1.0),
            None
        );
    }

    #[test]
    fn control_click_and_shortcuts_map_correctly() {
        let expected = if cfg!(target_os = "macos") {
            Some(0x04)
        } else {
            Some(0x01)
        };
        assert_eq!(mouse_button_bit(Button::Left, Modifiers::CTRL), expected);
        assert!(is_system_shortcut(
            Physical::Code(Code::SuperLeft),
            &Key::Named(Named::Super),
            Modifiers::NONE
        ));
        assert!(is_paste_shortcut(
            &Key::Character("v".into()),
            Modifiers::COMMAND
        ));
    }

    #[test]
    fn clipboard_does_not_echo_remote_updates() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.apply_remote("remote".into()), "remote");
        assert_eq!(sync.observe_local(Some("remote".into())), None);
        assert_eq!(
            sync.observe_local(Some("local".into())),
            Some("local".into())
        );
        assert_eq!(sync.observe_local(Some("local".into())), None);
    }

    #[test]
    fn clipboard_baselines_existing_local_contents() {
        let mut sync = ClipboardSync::default();
        assert_eq!(sync.observe_local(Some("existing".into())), None);
        assert_eq!(
            sync.observe_local(Some("changed".into())),
            Some("changed".into())
        );
    }

    #[test]
    fn paste_release_is_suppressed_with_its_local_shortcut() {
        let physical = Physical::Code(Code::KeyV);
        let mut input = InputState::default();
        input.suppress_paste(physical);
        input
            .handle(
                InputEvent::KeyReleased {
                    key: Key::Character("v".into()),
                    physical,
                    location: Location::Standard,
                    modifiers: Modifiers::COMMAND,
                },
                false,
            )
            .unwrap();
        assert!(input.paste_suppressed.is_empty());
    }
}
