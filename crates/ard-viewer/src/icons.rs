use iced::Color;
use iced::widget::{Svg, svg};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Settings,
    #[cfg(not(target_os = "macos"))]
    Minimize,
    #[cfg(not(target_os = "macos"))]
    Maximize,
    #[cfg(not(target_os = "macos"))]
    Close,
    Monitor,
    ChevronRight,
    ChevronDown,
    ChevronUp,
    Search,
    Eye,
    EyeOff,
    Shield,
    MoreHorizontal,
    Minus,
    Plus,
    Sliders,
    Keyboard,
    Info,
    Scan,
    ZoomIn,
    Pointer,
    Clipboard,
    User,
    Trash,
    Undo,
    Fullscreen,
    Pin,
}

impl Icon {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Settings => include_bytes!("../assets/icons/settings.svg"),
            #[cfg(not(target_os = "macos"))]
            Self::Minimize => include_bytes!("../assets/icons/minus.svg"),
            #[cfg(not(target_os = "macos"))]
            Self::Maximize => include_bytes!("../assets/icons/maximize.svg"),
            #[cfg(not(target_os = "macos"))]
            Self::Close => include_bytes!("../assets/icons/close.svg"),
            Self::Monitor => include_bytes!("../assets/icons/monitor.svg"),
            Self::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Self::ChevronDown => include_bytes!("../assets/icons/chevron-down.svg"),
            Self::ChevronUp => include_bytes!("../assets/icons/chevron-up.svg"),
            Self::Search => include_bytes!("../assets/icons/search.svg"),
            Self::Eye => include_bytes!("../assets/icons/eye.svg"),
            Self::EyeOff => include_bytes!("../assets/icons/eye-off.svg"),
            Self::Shield => include_bytes!("../assets/icons/shield.svg"),
            Self::MoreHorizontal => include_bytes!("../assets/icons/more-horizontal.svg"),
            Self::Minus => include_bytes!("../assets/icons/minus.svg"),
            Self::Plus => include_bytes!("../assets/icons/plus.svg"),
            Self::Sliders => include_bytes!("../assets/icons/sliders.svg"),
            Self::Keyboard => include_bytes!("../assets/icons/keyboard.svg"),
            Self::Info => include_bytes!("../assets/icons/info.svg"),
            Self::Scan => include_bytes!("../assets/icons/scan.svg"),
            Self::ZoomIn => include_bytes!("../assets/icons/zoom-in.svg"),
            Self::Pointer => include_bytes!("../assets/icons/pointer.svg"),
            Self::Clipboard => include_bytes!("../assets/icons/clipboard.svg"),
            Self::User => include_bytes!("../assets/icons/user.svg"),
            Self::Trash => include_bytes!("../assets/icons/trash.svg"),
            Self::Undo => include_bytes!("../assets/icons/undo.svg"),
            Self::Fullscreen => include_bytes!("../assets/icons/fullscreen.svg"),
            Self::Pin => include_bytes!("../assets/icons/pin.svg"),
        }
    }
}

pub fn icon(kind: Icon, size: f32, color: Color) -> Svg<'static> {
    svg(svg::Handle::from_memory(kind.bytes()))
        .width(size)
        .height(size)
        .style(move |_, _| svg::Style { color: Some(color) })
}
