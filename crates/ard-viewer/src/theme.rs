use iced::widget::overlay::menu;
use iced::widget::{
    button, checkbox as iced_checkbox, container, pick_list as iced_pick_list, text_input,
};
use iced::{Background, Border, Color, Shadow, Theme, border};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub background: Color,
    pub remote_canvas: Color,
    pub surface: Color,
    pub surface_active: Color,
    pub border: Color,
    pub text: Color,
    pub primary: Color,
    pub text_muted: Color,
    pub text_dim: Color,
    pub text_warm: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub success: Color,
    pub warning: Color,
}

const DARK: Palette = Palette {
    background: Color::from_rgb8(5, 5, 5),
    remote_canvas: Color::from_rgb8(5, 5, 5),
    surface: Color::from_rgb8(16, 16, 16),
    surface_active: Color::from_rgb8(28, 28, 28),
    border: Color::from_rgb8(61, 61, 61),
    text: Color::from_rgb8(243, 221, 221),
    primary: Color::from_rgb8(224, 224, 224),
    text_muted: Color::from_rgb8(150, 150, 150),
    text_dim: Color::from_rgb8(91, 91, 91),
    text_warm: Color::from_rgb8(224, 224, 224),
    accent: Color::from_rgb8(113, 109, 227),
    accent_text: Color::from_rgb8(5, 5, 5),
    success: Color::from_rgb8(87, 158, 107),
    warning: Color::from_rgb8(209, 204, 194),
};

const LIGHT: Palette = Palette {
    background: Color::from_rgb8(249, 249, 249),
    remote_canvas: Color::from_rgb8(249, 249, 249),
    surface: Color::from_rgb8(242, 242, 242),
    surface_active: Color::from_rgb8(232, 232, 232),
    border: Color::from_rgb8(194, 194, 194),
    text: Color::from_rgb8(35, 12, 12),
    primary: Color::from_rgb8(30, 30, 30),
    text_muted: Color::from_rgb8(105, 105, 105),
    text_dim: Color::from_rgb8(154, 154, 154),
    text_warm: Color::from_rgb8(30, 30, 30),
    accent: Color::from_rgb8(32, 28, 146),
    accent_text: Color::from_rgb8(249, 249, 249),
    success: Color::from_rgb8(65, 126, 82),
    warning: Color::from_rgb8(112, 79, 58),
};

static DARK_ACTIVE: AtomicBool = AtomicBool::new(true);

pub fn set_dark(is_dark: bool) {
    DARK_ACTIVE.store(is_dark, Ordering::Relaxed);
}

pub fn palette() -> Palette {
    if DARK_ACTIVE.load(Ordering::Relaxed) {
        DARK
    } else {
        LIGHT
    }
}

pub fn mix(from: Color, to: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    Color {
        r: from.r + (to.r - from.r) * amount,
        g: from.g + (to.g - from.g) * amount,
        b: from.b + (to.b - from.b) * amount,
        a: from.a + (to.a - from.a) * amount,
    }
}

// Layout tokens. Keep geometry here so every window uses the same, predictable
// control metrics instead of relying on each widget's intrinsic size.
pub const WINDOW_RADIUS: f32 = 12.0;
pub const CARD_RADIUS: f32 = 9.0;
pub const CONTROL_RADIUS: f32 = 8.0;
pub const CHECKBOX_RADIUS: f32 = 4.0;

pub const TITLE_SIZE: f32 = 15.0;
pub const ICON_SIZE: f32 = 16.0;
pub const BODY_SIZE: f32 = 11.0;
pub const CAPTION_SIZE: f32 = 10.0;
pub const MICRO_SIZE: f32 = 9.0;

pub const CONTROL_HEIGHT: f32 = 34.0;
pub const CONTROL_PADDING_X: f32 = 12.0;
pub const CONTENT_PADDING_X: f32 = 28.0;
pub const CONTENT_PADDING_BOTTOM: f32 = 16.0;

pub fn app_theme() -> Theme {
    let palette = palette();
    let name = if DARK_ACTIVE.load(Ordering::Relaxed) {
        "ARD Viewer Dark"
    } else {
        "ARD Viewer Light"
    };
    Theme::custom(
        name,
        iced::theme::palette::Seed {
            background: palette.background,
            text: palette.text,
            primary: palette.accent,
            success: palette.success,
            danger: palette.warning,
            warning: palette.warning,
        },
    )
}

pub fn panel(color: Color) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| {
        container::Style::default()
            .background(color)
            .color(palette().text)
    }
}

pub fn shaped_panel(
    color: Color,
    radius: border::Radius,
) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(palette().text),
        border: Border {
            radius,
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn bordered_panel(color: Color, radius: f32) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(palette().text),
        border: Border {
            color: palette().border,
            width: 1.0,
            radius: radius.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn modal_backdrop(opacity: f32) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style::default().background(Color::from_rgba(0.0, 0.0, 0.0, 0.68 * opacity))
}

pub fn modal_panel(progress: f32) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| {
        let surface = palette().surface;
        container::Style {
            background: Some(Background::Color(mix(
                Color { a: 0.0, ..surface },
                surface,
                progress,
            ))),
            text_color: Some(palette().text),
            border: Border {
                color: mix(Color::TRANSPARENT, palette().border, progress),
                width: 1.0,
                radius: 12.0.into(),
            },
            shadow: Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.35 * progress),
                offset: iced::Vector::new(0.0, 8.0 * progress),
                blur_radius: 28.0 * progress,
            },
            snap: true,
        }
    }
}

pub fn toolbar_foreground(is_dark: bool) -> Color {
    let _ = is_dark;
    palette().primary
}

