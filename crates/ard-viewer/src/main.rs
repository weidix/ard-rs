#![deny(unsafe_code)]

mod icons;
mod state;
mod theme;
mod views;
mod widgets;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use ard_rs::ArdVideoQuality;
use iced::widget::{column, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Element, Fill, Subscription, Task, Theme, window};

use state::{DeviceState, KeyMapping, SavedDevice, SettingsSection, ThemePreference, WindowKind};

const SESSION_TOOLBAR_WIDTH: f32 = 286.0;
const SESSION_TOOLBAR_COLLAPSED_WIDTH: f32 = 44.0;
const SETTINGS_WINDOW_OFFSET: f32 = 32.0;

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
    password: String,
    remember_password: bool,
    remember_device: bool,
    quality: ArdVideoQuality,
    frame_interval_ms: String,
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
    PasswordChanged(String),
    RememberPasswordChanged(bool),
    RememberDeviceChanged(bool),
    SaveDevice,
    Connect,
    ManageDevices,
    ExportShortcuts,
    SettingsSectionSelected(SettingsSection),
    QualityChanged(ArdVideoQuality),
    FrameIntervalChanged(String),
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
        let devices = vec![
            SavedDevice {
                name: "Studio Mac".into(),
                address: "10.0.0.42:5900".into(),
                state: DeviceState::Online,
            },
            SavedDevice {
                name: "Office Mini".into(),
                address: "office.example.com:5900".into(),
                state: DeviceState::Saved,
            },
            SavedDevice {
                name: "Home Server".into(),
                address: "192.168.1.18:5900".into(),
                state: DeviceState::RecentlyUsed,
            },
        ];
        let app = Self {
            windows: BTreeMap::new(),
            maximized_windows: BTreeSet::new(),
            address: "mac-studio.local".into(),
            devices,
            selected_device: 0,
            previous_selected_device: 0,
            device_transition: 1.0,
            search: String::new(),
            password: "ard-password".into(),
            remember_password: true,
            remember_device: true,
            quality: ArdVideoQuality::Adaptive,
            frame_interval_ms: "Server-driven".into(),
            settings_section: SettingsSection::KeyMapping,
            settings_transition: 1.0,
            key_profile: "macOS 默认".into(),
            auto_adapt_keyboard: true,
            capture_system_shortcuts: false,
            show_performance_hud: true,
            theme_preference: ThemePreference::System,
            system_dark: false,
            mappings: default_mappings(),
            session_zoom: 1.0,
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
            Some(WindowKind::Session) => "Studio Mac — ARD Viewer",
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
                if closed == Some(WindowKind::Connection) || self.windows.is_empty() {
                    return iced::exit();
                }
            }
            Message::WindowEvent(id, window::Event::Resized(size)) => {
                if self.windows.get(&id) == Some(&WindowKind::Session) {
                    self.session_toolbar_window_width = size.width;
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
                    self.address.clone_from(&device.address);
                }
            }
            Message::AddressChanged(value) => self.address = value,
            Message::PasswordChanged(value) => self.password = value,
            Message::RememberPasswordChanged(value) => self.remember_password = value,
            Message::RememberDeviceChanged(value) => self.remember_device = value,
            Message::SaveDevice => {
                let address = self.address.trim();
                if address.is_empty() {
                    self.status = "请输入设备地址".into();
                } else if let Some(index) = self
                    .devices
                    .iter()
                    .position(|device| device.address.eq_ignore_ascii_case(address))
                {
                    self.selected_device = index;
                    self.status = "设备已保存".into();
                } else {
                    let name = address.trim_end_matches(":5900").to_owned();
                    self.devices.push(SavedDevice {
                        name,
                        address: address.to_owned(),
                        state: DeviceState::Saved,
                    });
                    self.previous_selected_device = self.selected_device;
                    self.selected_device = self.devices.len() - 1;
                    self.device_transition = 0.0;
                    self.status = "设备已保存".into();
                }
            }
            Message::SettingsSectionSelected(section) => {
                if section != self.settings_section {
                    self.settings_section = section;
                    self.settings_transition = 0.0;
                }
            }
            Message::QualityChanged(quality) => self.quality = quality,
            Message::FrameIntervalChanged(value) => self.frame_interval_ms = value,
            Message::KeyProfileChanged(value) => self.key_profile = value,
            Message::AutoAdaptChanged(value) => self.auto_adapt_keyboard = value,
            Message::CaptureShortcutsChanged(value) => self.capture_system_shortcuts = value,
            Message::PerformanceHudChanged(value) => self.show_performance_hud = value,
            Message::ThemePreferenceChanged(preference) => {
                self.theme_preference = preference;
                theme::set_dark(self.effective_dark());
            }
            Message::SystemThemeChanged(mode) => {
                self.system_dark = mode == iced::theme::Mode::Dark;
                if self.theme_preference == ThemePreference::System {
                    theme::set_dark(self.system_dark);
                }
            }
            Message::ResetMappings => self.mappings = default_mappings(),
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
                self.session_toolbar_visible = true;
                self.session_toolbar_last_interaction = Instant::now();
                if let Some(id) = self.window_id(WindowKind::Session) {
                    return window::gain_focus(id);
                }
                return open_window(WindowKind::Session);
            }
            Message::ManageDevices => self.status = "Device management is not wired yet".into(),
            Message::ExportShortcuts => self.status = "Shortcut export is not wired yet".into(),
            Message::CopyPreset => self.status = "Preset copied".into(),
            Message::AddMapping => self.status = "Mapping editor is not wired yet".into(),
            Message::EditMapping(index) => self.status = format!("Mapping {} selected", index + 1),
            Message::SessionAction(_) => {
                self.touch_session_toolbar();
                self.status = "Session transport is not wired yet".into();
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
        ];
        let session_open = self.window_id(WindowKind::Session).is_some();
        if session_open {
            subscriptions.push(
                iced::time::every(Duration::from_millis(250)).map(Message::SessionToolbarTick),
            );
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
            || (self.close_modal_progress - modal_target).abs() > 0.001;
        if session_open || transition_active {
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

    fn clamp_session_toolbar_position(&mut self) {
        const TOOLBAR_HALF_WIDTH: f32 = SESSION_TOOLBAR_WIDTH / 2.0;
        if let Some(x) = self.session_toolbar_x.as_mut() {
            *x = x.clamp(
                TOOLBAR_HALF_WIDTH,
                (self.session_toolbar_window_width - TOOLBAR_HALF_WIDTH).max(TOOLBAR_HALF_WIDTH),
            );
        }
    }
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
    vec![
        KeyMapping {
            local: "⌘ C",
            remote: "复制",
            scope: "全局",
        },
        KeyMapping {
            local: "⌘ V",
            remote: "粘贴",
            scope: "全局",
        },
        KeyMapping {
            local: "⌘ ⌥ Esc",
            remote: "强制退出",
            scope: "macOS",
        },
        KeyMapping {
            local: "Ctrl Alt Del",
            remote: "安全选项",
            scope: "Windows",
        },
        KeyMapping {
            local: "⌘ `",
            remote: "切换远程窗口",
            scope: "会话",
        },
    ]
}

fn main() -> iced::Result {
    iced::daemon(ArdViewer::new, ArdViewer::update, ArdViewer::view)
        .title(ArdViewer::title)
        .theme(ArdViewer::theme)
        .subscription(ArdViewer::subscription)
        .font(include_bytes!("../assets/Inter-Variable.ttf").as_slice())
        .default_font(iced::Font::with_name("Inter"))
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
    #[ignore = "writes visual QA snapshots to /tmp"]
    fn render_visual_qa_snapshots() -> Result<(), iced_test::Error> {
        let settings = iced::Settings {
            fonts: vec![
                include_bytes!("../assets/Inter-Variable.ttf")
                    .as_slice()
                    .into(),
            ],
            default_font: iced::Font::with_name("Inter"),
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
                assert!(snapshot.matches_image(format!("/tmp/ard-viewer-{mode}-{name}"))?);
            }
        }
        Ok(())
    }
}
