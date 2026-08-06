#![deny(unsafe_code)]

mod config;
mod icons;
mod session_renderer;
mod session_runtime;
mod state;
mod theme;
mod views;
mod widgets;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::{Duration, Instant};

use ard_rs::ArdVideoQuality;
use iced::widget::{column, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Element, Fill, Subscription, Task, Theme, window};

use session_runtime::{
    ClipboardSync, ConnectionState, InputEvent, InputState, SessionConfig, SessionEvent,
    SessionRuntime, StreamMetrics, is_paste_shortcut, map_remote_position,
};

use state::{DeviceState, KeyMapping, SavedDevice, SettingsSection, ThemePreference, WindowKind};

const SESSION_TOOLBAR_WIDTH: f32 = 286.0;
const SESSION_TOOLBAR_COLLAPSED_WIDTH: f32 = 44.0;
const SETTINGS_WINDOW_OFFSET: f32 = 32.0;
const SESSION_IME_ID: &str = "session-ime-sink";

#[derive(Debug)]
struct ArdViewer {
    windows: BTreeMap<window::Id, WindowKind>,
    maximized_windows: BTreeSet<window::Id>,
    devices: Vec<SavedDevice>,
    selected_device: usize,
    previous_selected_device: usize,
    device_transition: f32,
    search: String,
    address: String,
    port: String,
    username: String,
    password: String,
    password_visible: bool,
    has_saved_password: bool,
    remember_password: bool,
    remember_device: bool,
    quality: ArdVideoQuality,
    frame_rate: String,
    settings_section: SettingsSection,
    settings_transition: f32,
    key_profile: String,
    auto_adapt_keyboard: bool,
    capture_system_shortcuts: bool,
    show_performance_hud: bool,
    theme_preference: ThemePreference,
    system_dark: bool,
    mappings: Vec<KeyMapping>,
    session_zoom: f32,
    session_runtime: Option<SessionRuntime>,
    session_connection: ConnectionState,
    session_metrics: StreamMetrics,
    session_server_name: String,
    session_error: Option<String>,
    session_input: InputState,
    session_clipboard: ClipboardSync,
    session_window_size: iced::Size,
    session_pointer_remote: Option<(u16, u16)>,
    ime_sink: String,
    session_fullscreen: bool,
    session_toolbar_visible: bool,
    session_toolbar_progress: f32,
    session_toolbar_pinned: bool,
    session_toolbar_last_interaction: Instant,
    session_toolbar_x: Option<f32>,
    session_toolbar_window_width: f32,
    session_toolbar_pointer_x: f32,
    session_toolbar_drag_offset: f32,
    session_toolbar_dragging: bool,
    pending_close: Option<window::Id>,
    close_modal_target_visible: bool,
    close_modal_progress: f32,
    animation_clock: Instant,
    ui_time: f32,
    status: String,
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(WindowKind, window::Id),
    WindowClosed(window::Id),
    WindowEvent(window::Id, window::Event),
    WindowMaximizedChanged(window::Id, bool),
    InitialSystemTheme(iced::theme::Mode),
    CloseRequested(window::Id),
    CancelClose,
    ConfirmClose,
    #[cfg(not(target_os = "macos"))]
    CloseWindow(window::Id),
    DragWindow(window::Id),
    #[cfg(not(target_os = "macos"))]
    MinimizeWindow(window::Id),
    ToggleMaximizeWindow(window::Id),
    OpenSettings,
    SearchChanged(String),
    DeviceSelected(usize),
    AddressChanged(String),
    PortChanged(String),
    UsernameChanged(String),
    PasswordChanged(String),
    TogglePasswordVisibility,
    RememberPasswordChanged(bool),
    RememberDeviceChanged(bool),
    SaveDevice,
    Connect,
    ManageDevices,
    ExportShortcuts,
    SettingsSectionSelected(SettingsSection),
    QualityChanged(ArdVideoQuality),
    FrameRateChanged(String),
    KeyProfileChanged(String),
    AutoAdaptChanged(bool),
    CaptureShortcutsChanged(bool),
    PerformanceHudChanged(bool),
    ThemePreferenceChanged(ThemePreference),
    SystemThemeChanged(iced::theme::Mode),
    CopyPreset,
    ResetMappings,
    AddMapping,
    EditMapping(usize),
    SessionAction(SessionAction),
    ToggleFullscreen,
    SessionToolbarTick(Instant),
    AnimationTick(Instant),
    ShowSessionToolbar,
    HideSessionToolbar,
    SessionToolbarInteraction,
    SessionToolbarPointerMoved(iced::Point),
    SessionToolbarDragStarted,
    SessionToolbarDragEnded,
    ToggleSessionToolbarPin,
    SessionPoll,
    SessionClipboardPoll,
    SessionRawEvent(window::Id, iced::Event, iced::event::Status),
    ClipboardRead(Option<String>),
    ImeSinkChanged(String),
}

#[derive(Debug, Clone, Copy)]
enum SessionAction {
    Fit,
    Zoom,
    Undo,
    Input,
    Clipboard,
    SystemShortcut,
}

impl ArdViewer {
    fn new() -> (Self, Task<Message>) {
        let cached = config::AppConfig::load();
        let devices = config::devices_from_cache(&cached);
        let address = if cached.last_address.is_empty() {
            devices
                .first()
                .map_or_else(String::new, |device| device.address.clone())
        } else {
            cached.last_address.clone()
        };
        let (address, port) = split_endpoint(&address);
        let endpoint = format_endpoint(&address, &port);
        let username = if cached.last_username.is_empty() {
            devices
                .first()
                .map_or_else(String::new, |device| device.username.clone())
        } else {
            cached.last_username.clone()
        };
        let has_saved_password =
            cached.remember_password && config::load_password(&endpoint, &username).is_some();
        let mappings = profile_mappings(&cached.key_profile);
        let app = Self {
            windows: BTreeMap::new(),
            maximized_windows: BTreeSet::new(),
            address,
            port,
            username,
            devices,
            selected_device: 0,
            previous_selected_device: 0,
            device_transition: 1.0,
            search: String::new(),
            password: String::new(),
            password_visible: false,
            has_saved_password,
            remember_password: cached.remember_password,
            remember_device: cached.remember_device,
            quality: config::quality_from_cache(&cached.quality),
            frame_rate: if cached.frame_rate.is_empty() {
                frame_rate_from_interval(&cached.frame_interval_ms)
            } else {
                cached.frame_rate.clone()
            },
            settings_section: SettingsSection::KeyMapping,
            settings_transition: 1.0,
            key_profile: cached.key_profile,
            auto_adapt_keyboard: cached.auto_adapt_keyboard,
            capture_system_shortcuts: cached.capture_system_shortcuts,
            show_performance_hud: cached.show_performance_hud,
            theme_preference: config::theme_from_cache(&cached.theme),
            system_dark: false,
            mappings,
            session_zoom: 1.0,
            session_runtime: None,
            session_connection: ConnectionState::Idle,
            session_metrics: StreamMetrics::default(),
            session_server_name: String::new(),
            session_error: None,
            session_input: InputState::default(),
            session_clipboard: ClipboardSync::default(),
            session_window_size: WindowKind::Session.size(),
            session_pointer_remote: None,
            ime_sink: String::new(),
            session_fullscreen: false,
            session_toolbar_visible: true,
            session_toolbar_progress: 1.0,
            session_toolbar_pinned: false,
            session_toolbar_last_interaction: Instant::now(),
            session_toolbar_x: None,
            session_toolbar_window_width: WindowKind::Session.size().width,
            session_toolbar_pointer_x: WindowKind::Session.size().width / 2.0,
            session_toolbar_drag_offset: 0.0,
            session_toolbar_dragging: false,
            pending_close: None,
            close_modal_target_visible: false,
            close_modal_progress: 0.0,
            animation_clock: Instant::now(),
            ui_time: 0.0,
            status: String::new(),
        };
        theme::set_dark(app.effective_dark());
        (app, iced::system::theme().map(Message::InitialSystemTheme))
    }