fn toolbar_glass_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_rgba8(24, 24, 23, 0.76)
    } else {
        Color::from_rgba8(238, 238, 235, 0.82)
    }
}

pub fn toolbar_glass(
    is_dark: bool,
    radius: border::Radius,
) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(toolbar_glass_color(is_dark))),
        text_color: Some(toolbar_foreground(is_dark)),
        border: Border {
            radius,
            ..Border::default()
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, if is_dark { 0.26 } else { 0.12 }),
            offset: iced::Vector::new(0.0, 3.0),
            blur_radius: 12.0,
        },
        snap: true,
    }
}

pub fn toolbar_glass_button(
    is_dark: bool,
    selected: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let alpha = match (selected, hovered, is_dark) {
            (true, true, true) => 0.22,
            (true, _, true) => 0.16,
            (false, true, true) => 0.11,
            (true, true, false) => 0.17,
            (true, _, false) => 0.12,
            (false, true, false) => 0.08,
            _ => 0.0,
        };
        button::Style {
            background: (alpha > 0.0).then_some(Background::Color(if is_dark {
                Color::from_rgba(1.0, 1.0, 1.0, alpha)
            } else {
                Color::from_rgba(0.0, 0.0, 0.0, alpha)
            })),
            text_color: toolbar_foreground(is_dark),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn toolbar_handle(is_dark: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            if is_dark {
                Color::from_rgba8(38, 38, 37, 0.84)
            } else {
                Color::from_rgba8(222, 222, 219, 0.88)
            }
        } else {
            toolbar_glass_color(is_dark)
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: toolbar_foreground(is_dark),
            border: Border {
                radius: border::bottom(7),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn secondary_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        palette().surface_active
    } else {
        palette().surface
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette().text,
        border: Border {
            color: palette().border,
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
        ..button::Style::default()
    }
}

pub fn primary_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Pressed) {
        mix(palette().accent, Color::BLACK, 0.16)
    } else {
        palette().accent
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette().accent_text,
        border: Border {
            radius: CONTROL_RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub fn close_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        Color::from_rgb8(196, 55, 55)
    } else {
        palette().surface
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette().text,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

#[cfg(target_os = "windows")]
pub fn windows_caption_button(
    close: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background = match (close, status) {
            (true, button::Status::Hovered) => Some(Color::from_rgb8(196, 43, 28)),
            (true, button::Status::Pressed) => Some(Color::from_rgb8(153, 32, 21)),
            (false, button::Status::Hovered) => Some(Color::from_rgb8(48, 48, 51)),
            (false, button::Status::Pressed) => Some(Color::from_rgb8(61, 61, 65)),
            _ => None,
        };
        button::Style {
            background: background.map(Background::Color),
            text_color: palette().text,
            border: Border::default(),
            ..button::Style::default()
        }
    }
}

pub fn nav_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background =
            if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                Some(Background::Color(palette().surface_active))
            } else {
                None
            };
        button::Style {
            background,
            text_color: if selected {
                palette().text
            } else {
                palette().text_muted
            },
            border: Border {
                radius: CONTROL_RADIUS.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn device_button(selection: f32) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let amount = if hover { selection.max(0.5) } else { selection };
        button::Style {
            background: (amount > 0.0).then_some(Background::Color(mix(
                Color::TRANSPARENT,
                palette().surface_active,
                amount,
            ))),
            text_color: mix(palette().text_muted, palette().text, amount),
            border: Border {
                radius: 5.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(
            if matches!(status, text_input::Status::Focused { is_hovered: _ }) {
                palette().surface_active
            } else {
                palette().surface
            },
        ),
        border: Border {
            color: if matches!(status, text_input::Status::Focused { is_hovered: _ }) {
                palette().accent
            } else {
                palette().border
            },
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
        placeholder: palette().text_muted,
        value: palette().text,
        selection: Color::from_rgba8(235, 237, 242, 0.28),
    }
}

pub fn inline_input(_: &Theme, _: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        placeholder: palette().text_muted,
        value: palette().text,
        selection: Color::from_rgba8(235, 237, 242, 0.28),
    }
}

pub fn pick_list(_: &Theme, status: iced_pick_list::Status) -> iced_pick_list::Style {
    iced_pick_list::Style {
        text_color: palette().text,
        placeholder_color: palette().text_muted,
        handle_color: palette().text_muted,
        background: Background::Color(palette().surface_active),
        border: Border {
            color: if matches!(status, iced_pick_list::Status::Active) {
                palette().border
            } else {
                palette().text_muted
            },
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
    }
}

pub fn pick_list_menu(_: &Theme) -> menu::Style {
    menu::Style {
        background: Background::Color(palette().surface_active),
        border: Border {
            color: palette().border,
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
        text_color: palette().text,
        selected_text_color: palette().text,
        selected_background: Background::Color(mix(
            palette().surface_active,
            palette().accent,
            0.18,
        )),
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, 0.45),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
    }
}

pub fn checkbox(_: &Theme, status: iced_checkbox::Status) -> iced_checkbox::Style {
    let checked = matches!(
        status,
        iced_checkbox::Status::Active { is_checked: true }
            | iced_checkbox::Status::Hovered { is_checked: true }
    );
    iced_checkbox::Style {
        background: Background::Color(if checked {
            palette().accent
        } else {
            palette().surface
        }),
        icon_color: palette().accent_text,
        border: Border {
            color: if checked {
                palette().accent
            } else {
                palette().border
            },
            width: 1.0,
            radius: CHECKBOX_RADIUS.into(),
        },
        text_color: Some(palette().text),
    }
}
