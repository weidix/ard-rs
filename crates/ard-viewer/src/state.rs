use crate::i18n::Language;
use crate::icons::Icon;
use iced::Size;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Connection,
    Settings,
    Session,
}

impl WindowKind {
    pub fn size(self) -> Size {
        match self {
            Self::Connection => Size::new(900.0, 680.0),
            Self::Settings => Size::new(900.0, 680.0),
            Self::Session => Size::new(1280.0, 800.0),
        }
    }

    pub fn min_size(self) -> Size {
        match self {
            Self::Connection | Self::Settings => Size::new(760.0, 560.0),
            Self::Session => Size::new(640.0, 480.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DeviceState {
    Online,
    Saved,
    RecentlyUsed,
}

#[derive(Debug, Clone)]
pub struct SavedDevice {
    pub name: String,
    pub address: String,
    pub username: String,
    pub state: DeviceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Display,
    KeyMapping,
    Security,
    About,
}

impl SettingsSection {
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::Display,
        Self::KeyMapping,
        Self::Security,
        Self::About,
    ];
    pub fn label(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::General => "常规",
            Self::Display => "显示与性能",
            Self::KeyMapping => "按键映射",
            Self::Security => "安全",
            Self::About => "关于",
        })
    }
    pub fn icon(self) -> Icon {
        match self {
            Self::General => Icon::Sliders,
            Self::Display => Icon::Monitor,
            Self::KeyMapping => Icon::Keyboard,
            Self::Security => Icon::Shield,
            Self::About => Icon::Info,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyMapping {
    pub local: String,
    pub remote: String,
    pub scope: String,
}

/// A quick action that can appear in the session toolbar. The visible set is
/// user-configurable from the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolbarButton {
    Screenshot,
    AppSwitcher,
    MissionControl,
    Desktop,
    ZoomOut,
    ZoomIn,
    ActualSize,
    FitToWindow,
    RemoteKeyboard,
    Pointer,
    Clipboard,
    SystemShortcut,
    Undo,
}

impl ToolbarButton {
    pub const ALL: [Self; 13] = [
        Self::Screenshot,
        Self::AppSwitcher,
        Self::MissionControl,
        Self::Desktop,
        Self::ZoomOut,
        Self::ZoomIn,
        Self::ActualSize,
        Self::FitToWindow,
        Self::RemoteKeyboard,
        Self::Pointer,
        Self::Clipboard,
        Self::SystemShortcut,
        Self::Undo,
    ];

    pub fn icon(self) -> Icon {
        match self {
            Self::Screenshot => Icon::Camera,
            Self::AppSwitcher => Icon::AppSwitcher,
            Self::MissionControl => Icon::Layers,
            Self::Desktop => Icon::Desktop,
            Self::ZoomOut => Icon::ZoomOut,
            Self::ZoomIn => Icon::ZoomIn,
            Self::ActualSize => Icon::ActualSize,
            Self::FitToWindow => Icon::Scan,
            Self::RemoteKeyboard => Icon::Keyboard,
            Self::Pointer => Icon::Pointer,
            Self::Clipboard => Icon::Clipboard,
            Self::SystemShortcut => Icon::Sliders,
            Self::Undo => Icon::Undo,
        }
    }

    pub fn label(self, language: Language) -> &'static str {
        language.tr(match self {
            Self::Screenshot => "截屏",
            Self::AppSwitcher => "App 切换",
            Self::MissionControl => "调度中心",
            Self::Desktop => "桌面",
            Self::ZoomOut => "缩小",
            Self::ZoomIn => "放大",
            Self::ActualSize => "实际画面",
            Self::FitToWindow => "缩放至窗口大小",
            Self::RemoteKeyboard => "远程键盘",
            Self::Pointer => "键鼠输入",
            Self::Clipboard => "剪贴板",
            Self::SystemShortcut => "系统快捷键",
            Self::Undo => "重置缩放",
        })
    }
}