    fn title(&self, id: window::Id) -> String {
        match self.windows.get(&id) {
            Some(WindowKind::Connection) => "ARD Viewer — Connect",
            Some(WindowKind::Settings) => "ARD Viewer — Settings",
            Some(WindowKind::Session) if !self.session_server_name.is_empty() => {
                return format!("{} — ARD Viewer", self.session_server_name);
            }
            Some(WindowKind::Session) => "ARD Viewer — Session",
            None => "ARD Viewer",
        }
        .to_owned()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(kind, id) => {
                self.windows.insert(id, kind);
                if kind == WindowKind::Session {
                    return disable_implicit_titlebar_drag(id);
                }
            }
            Message::WindowClosed(id) => {
                self.maximized_windows.remove(&id);
                if self.pending_close == Some(id) {
                    self.pending_close = None;
                }
                let closed = self.windows.remove(&id);
                if closed == Some(WindowKind::Session) {
                    self.disconnect_session();
                }
                if closed == Some(WindowKind::Connection) || self.windows.is_empty() {
                    self.disconnect_session();
                    return iced::exit();
                }
            }
            Message::WindowEvent(id, window::Event::Resized(size)) => {
                if self.windows.get(&id) == Some(&WindowKind::Session) {
                    self.session_toolbar_window_width = size.width;
                    self.session_window_size = size;
                    self.clamp_session_toolbar_position();
                }
                return window::is_maximized(id)
                    .map(move |maximized| Message::WindowMaximizedChanged(id, maximized));
            }
            Message::WindowEvent(_, _) => {}
            Message::WindowMaximizedChanged(id, maximized) => {
                if maximized {
                    self.maximized_windows.insert(id);
                } else {
                    self.maximized_windows.remove(&id);
                }
            }
            Message::InitialSystemTheme(mode) => {
                self.system_dark = mode == iced::theme::Mode::Dark;
                theme::set_dark(self.effective_dark());
                return open_window(WindowKind::Connection);
            }
            Message::CloseRequested(id) => {
                if self.windows.get(&id) == Some(&WindowKind::Settings) {
                    return window::close(id);
                }
                if self.windows.contains_key(&id) {
                    self.pending_close = Some(id);
                    self.close_modal_target_visible = true;
                    self.close_modal_progress = 0.0;
                    return reveal_and_focus(id);
                }
            }
            Message::CancelClose => {
                self.close_modal_target_visible = false;
                if self.close_modal_progress <= 0.001 {
                    self.pending_close = None;
                }
            }
            Message::ConfirmClose => {
                if let Some(id) = self.pending_close.take() {
                    self.close_modal_target_visible = false;
                    self.close_modal_progress = 0.0;
                    return window::close(id);
                }
            }
            #[cfg(not(target_os = "macos"))]
            Message::CloseWindow(id) => {
                if self.windows.get(&id) == Some(&WindowKind::Settings) {
                    return window::close(id);
                }
                self.pending_close = Some(id);
                self.close_modal_target_visible = true;
                self.close_modal_progress = 0.0;
                return reveal_and_focus(id);
            }
            Message::DragWindow(id) => return window::drag(id),
            #[cfg(not(target_os = "macos"))]
            Message::MinimizeWindow(id) => return window::minimize(id, true),
            Message::ToggleMaximizeWindow(id) => {
                let maximized = !self.maximized_windows.contains(&id);
                if maximized {
                    self.maximized_windows.insert(id);
                } else {
                    self.maximized_windows.remove(&id);
                }
                return window::maximize(id, maximized);
            }
            Message::OpenSettings => {
                if let Some(id) = self.window_id(WindowKind::Settings) {
                    return window::gain_focus(id);
                }
                if let Some(id) = self.window_id(WindowKind::Connection) {
                    return window::position(id).then(|position| {
                        open_window_at(
                            WindowKind::Settings,
                            position.map_or(window::Position::Centered, |position| {
                                window::Position::Specific(iced::Point::new(
                                    position.x + SETTINGS_WINDOW_OFFSET,
                                    position.y + SETTINGS_WINDOW_OFFSET,
                                ))
                            }),
                        )
                    });
                }
                return open_window(WindowKind::Settings);
            }
            Message::SearchChanged(value) => self.search = value,
            Message::DeviceSelected(index) => {
                if let Some(device) = self.devices.get(index) {
                    if index != self.selected_device {
                        self.previous_selected_device = self.selected_device;
                        self.device_transition = 0.0;
                    }
                    self.selected_device = index;
                    (self.address, self.port) = split_endpoint(&device.address);
                    self.username.clone_from(&device.username);
                    self.password.clear();
                    self.has_saved_password = self.remember_password && {
                        let endpoint = format_endpoint(&self.address, &self.port);
                        config::load_password(&endpoint, &self.username).is_some()
                    };
                }
            }
            Message::AddressChanged(value) => {
                let (address, port) = split_endpoint(&value);
                self.address = address;
                if value.trim() != self.address.trim() {
                    self.port = port;
                }
            }
            Message::PortChanged(value) => {
                self.port = value.chars().filter(char::is_ascii_digit).take(5).collect();
            }
            Message::UsernameChanged(value) => {
                self.username = value;
                self.password.clear();
                self.has_saved_password = false;
            }
            Message::PasswordChanged(value) => {
                self.password = value;
                self.has_saved_password = false;
            }
            Message::TogglePasswordVisibility => {
                self.password_visible = !self.password_visible;
            }
            Message::RememberPasswordChanged(value) => {
                self.remember_password = value;
                let address = self
                    .remote_endpoint()
                    .unwrap_or_else(|_| self.address.trim().to_owned());
                if !value && let Err(error) = config::save_password(&address, &self.username, None)
                {
                    self.status = format!("移除系统密钥库密码失败：{error}");
                }
                if !value {
                    self.has_saved_password = false;
                }
                self.persist_config();
            }
            Message::RememberDeviceChanged(value) => {
                self.remember_device = value;
                self.persist_config();
            }
            Message::SaveDevice => {
                let Ok(address) = self.remote_endpoint() else {
                    self.status = "请输入有效的设备地址和端口".into();
                    return Task::none();
                };
                if let Some(index) = self
                    .devices
                    .iter()
                    .position(|device| device.address.eq_ignore_ascii_case(&address))
                {
                    self.selected_device = index;
                    self.devices[index].username = self.username.trim().to_owned();
                    self.status = "历史连接已更新".into();
                } else {
                    let (name, _) = split_endpoint(&address);
                    self.devices.push(SavedDevice {
                        name,
                        address: address.clone(),
                        username: self.username.trim().to_owned(),
                        state: DeviceState::Saved,
                    });
                    self.previous_selected_device = self.selected_device;
                    self.selected_device = self.devices.len() - 1;
                    self.device_transition = 0.0;
                    self.status = "已加入历史连接".into();
                }
                self.store_credentials();
                self.persist_config();
            }
            Message::SettingsSectionSelected(section) => {
                if section != self.settings_section {
                    self.settings_section = section;
                    self.settings_transition = 0.0;
                }
            }
            Message::QualityChanged(quality) => {
                self.quality = quality;
                self.persist_config();
            }
            Message::FrameRateChanged(value) => {
                let value: String = value.chars().filter(char::is_ascii_digit).take(3).collect();
                self.frame_rate = value.parse::<u16>().map_or(value.clone(), |rate| {
                    if rate == 0 {
                        String::new()
                    } else {
                        rate.min(240).to_string()
                    }
                });
                self.persist_config();
            }
            Message::KeyProfileChanged(value) => {
                self.mappings = profile_mappings(&value);
                self.key_profile = value;
                self.persist_config();
            }
            Message::AutoAdaptChanged(value) => {
                self.auto_adapt_keyboard = value;
                self.persist_config();
            }
            Message::CaptureShortcutsChanged(value) => {
                self.capture_system_shortcuts = value;
                self.persist_config();
            }
            Message::PerformanceHudChanged(value) => {
                self.show_performance_hud = value;
                self.persist_config();
            }
            Message::ThemePreferenceChanged(preference) => {
                self.theme_preference = preference;
                theme::set_dark(self.effective_dark());
                self.persist_config();
            }
            Message::SystemThemeChanged(mode) => {
                self.system_dark = mode == iced::theme::Mode::Dark;
                if self.theme_preference == ThemePreference::System {
                    theme::set_dark(self.system_dark);
                }
            }
            Message::ResetMappings => {
                self.mappings = default_mappings();
                self.status = "按键映射已恢复默认".into();
            }
            Message::ToggleFullscreen => {
                self.touch_session_toolbar();
                self.session_fullscreen = !self.session_fullscreen;
                if let Some(id) = self.window_id(WindowKind::Session) {
                    let mode = if self.session_fullscreen {
                        window::Mode::Fullscreen
                    } else {
                        window::Mode::Windowed
                    };
                    return window::set_mode(id, mode);
                }
            }
            Message::SessionAction(SessionAction::Fit) => {
                self.touch_session_toolbar();
                self.session_zoom = 1.0;
            }
            Message::SessionAction(SessionAction::Zoom) => {
                self.touch_session_toolbar();
                self.session_zoom = if self.session_zoom >= 1.4 {
                    0.6
                } else {
                    self.session_zoom + 0.1
                }
            }
            Message::Connect => {
                if self.address.trim().is_empty() || self.username.trim().is_empty() {
                    self.status = "请输入设备地址和用户名".into();
                } else if self.remote_endpoint().is_err() {
                    self.status = "请输入有效端口（1–65535）".into();
                } else if self.password.is_empty() && !self.has_saved_password {
                    self.status = "请输入密码".into();
                } else {
                    let endpoint = self.remote_endpoint().expect("endpoint validated above");
                    let password = if self.password.is_empty() {
                        config::load_password(&endpoint, &self.username).unwrap_or_default()
                    } else {
                        self.password.clone()
                    };
                    if password.is_empty() {
                        self.has_saved_password = false;
                        self.status = "保存的密码不可用，请重新输入".into();
                        return Task::none();
                    }
                    if self.remember_device {
                        self.upsert_history(&endpoint);
                    }
                    self.store_credentials();
                    self.persist_config();
                    self.start_session(password);
                    if let Some(id) = self.window_id(WindowKind::Session) {
                        return window::gain_focus(id);
                    }
                    return open_window(WindowKind::Session);
                }
            }
            Message::ManageDevices => {
                if !self.devices.is_empty() {
                    let removed = self
                        .devices
                        .remove(self.selected_device.min(self.devices.len() - 1));
                    if let Err(error) =
                        config::save_password(&removed.address, &removed.username, None)
                    {
                        self.status = format!("移除系统密钥库密码失败：{error}");
                    }
                    self.selected_device = self
                        .selected_device
                        .min(self.devices.len().saturating_sub(1));
                    self.previous_selected_device = self.selected_device;
                    if !self.status.starts_with("移除系统密钥库密码失败") {
                        self.status = "已移除所选历史连接".into();
                    }
                    self.persist_config();
                }
            }
            Message::ExportShortcuts => self.export_shortcuts(),
            Message::CopyPreset => {
                self.key_profile = format!("{} 副本", self.key_profile.trim_end_matches(" 副本"));
                self.status = "已复制按键预设".into();
                self.persist_config();
            }
            Message::AddMapping => {
                self.mappings.push(KeyMapping {
                    local: "F11".into(),
                    remote: "显示桌面".into(),
                    scope: "会话".into(),
                });
                self.status = "已添加常用映射".into();
            }
            Message::EditMapping(index) => {
                if index < self.mappings.len() {
                    self.mappings.remove(index);
                    self.status = "已移除按键映射".into();
                }
            }
            Message::SessionAction(SessionAction::Undo) => {
                self.session_zoom = 1.0;
                self.touch_session_toolbar();
            }
            Message::SessionAction(SessionAction::Input) => {
                self.touch_session_toolbar();
                self.status = "远程键鼠输入已启用".into();
                if let Some(id) = self.window_id(WindowKind::Session) {
                    return Task::batch([
                        window::gain_focus(id),
                        iced::widget::operation::focus(iced::widget::Id::new(SESSION_IME_ID)),
                    ]);
                }
            }
            Message::SessionAction(SessionAction::Clipboard) => {
                self.touch_session_toolbar();
                return read_clipboard_text();
            }
            Message::SessionAction(SessionAction::SystemShortcut) => {
                self.touch_session_toolbar();
                self.capture_system_shortcuts = !self.capture_system_shortcuts;
                self.persist_config();
                self.status = if self.capture_system_shortcuts {
                    "系统快捷键将发送到远端".into()
                } else {
                    "系统快捷键保留在本机".into()
                };
            }
            Message::SessionToolbarTick(now) => {
                if self.session_toolbar_visible
                    && !self.session_toolbar_pinned
                    && !self.session_toolbar_dragging
                    && now.duration_since(self.session_toolbar_last_interaction)
                        >= Duration::from_secs(4)
                {
                    self.session_toolbar_visible = false;
                }
            }
            Message::AnimationTick(now) => {
                let delta = now
                    .saturating_duration_since(self.animation_clock)
                    .as_secs_f32()
                    .min(0.05);
                self.animation_clock = now;
                self.ui_time = (self.ui_time + delta) % 60.0;
                self.device_transition = advance(self.device_transition, 1.0, delta, 8.5);
                self.settings_transition = advance(self.settings_transition, 1.0, delta, 7.5);
                self.session_toolbar_progress = advance(
                    self.session_toolbar_progress,
                    if self.session_toolbar_visible {
                        1.0
                    } else {
                        0.0
                    },
                    delta,
                    10.0,
                );
                self.close_modal_progress = advance(
                    self.close_modal_progress,
                    if self.close_modal_target_visible {
                        1.0
                    } else {
                        0.0
                    },
                    delta,
                    12.0,
                );
                if !self.close_modal_target_visible && self.close_modal_progress <= 0.001 {
                    self.pending_close = None;
                }
            }
            Message::ShowSessionToolbar | Message::SessionToolbarInteraction => {
                self.touch_session_toolbar();
            }
            Message::SessionToolbarPointerMoved(point) => {
                self.session_toolbar_pointer_x = point.x;
                if self.session_toolbar_dragging {
                    self.session_toolbar_x = Some(point.x - self.session_toolbar_drag_offset);
                    self.clamp_session_toolbar_position();
                    self.touch_session_toolbar();
                }
            }
            Message::SessionToolbarDragStarted => {
                let current_x = self
                    .session_toolbar_x
                    .unwrap_or(self.session_toolbar_window_width / 2.0);
                self.session_toolbar_drag_offset = self.session_toolbar_pointer_x - current_x;
                self.session_toolbar_dragging = true;
                self.touch_session_toolbar();
            }
            Message::SessionToolbarDragEnded => {
                if self.session_toolbar_dragging {
                    self.session_toolbar_dragging = false;
                    self.touch_session_toolbar();
                }
            }
            Message::HideSessionToolbar => {
                self.session_toolbar_visible = false;
                self.session_toolbar_dragging = false;
            }
            Message::ToggleSessionToolbarPin => {
                self.session_toolbar_pinned = !self.session_toolbar_pinned;
                self.touch_session_toolbar();
            }
            Message::SessionPoll => {
                let events = self
                    .session_runtime
                    .as_ref()
                    .map(SessionRuntime::drain_events)
                    .unwrap_or_default();
                let mut tasks = Vec::new();
                for event in events {
                    tasks.push(self.handle_session_event(event));
                }
                return Task::batch(tasks);
            }
            Message::SessionClipboardPoll => {
                return read_clipboard_text();
            }
            Message::SessionRawEvent(id, event, event_status) => {
                return self.handle_session_raw_event(id, event, event_status);
            }
            Message::ClipboardRead(contents) => {
                if let Some(text) = self.session_clipboard.observe_local(contents)
                    && self.session_input.is_ready()
                    && let Err(error) = self.session_input.send_clipboard(&text)
                {
                    self.session_error = Some(format!("发送剪贴板失败：{error}"));
                }
            }
            Message::ImeSinkChanged(value) => {
                self.ime_sink = value
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
            }
        }
        Task::none()
    }

    fn view(&self, id: window::Id) -> Element<'_, Message> {
        theme::set_dark(self.effective_dark());
        let Some(kind) = self.windows.get(&id).copied() else {
            return iced::widget::space().into();
        };
        let content = match kind {
            WindowKind::Connection => views::connection(self, id),
            WindowKind::Settings => views::settings(self, id),
            WindowKind::Session => views::session(self, id),
        };
        if self.pending_close == Some(id) {
            close_confirmation(content, kind, self.close_modal_progress)
        } else {
            content
        }
    }

    fn theme(&self, _id: window::Id) -> Option<Theme> {
        Some(theme::app_theme())
    }
    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::close_requests().map(Message::CloseRequested),
            window::close_events().map(Message::WindowClosed),
            window::events().map(|(id, event)| Message::WindowEvent(id, event)),
            iced::system::theme_changes().map(Message::SystemThemeChanged),
            iced::event::listen_with(session_event_subscription),
        ];
        let session_open = self.window_id(WindowKind::Session).is_some();
        if session_open {
            subscriptions.push(
                iced::time::every(Duration::from_millis(250)).map(Message::SessionToolbarTick),
            );
            subscriptions.push(
                iced::time::every(Duration::from_millis(250))
                    .map(|_| Message::SessionClipboardPoll),
            );
            if let Some(runtime) = &self.session_runtime {
                subscriptions.push(runtime.frame_subscription().map(|_| Message::SessionPoll));
            }
        }
        let toolbar_target = if self.session_toolbar_visible {
            1.0
        } else {
            0.0
        };
        let modal_target = if self.close_modal_target_visible {
            1.0
        } else {
            0.0
        };
        let transition_active = self.device_transition < 0.999
            || self.settings_transition < 0.999
            || (self.session_toolbar_progress - toolbar_target).abs() > 0.001
            || (self.close_modal_progress - modal_target).abs() > 0.001
            || matches!(
                self.session_connection,
                ConnectionState::Connecting | ConnectionState::Reconnecting { .. }
            );
        if transition_active {
            subscriptions
                .push(iced::time::every(Duration::from_millis(16)).map(Message::AnimationTick));
        }
        Subscription::batch(subscriptions)
    }
    fn window_id(&self, kind: WindowKind) -> Option<window::Id> {
        self.windows
            .iter()
            .find_map(|(id, current)| (*current == kind).then_some(*id))
    }

    fn touch_session_toolbar(&mut self) {
        self.session_toolbar_visible = true;
        self.session_toolbar_last_interaction = Instant::now();
    }

    fn is_window_maximized(&self, id: window::Id) -> bool {
        self.maximized_windows.contains(&id)
    }

    fn effective_dark(&self) -> bool {
        match self.theme_preference {
            ThemePreference::System => self.system_dark,
            ThemePreference::Light => false,
            ThemePreference::Dark => true,
        }
    }

    fn remote_endpoint(&self) -> Result<String, ()> {
        let address = self.address.trim();
        let (parsed_host, parsed_port) = split_endpoint(address);
        let has_embedded_port = parsed_host != address;
        if !has_embedded_port && address.matches(':').count() == 1 {
            return Err(());
        }
        let host = if has_embedded_port {
            parsed_host.as_str()
        } else {
            address
        };
        let port_text = if has_embedded_port {
            parsed_port.as_str()
        } else if self.port.trim().is_empty() {
            "5900"
        } else {
            self.port.trim()
        };
        let port = port_text.parse::<u16>().map_err(|_| ())?;
        if host.is_empty() || port == 0 {
            return Err(());
        }
        Ok(format_endpoint(host, &port.to_string()))
    }

    fn clamp_session_toolbar_position(&mut self) {
        const TOOLBAR_HALF_WIDTH: f32 = SESSION_TOOLBAR_WIDTH / 2.0;
        if let Some(x) = self.session_toolbar_x.as_mut() {
            *x = x.clamp(
                TOOLBAR_HALF_WIDTH,
                (self.session_toolbar_window_width - TOOLBAR_HALF_WIDTH).max(TOOLBAR_HALF_WIDTH),
            );
        }
    }

    fn persist_config(&mut self) {
        let cached = config::AppConfig {
            devices: self
                .devices
                .iter()
                .map(|device| config::CachedDevice {
                    name: device.name.clone(),
                    address: device.address.clone(),
                    username: device.username.clone(),
                })
                .collect(),
            last_address: self
                .remote_endpoint()
                .unwrap_or_else(|_| self.address.trim().to_owned()),
            last_username: self.username.trim().to_owned(),
            remember_password: self.remember_password,
            remember_device: self.remember_device,
            quality: config::quality_to_cache(self.quality).into(),
            frame_rate: self.frame_rate.clone(),
            frame_interval_ms: frame_interval_from_rate(&self.frame_rate).to_string(),
            key_profile: self.key_profile.clone(),
            auto_adapt_keyboard: self.auto_adapt_keyboard,
            capture_system_shortcuts: self.capture_system_shortcuts,
            show_performance_hud: self.show_performance_hud,
            theme: config::theme_to_cache(self.theme_preference).into(),
        };
        if let Err(error) = cached.save() {
            self.status = format!("保存配置失败：{error}");
        }
    }

    fn store_credentials(&mut self) {
        if self.remember_password && self.password.is_empty() {
            return;
        }
        let password = self.remember_password.then_some(self.password.as_str());
        let address = self
            .remote_endpoint()
            .unwrap_or_else(|_| self.address.trim().to_owned());
        if let Err(error) = config::save_password(&address, &self.username, password) {
            self.status = format!("系统密钥库不可用：{error}");
        } else {
            self.has_saved_password = password.is_some();
        }
    }

    fn start_session(&mut self, password: String) {
        self.disconnect_session();
        self.session_connection = ConnectionState::Connecting;
        self.session_metrics = StreamMetrics::default();
        self.session_server_name.clear();
        self.session_error = None;
        self.session_zoom = 1.0;
        self.session_runtime = Some(SessionRuntime::start(SessionConfig {
            address: self
                .remote_endpoint()
                .expect("validated before starting a session"),
            username: self.username.trim().to_owned(),
            password: password.into_bytes(),
            quality: self.quality,
            frame_interval: frame_duration_from_rate(&self.frame_rate),
        }));
        self.status = "正在当前 Session 窗口中连接…".into();
    }

    fn upsert_history(&mut self, endpoint: &str) {
        if let Some(index) = self
            .devices
            .iter()
            .position(|device| device.address.eq_ignore_ascii_case(endpoint))
        {
            self.devices[index].username = self.username.trim().to_owned();
            self.devices[index].state = DeviceState::RecentlyUsed;
            self.selected_device = index;
            return;
        }
        let (name, _) = split_endpoint(endpoint);
        self.devices.push(SavedDevice {
            name,
            address: endpoint.to_owned(),
            username: self.username.trim().to_owned(),
            state: DeviceState::RecentlyUsed,
        });
        self.previous_selected_device = self.selected_device;
        self.selected_device = self.devices.len() - 1;
    }

    fn disconnect_session(&mut self) {
        if let Some(mut runtime) = self.session_runtime.take() {
            runtime.disconnect();
        }
        self.session_input.clear_input();
        self.session_pointer_remote = None;
        self.session_connection = ConnectionState::Idle;
    }

    fn handle_session_event(&mut self, event: SessionEvent) -> Task<Message> {
        match event {
            SessionEvent::State(state) => {
                if matches!(
                    state,
                    ConnectionState::Disconnected(_)
                        | ConnectionState::Reconnecting { .. }
                        | ConnectionState::Failed(_)
                ) {
                    self.session_input.clear_input();
                }
                self.status = state.label();
                self.session_connection = state;
            }
            SessionEvent::Connected { server_name, input } => {
                self.session_server_name = server_name;
                self.session_input.set_input(input);
                self.session_error = None;
            }
            SessionEvent::Clipboard(text) => {
                let text = self.session_clipboard.apply_remote(text);
                return iced::clipboard::write(text).discard();
            }
            SessionEvent::Metrics(metrics) => self.session_metrics = metrics,
            SessionEvent::RenderFailed(error) => {
                self.session_error = Some(format!("渲染失败：{error}"));
            }
        }
        Task::none()
    }

    fn handle_session_raw_event(
        &mut self,
        id: window::Id,
        event: iced::Event,
        event_status: iced::event::Status,
    ) -> Task<Message> {
        if self.window_id(WindowKind::Session) != Some(id) || self.session_runtime.is_none() {
            return Task::none();
        }
        let input_event = match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                let canvas = self.session_canvas_bounds();
                let position = map_remote_position(
                    canvas,
                    position,
                    iced::Size::new(self.session_metrics.width, self.session_metrics.height),
                    self.session_zoom,
                );
                self.session_pointer_remote = position;
                Some(InputEvent::CursorMoved(position))
            }
            iced::Event::Mouse(iced::mouse::Event::CursorLeft) => {
                self.session_pointer_remote = None;
                Some(InputEvent::CursorMoved(None))
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(button))
                if event_status == iced::event::Status::Ignored =>
            {
                Some(InputEvent::ButtonPressed(button))
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(button))
                if event_status == iced::event::Status::Ignored =>
            {
                Some(InputEvent::ButtonReleased(button))
            }
            iced::Event::Mouse(iced::mouse::Event::WheelScrolled { delta })
                if event_status == iced::event::Status::Ignored =>
            {
                Some(InputEvent::Wheel(delta))
            }
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) => {
                Some(InputEvent::ModifiersChanged(modifiers))
            }
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                location,
                modifiers,
                ..
            }) => {
                if is_paste_shortcut(&key, modifiers) {
                    self.session_input.suppress_paste(physical_key);
                    return read_clipboard_text();
                }
                Some(InputEvent::KeyPressed {
                    key,
                    physical: physical_key,
                    location,
                    modifiers,
                })
            }
            iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
                key,
                physical_key,
                location,
                modifiers,
                ..
            }) => Some(InputEvent::KeyReleased {
                key,
                physical: physical_key,
                location,
                modifiers,
            }),
            iced::Event::InputMethod(iced::advanced::input_method::Event::Opened) => {
                Some(InputEvent::ImeOpened)
            }
            iced::Event::InputMethod(iced::advanced::input_method::Event::Preedit(text, _)) => {
                Some(InputEvent::ImePreedit(text))
            }
            iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(text)) => {
                Some(InputEvent::ImeCommit(text))
            }
            iced::Event::InputMethod(iced::advanced::input_method::Event::Closed) => {
                Some(InputEvent::ImeClosed)
            }
            iced::Event::Window(window::Event::Unfocused) => Some(InputEvent::FocusLost),
            _ => None,
        };
        if let Some(event) = input_event
            && let Err(error) = self
                .session_input
                .handle(event, self.capture_system_shortcuts)
        {
            self.session_error = Some(format!("远程输入失败：{error}"));
        }
        Task::none()
    }

    fn session_canvas_bounds(&self) -> iced::Rectangle {
        iced::Rectangle::new(iced::Point::ORIGIN, self.session_window_size)
    }

    fn export_shortcuts(&mut self) {
        let Some(path) = config::export_path() else {
            self.status = "无法确定导出目录".into();
            return;
        };
        let mappings: Vec<_> = self.mappings.iter().map(|mapping| {
            serde_json::json!({"local": mapping.local, "remote": mapping.remote, "scope": mapping.scope})
        }).collect();
        let result = path
            .parent()
            .map(fs::create_dir_all)
            .transpose()
            .and_then(|_| {
                fs::write(
                    &path,
                    serde_json::to_vec_pretty(&mappings).expect("JSON serialization cannot fail"),
                )
            });
        self.status = match result {
            Ok(()) => format!("快捷方式已导出到 {}", path.display()),
            Err(error) => format!("导出失败：{error}"),
        };
    }
}

