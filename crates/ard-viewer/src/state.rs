use crate::i18n::Language;
use crate::icons::Icon;
use iced::Size;

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
            Self::Connection => Size::new(900.0, 640.0),
            Self::Settings => Size::new(900.0, 680.0),
            Self::Session => Size::new(1280.0, 800.0),
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
