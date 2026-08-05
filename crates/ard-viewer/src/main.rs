#![forbid(unsafe_code)]

mod state;
mod theme;
mod views;
mod widgets;

use std::collections::BTreeMap;

use ard_rs::ArdVideoQuality;
use iced::{Element, Subscription, Task, Theme, window};

use state::{DeviceState, KeyMapping, SavedDevice, SettingsSection, WindowKind};

#[derive(Debug)]
struct ArdViewer {
    windows: BTreeMap<window::Id, WindowKind>,
    devices: Vec<SavedDevice>,
    selected_device: usize,
    search: String,
    address: String,
    password: String,
    remember_password: bool,
    remember_device: bool,
    quality: ArdVideoQuality,
    frame_interval_ms: String,
    settings_section: SettingsSection,
    key_profile: String,
    auto_adapt_keyboard: bool,
    capture_system_shortcuts: bool,
    show_performance_hud: bool,
    mappings: Vec<KeyMapping>,
    session_zoom: f32,
    session_fullscreen: bool,
    status: String,
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(WindowKind, window::Id),
    WindowClosed(window::Id),
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
    Cancel,
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
    CopyPreset,
    ResetMappings,
    AddMapping,
    EditMapping(usize),
    SessionAction(SessionAction),
    ToggleFullscreen,
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
            address: "mac-studio.local".into(),
            devices,
            selected_device: 0,
            search: String::new(),
            password: "ard-password".into(),
            remember_password: true,
            remember_device: true,
            quality: ArdVideoQuality::Adaptive,
            frame_interval_ms: "Server-driven".into(),
            settings_section: SettingsSection::KeyMapping,
            key_profile: "macOS 默认".into(),
            auto_adapt_keyboard: true,
            capture_system_shortcuts: false,
            show_performance_hud: true,
            mappings: default_mappings(),
            session_zoom: 1.0,
            session_fullscreen: false,
            status: String::new(),
        };
        (app, open_window(WindowKind::Connection))
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
            }
            Message::WindowClosed(id) => {
                let closed = self.windows.remove(&id);
                if closed == Some(WindowKind::Connection) || self.windows.is_empty() {
                    return iced::exit();
                }
            }
            #[cfg(not(target_os = "macos"))]
            Message::CloseWindow(id) => return window::close(id),
            Message::DragWindow(id) => return window::drag(id),
            #[cfg(not(target_os = "macos"))]
            Message::MinimizeWindow(id) => return window::minimize(id, true),
            Message::ToggleMaximizeWindow(id) => return window::toggle_maximize(id),
            Message::OpenSettings => {
                if let Some(id) = self.window_id(WindowKind::Settings) {
                    return window::gain_focus(id);
                }
                return open_window(WindowKind::Settings);
            }
            Message::SearchChanged(value) => self.search = value,
            Message::DeviceSelected(index) => {
                if let Some(device) = self.devices.get(index) {
                    self.selected_device = index;
                    self.address.clone_from(&device.address);
                }
            }
            Message::AddressChanged(value) => self.address = value,
            Message::PasswordChanged(value) => self.password = value,
            Message::RememberPasswordChanged(value) => self.remember_password = value,
            Message::RememberDeviceChanged(value) => self.remember_device = value,
            Message::Cancel => self.status.clear(),
            Message::SettingsSectionSelected(section) => self.settings_section = section,
            Message::QualityChanged(quality) => self.quality = quality,
            Message::FrameIntervalChanged(value) => self.frame_interval_ms = value,
            Message::KeyProfileChanged(value) => self.key_profile = value,
            Message::AutoAdaptChanged(value) => self.auto_adapt_keyboard = value,
            Message::CaptureShortcutsChanged(value) => self.capture_system_shortcuts = value,
            Message::PerformanceHudChanged(value) => self.show_performance_hud = value,
            Message::ResetMappings => self.mappings = default_mappings(),
            Message::ToggleFullscreen => {
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
            Message::SessionAction(SessionAction::Fit) => self.session_zoom = 1.0,
            Message::SessionAction(SessionAction::Zoom) => {
                self.session_zoom = if self.session_zoom >= 1.4 {
                    0.6
                } else {
                    self.session_zoom + 0.1
                }
            }
            Message::Connect => {
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
            Message::SessionAction(_) => self.status = "Session transport is not wired yet".into(),
        }
        Task::none()
    }

    fn view(&self, id: window::Id) -> Element<'_, Message> {
        match self.windows.get(&id) {
            Some(WindowKind::Connection) => views::connection(self, id),
            Some(WindowKind::Settings) => views::settings(self, id),
            Some(WindowKind::Session) => views::session(self, id),
            None => iced::widget::space().into(),
        }
    }

    fn theme(&self, _id: window::Id) -> Option<Theme> {
        Some(theme::app_theme())
    }
    fn subscription(&self) -> Subscription<Message> {
        window::close_events().map(Message::WindowClosed)
    }
    fn window_id(&self, kind: WindowKind) -> Option<window::Id> {
        self.windows
            .iter()
            .find_map(|(id, current)| (*current == kind).then_some(*id))
    }
}

fn open_window(kind: WindowKind) -> Task<Message> {
    let size = kind.size();
    let (_, task) = window::open(window::Settings {
        size,
        min_size: Some(size),
        position: window::Position::Centered,
        decorations: cfg!(target_os = "macos"),
        transparent: true,
        resizable: true,
        closeable: true,
        minimizable: true,
        platform_specific: platform_window_settings(),
        ..window::Settings::default()
    });
    task.map(move |id| Message::WindowOpened(kind, id))
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
    }

    #[test]
    #[ignore = "writes visual QA snapshots to /tmp"]
    fn render_visual_qa_snapshots() -> Result<(), iced_test::Error> {
        let (app, _task) = ArdViewer::new();
        let settings = iced::Settings {
            fonts: vec![
                include_bytes!("../assets/Inter-Variable.ttf")
                    .as_slice()
                    .into(),
            ],
            default_font: iced::Font::with_name("Inter"),
            ..iced::Settings::default()
        };
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
            assert!(snapshot.matches_image(format!("/tmp/ard-viewer-{name}"))?);
        }
        Ok(())
    }
}