fn split_endpoint(value: &str) -> (String, String) {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[')
        && let Some((host, port)) = rest.rsplit_once("]:")
        && port.parse::<u16>().is_ok()
    {
        return (host.to_owned(), port.to_owned());
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.contains(':')
        && port.parse::<u16>().is_ok()
    {
        return (host.to_owned(), port.to_owned());
    }
    (value.to_owned(), "5900".to_owned())
}

fn format_endpoint(host: &str, port: &str) -> String {
    let host = host.trim();
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn frame_interval_from_rate(value: &str) -> u64 {
    let frames_per_second = value.trim().parse::<u64>().unwrap_or(0).min(240);
    if frames_per_second == 0 {
        0
    } else {
        (1000 / frames_per_second).max(1)
    }
}

fn frame_duration_from_rate(value: &str) -> Duration {
    let frames_per_second = value.trim().parse::<u16>().unwrap_or(0).min(240);
    if frames_per_second == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(1.0 / f64::from(frames_per_second))
    }
}

fn frame_rate_from_interval(value: &str) -> String {
    let interval = value.trim().parse::<u64>().unwrap_or(0);
    if interval == 0 {
        String::new()
    } else {
        (1000.0 / interval as f64)
            .round()
            .clamp(1.0, 240.0)
            .to_string()
    }
}

fn session_event_subscription(
    event: iced::Event,
    status: iced::event::Status,
    id: window::Id,
) -> Option<Message> {
    matches!(
        event,
        iced::Event::Keyboard(_)
            | iced::Event::Mouse(_)
            | iced::Event::InputMethod(_)
            | iced::Event::Window(window::Event::Unfocused)
    )
    .then_some(Message::SessionRawEvent(id, event, status))
}

fn close_confirmation<'a>(
    content: Element<'a, Message>,
    kind: WindowKind,
    progress: f32,
) -> Element<'a, Message> {
    let (title, description) = match kind {
        WindowKind::Connection => ("退出 ARD Viewer？", "打开的设置和远程会话也会一并关闭。"),
        WindowKind::Settings => ("关闭设置窗口？", "确认关闭当前设置窗口。"),
        WindowKind::Session => ("关闭远程会话？", "当前远程连接将会断开。"),
    };
    let progress = smoothstep(0.0, 1.0, progress);
    let text_color = theme::mix(iced::Color::TRANSPARENT, theme::palette().text, progress);
    let muted_color = theme::mix(
        iced::Color::TRANSPARENT,
        theme::palette().text_muted,
        progress,
    );
    let dialog = container(
        column![
            text(title).size(theme::TITLE_SIZE).color(text_color),
            text(description).size(theme::BODY_SIZE).color(muted_color),
            row![
                space().width(Fill),
                widgets::secondary("取消", Message::CancelClose).width(88),
                widgets::primary("关闭", Message::ConfirmClose).width(88),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        ]
        .spacing(18),
    )
    .width(360)
    .padding(22)
    .style(theme::modal_panel(progress));
    let overlay = mouse_area(
        container(dialog)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill)
            .style(theme::modal_backdrop(progress)),
    )
    .on_press(Message::CancelClose);
    stack![content, overlay].width(Fill).height(Fill).into()
}

fn advance(current: f32, target: f32, delta_seconds: f32, speed: f32) -> f32 {
    if (current - target).abs() <= 0.001 {
        return target;
    }
    current + (target - current) * (1.0 - (-speed * delta_seconds).exp())
}

fn smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    let value = ((value - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}

fn open_window(kind: WindowKind) -> Task<Message> {
    open_window_at(kind, window::Position::Centered)
}

fn open_window_at(kind: WindowKind, position: window::Position) -> Task<Message> {
    let size = kind.size();
    let (_, task) = window::open(window::Settings {
        size,
        min_size: Some(size),
        position,
        decorations: cfg!(target_os = "macos"),
        transparent: false,
        resizable: true,
        closeable: true,
        minimizable: true,
        exit_on_close_request: false,
        platform_specific: platform_window_settings(),
        ..window::Settings::default()
    });
    task.map(move |id| Message::WindowOpened(kind, id))
}

fn reveal_and_focus(id: window::Id) -> Task<Message> {
    Task::batch([window::minimize(id, false), window::gain_focus(id)])
}

fn read_clipboard_text() -> Task<Message> {
    iced::clipboard::read_text()
        .map(|result| Message::ClipboardRead(result.ok().map(|text| text.as_ref().clone())))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn disable_implicit_titlebar_drag(id: window::Id) -> Task<Message> {
    window::run(id, |window| {
        use iced::window::raw_window_handle::RawWindowHandle;
        use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
        use objc2::sel;

        extern "C" fn prevent_implicit_drag(_view: &AnyObject, _selector: Sel) -> Bool {
            Bool::NO
        }

        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };

        // winit supplies its live content view on the AppKit main thread.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<AnyObject>() };
        let class = AnyClass::get("ArdSessionView").unwrap_or_else(|| {
            let mut builder = ClassBuilder::new("ArdSessionView", view.class())
                .expect("ARD session view class should only be registered once");
            unsafe {
                builder.add_method(
                    sel!(mouseDownCanMoveWindow),
                    prevent_implicit_drag as extern "C" fn(_, _) -> _,
                );
            }
            builder.register()
        });

        // The subclass adds no state and only overrides AppKit's implicit
        // titlebar hit-test. NSWindow remains movable for `window::drag`.
        unsafe {
            AnyObject::set_class(view, class);
        }
    })
    .discard()
}

#[cfg(not(target_os = "macos"))]
fn disable_implicit_titlebar_drag(_id: window::Id) -> Task<Message> {
    Task::none()
}

#[cfg(target_os = "macos")]
fn platform_window_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        title_hidden: true,
        titlebar_transparent: true,
        fullsize_content_view: true,
    }
}

#[cfg(target_os = "windows")]
fn platform_window_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        undecorated_shadow: true,
        corner_preference: window::settings::platform::CornerPreference::Round,
        ..window::settings::PlatformSpecific::default()
    }
}

#[cfg(target_os = "linux")]
fn platform_window_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        application_id: "ard-viewer".to_owned(),
        override_redirect: false,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn platform_window_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific::default()
}

fn default_mappings() -> Vec<KeyMapping> {
    profile_mappings("macOS 默认")
}

fn profile_mappings(profile: &str) -> Vec<KeyMapping> {
    let (copy, paste, switch) = if profile.starts_with("Windows") || profile.starts_with("Linux") {
        ("Ctrl C", "Ctrl V", "Alt Tab")
    } else {
        ("⌘ C", "⌘ V", "⌘ `")
    };
    vec![
        KeyMapping {
            local: copy.into(),
            remote: "复制".into(),
            scope: "全局".into(),
        },
        KeyMapping {
            local: paste.into(),
            remote: "粘贴".into(),
            scope: "全局".into(),
        },
        KeyMapping {
            local: "⌘ ⌥ Esc".into(),
            remote: "强制退出".into(),
            scope: "macOS".into(),
        },
        KeyMapping {
            local: "Ctrl Alt Del".into(),
            remote: "安全选项".into(),
            scope: "Windows".into(),
        },
        KeyMapping {
            local: switch.into(),
            remote: "切换远程窗口".into(),
            scope: "会话".into(),
        },
    ]
}

fn main() -> iced::Result {
    iced::daemon(ArdViewer::new, ArdViewer::update, ArdViewer::view)
        .title(ArdViewer::title)
        .theme(ArdViewer::theme)
        .subscription(ArdViewer::subscription)
        .font(include_bytes!("../assets/Inter-Variable.ttf").as_slice())
        .default_font(iced::Font::new("Inter"))
        .antialiasing(true)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn app_starts_in_connection_window_state() {
        let (app, _task) = ArdViewer::new();
        assert_eq!(app.settings_section, SettingsSection::KeyMapping);
        assert_eq!(app.quality, ArdVideoQuality::Adaptive);
        assert_eq!(app.theme_preference, ThemePreference::System);
        assert!(app.session_toolbar_visible);
        assert!(!app.session_toolbar_pinned);
        assert_eq!(app.pending_close, None);
        assert_eq!(app.port, "5900");
    }

    #[test]
    fn endpoint_uses_the_separate_port_and_supports_pasted_addresses() {
        let (mut app, _task) = ArdViewer::new();
        app.address = "host.local".into();
        app.port = "5999".into();
        assert_eq!(app.remote_endpoint(), Ok("host.local:5999".into()));

        let _task = app.update(Message::AddressChanged("other.local:5901".into()));
        assert_eq!(app.address, "other.local");
        assert_eq!(app.port, "5901");
        assert_eq!(app.remote_endpoint(), Ok("other.local:5901".into()));

        app.address = "other.local:not-a-port".into();
        assert_eq!(app.remote_endpoint(), Err(()));
    }

    #[test]
    fn empty_port_uses_ard_default() {
        let (mut app, _task) = ArdViewer::new();
        app.address = "host.local".into();
        app.port.clear();

        assert_eq!(app.remote_endpoint(), Ok("host.local:5900".into()));
    }

    #[test]
    fn frame_rate_is_converted_to_protocol_interval() {
        assert_eq!(frame_interval_from_rate(""), 0);
        assert_eq!(frame_interval_from_rate("30"), 33);
        assert_eq!(frame_interval_from_rate("60"), 16);
        assert_eq!(frame_interval_from_rate("240"), 4);
        assert_eq!(
            frame_duration_from_rate("60"),
            Duration::from_nanos(16_666_667)
        );
        assert_eq!(frame_rate_from_interval("0"), "");
        assert_eq!(frame_rate_from_interval("16"), "63");
    }

    #[test]
    fn session_canvas_fills_the_window() {
        let (mut app, _task) = ArdViewer::new();
        app.session_window_size = iced::Size::new(1440.0, 900.0);
        assert_eq!(
            app.session_canvas_bounds(),
            iced::Rectangle::new(iced::Point::ORIGIN, app.session_window_size)
        );
    }

    #[test]
    fn connection_progress_is_centered_in_the_session_window() {
        let (mut app, _task) = ArdViewer::new();
        app.session_connection = ConnectionState::Connecting;
        let size = WindowKind::Session.size();
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            size,
            views::session(&app, window::Id::unique()),
        );
        let bounds = ui
            .find(iced::widget::Id::new("session-connection-progress"))
            .expect("connection progress should be present")
            .visible_bounds()
            .expect("connection progress should be visible");

        assert!((bounds.center_x() - size.width / 2.0).abs() < 1.0);
        assert!((bounds.center_y() - size.height / 2.0).abs() < 1.0);
    }

    #[test]
    fn save_device_adds_the_current_address_and_selects_it() {
        let (mut app, _task) = ArdViewer::new();
        app.address = "new-host.local:5900".into();
        let previous_len = app.devices.len();

        let _task = app.update(Message::SaveDevice);

        assert_eq!(app.devices.len(), previous_len + 1);
        assert_eq!(app.selected_device, previous_len);
        assert_eq!(app.devices[previous_len].name, "new-host.local");
        assert_eq!(app.devices[previous_len].state, DeviceState::Saved);
    }

    #[test]
    fn history_is_retained_when_auto_add_is_disabled() {
        let (mut app, _task) = ArdViewer::new();
        app.devices.push(SavedDevice {
            name: "history-host".into(),
            address: "history-host:5900".into(),
            username: "tester".into(),
            state: DeviceState::RecentlyUsed,
        });

        let _task = app.update(Message::RememberDeviceChanged(false));

        assert_eq!(app.devices.len(), 1);
        assert_eq!(app.devices[0].address, "history-host:5900");
    }

    #[test]
    fn close_request_requires_confirmation() {
        let (mut app, _task) = ArdViewer::new();
        let id = window::Id::unique();
        app.windows.insert(id, WindowKind::Session);

        let _task = app.update(Message::CloseRequested(id));
        assert_eq!(app.pending_close, Some(id));

        let _task = app.update(Message::CancelClose);
        assert_eq!(app.pending_close, None);
        assert!(app.windows.contains_key(&id));
    }

    #[test]
    fn settings_close_request_skips_confirmation() {
        let (mut app, _task) = ArdViewer::new();
        let id = window::Id::unique();
        app.windows.insert(id, WindowKind::Settings);

        let _task = app.update(Message::CloseRequested(id));

        assert_eq!(app.pending_close, None);
        assert!(!app.close_modal_target_visible);
    }

    #[test]
    fn session_toolbar_auto_collapses_unless_pinned() {
        let (mut app, _task) = ArdViewer::new();
        let now = Instant::now();
        app.session_toolbar_last_interaction = now - Duration::from_secs(5);

        let _task = app.update(Message::SessionToolbarTick(now));
        assert!(!app.session_toolbar_visible);

        let _task = app.update(Message::ToggleSessionToolbarPin);
        assert!(app.session_toolbar_visible);
        assert!(app.session_toolbar_pinned);

        app.session_toolbar_last_interaction = now - Duration::from_secs(5);
        let _task = app.update(Message::SessionToolbarTick(now));
        assert!(app.session_toolbar_visible);
    }

    #[test]
    fn session_toolbar_hide_animation_reaches_the_collapsed_state() {
        let (mut app, _task) = ArdViewer::new();
        let start = app.animation_clock;

        let _task = app.update(Message::HideSessionToolbar);
        for step in 1..=60 {
            let _task = app.update(Message::AnimationTick(
                start + Duration::from_millis(16 * step),
            ));
        }

        assert!(!app.session_toolbar_visible);
        assert!(app.session_toolbar_progress < 0.001);
    }

    #[test]
    fn session_toolbar_can_be_dragged_horizontally_and_stays_in_bounds() {
        let (mut app, _task) = ArdViewer::new();
        let id = window::Id::unique();
        app.windows.insert(id, WindowKind::Session);

        let _task = app.update(Message::SessionToolbarDragStarted);
        let _task = app.update(Message::SessionToolbarPointerMoved(iced::Point::new(
            900.0, 20.0,
        )));
        assert_eq!(app.session_toolbar_x, Some(900.0));

        let _task = app.update(Message::WindowEvent(
            id,
            window::Event::Resized(iced::Size::new(600.0, 500.0)),
        ));
        assert_eq!(app.session_toolbar_x, Some(457.0));

        let _task = app.update(Message::SessionToolbarDragEnded);
        assert!(!app.session_toolbar_dragging);
    }

    #[test]
    fn collapsed_and_expanded_toolbar_handles_share_the_same_axis() {
        let (mut app, _task) = ArdViewer::new();
        app.session_toolbar_x = Some(215.0);
        let window_id = window::Id::unique();
        let mut expanded = iced_test::Simulator::with_size(
            iced::Settings::default(),
            WindowKind::Session.size(),
            views::session(&app, window_id),
        );
        let collapse_x = expanded
            .find(iced::widget::Id::new("session-toolbar-collapse-handle"))
            .expect("collapse handle should be present")
            .visible_bounds()
            .expect("collapse handle should be visible")
            .center_x();
        drop(expanded);

        app.session_toolbar_visible = false;
        app.session_toolbar_progress = 0.0;
        let mut collapsed = iced_test::Simulator::with_size(
            iced::Settings::default(),
            WindowKind::Session.size(),
            views::session(&app, window_id),
        );
        let expand_x = collapsed
            .find(iced::widget::Id::new("session-toolbar-expand-handle"))
            .expect("expand handle should be present")
            .visible_bounds()
            .expect("expand handle should be visible")
            .center_x();

        assert!(
            (collapse_x - expand_x).abs() < f32::EPSILON,
            "collapse: {collapse_x}, expand: {expand_x}"
        );
    }

    #[test]
    fn maximized_window_state_is_tracked_for_square_outer_corners() {
        let (mut app, _task) = ArdViewer::new();
        let id = window::Id::unique();

        let _task = app.update(Message::WindowMaximizedChanged(id, true));
        assert!(app.is_window_maximized(id));

        let _task = app.update(Message::WindowMaximizedChanged(id, false));
        assert!(!app.is_window_maximized(id));
    }

    #[test]
    fn ui_transitions_start_and_converge_smoothly() {
        let (mut app, _task) = ArdViewer::new();
        app.devices = vec![
            SavedDevice {
                name: "A".into(),
                address: "a.local:5900".into(),
                username: "a".into(),
                state: DeviceState::Saved,
            },
            SavedDevice {
                name: "B".into(),
                address: "b.local:5900".into(),
                username: "b".into(),
                state: DeviceState::Saved,
            },
        ];
        let now = Instant::now();
        app.animation_clock = now;

        let _task = app.update(Message::DeviceSelected(1));
        let _task = app.update(Message::SettingsSectionSelected(SettingsSection::Display));
        assert_eq!(app.device_transition, 0.0);
        assert_eq!(app.settings_transition, 0.0);

        let _task = app.update(Message::AnimationTick(now + Duration::from_millis(16)));
        assert!(app.device_transition > 0.0 && app.device_transition < 1.0);
        assert!(app.settings_transition > 0.0 && app.settings_transition < 1.0);

        for step in 2..=120 {
            let _task = app.update(Message::AnimationTick(
                now + Duration::from_millis(16 * step),
            ));
        }
        assert!(app.device_transition > 0.999);
        assert!(app.settings_transition > 0.999);
    }

    #[test]
    fn theme_preference_can_follow_or_override_the_system() {
        let (mut app, _task) = ArdViewer::new();

        let _task = app.update(Message::SystemThemeChanged(iced::theme::Mode::Dark));
        assert!(app.effective_dark());

        let _task = app.update(Message::ThemePreferenceChanged(ThemePreference::Light));
        assert!(!app.effective_dark());

        let _task = app.update(Message::SystemThemeChanged(iced::theme::Mode::Light));
        let _task = app.update(Message::ThemePreferenceChanged(ThemePreference::Dark));
        assert!(app.effective_dark());
    }

    #[test]
    fn session_connection_transitions_cover_disconnect_and_reconnect() {
        let (mut app, _task) = ArdViewer::new();
        let _ = app.handle_session_event(SessionEvent::State(ConnectionState::Connecting));
        assert_eq!(app.session_connection, ConnectionState::Connecting);

        let _ = app.handle_session_event(SessionEvent::State(ConnectionState::Disconnected(
            "transport lost".into(),
        )));
        assert!(app.status.contains("transport lost"));

        let _ = app.handle_session_event(SessionEvent::State(ConnectionState::Reconnecting {
            attempt: 1,
        }));
        assert!(app.status.contains("正在重连"));

        let _ = app.handle_session_event(SessionEvent::State(ConnectionState::Connected));
        assert_eq!(app.session_connection, ConnectionState::Connected);
    }

    #[test]
    fn session_buttons_change_zoom_shortcut_and_toolbar_state() {
        let (mut app, _task) = ArdViewer::new();
        let _ = app.update(Message::SessionAction(SessionAction::Zoom));
        assert!(app.session_zoom > 1.0);

        let _ = app.update(Message::SessionAction(SessionAction::Fit));
        assert_eq!(app.session_zoom, 1.0);

        let shortcuts = app.capture_system_shortcuts;
        let _ = app.update(Message::SessionAction(SessionAction::SystemShortcut));
        assert_ne!(app.capture_system_shortcuts, shortcuts);
        let _ = app.update(Message::SessionAction(SessionAction::SystemShortcut));
        assert_eq!(app.capture_system_shortcuts, shortcuts);

        let _ = app.update(Message::ToggleSessionToolbarPin);
        assert!(app.session_toolbar_pinned);
    }

    #[test]
    #[ignore = "writes visual QA snapshots to /tmp"]
    fn render_visual_qa_snapshots() -> Result<(), iced_test::Error> {
        let settings = iced::Settings {
            fonts: vec![
                include_bytes!("../assets/Inter-Variable.ttf")
                    .as_slice()
                    .into(),
            ],
            default_font: iced::Font::new("Inter"),
            ..iced::Settings::default()
        };
        for (mode, preference) in [
            ("light", ThemePreference::Light),
            ("dark", ThemePreference::Dark),
        ] {
            let (mut app, _task) = ArdViewer::new();
            app.theme_preference = preference;
            app.settings_section = SettingsSection::General;
            theme::set_dark(app.effective_dark());

            for (name, size, view) in [
                (
                    "connection",
                    WindowKind::Connection.size(),
                    views::connection(&app, window::Id::unique()),
                ),
                (
                    "settings",
                    WindowKind::Settings.size(),
                    views::settings(&app, window::Id::unique()),
                ),
                (
                    "session",
                    WindowKind::Session.size(),
                    views::session(&app, window::Id::unique()),
                ),
            ] {
                let mut ui = iced_test::Simulator::with_size(settings.clone(), size, view);
                let snapshot = ui.snapshot(&theme::app_theme())?;
                assert!(
                    snapshot
                        .matches_image(format!("/tmp/ard-viewer-capabilities-v2-{mode}-{name}"))?
                );
            }
        }
        Ok(())
    }
}
