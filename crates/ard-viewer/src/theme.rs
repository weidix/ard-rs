use iced::widget::{button, checkbox as iced_checkbox, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme, border};

pub const BACKGROUND: Color = Color::from_rgb8(13, 13, 14);
pub const REMOTE_CANVAS: Color = Color::from_rgb8(5, 5, 6);
pub const SURFACE: Color = Color::from_rgb8(22, 22, 24);
pub const SURFACE_ACTIVE: Color = Color::from_rgb8(41, 41, 43);
pub const BORDER: Color = Color::from_rgb8(59, 59, 64);
pub const TEXT: Color = Color::from_rgb8(240, 240, 242);
pub const TEXT_MUTED: Color = Color::from_rgb8(158, 158, 168);
pub const TEXT_DIM: Color = Color::from_rgb8(107, 107, 117);
pub const TEXT_WARM: Color = Color::from_rgb8(209, 204, 194);
pub const ACCENT: Color = TEXT;
pub const ACCENT_TEXT: Color = Color::from_rgb8(9, 9, 10);
pub const SUCCESS: Color = Color::from_rgb8(87, 158, 107);
pub const WARNING: Color = TEXT_WARM;

pub fn app_theme() -> Theme {
    Theme::custom(
        "ARD Viewer".to_owned(),
        iced::theme::Palette {
            background: Color::TRANSPARENT,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            danger: WARNING,
            warning: WARNING,
        },
    )
}

pub fn panel(color: Color) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style::default().background(color).color(TEXT)
}

pub fn shaped_panel(
    color: Color,
    radius: border::Radius,
) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(TEXT),
        border: Border {
            radius,
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn rounded_panel(color: Color, radius: f32) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(TEXT),
        border: Border {
            radius: radius.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn bordered_panel(color: Color, radius: f32) -> impl Fn(&Theme) -> container::Style + Copy {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(TEXT),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: radius.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn secondary_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        SURFACE_ACTIVE
    } else {
        SURFACE
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: TEXT,
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..button::Style::default()
    }
}

pub fn primary_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Pressed) {
        Color::from_rgb8(210, 212, 218)
    } else {
        ACCENT
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: ACCENT_TEXT,
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

#[cfg(not(target_os = "macos"))]
pub fn close_button(_: &Theme, status: button::Status) -> button::Style {
    let background = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        Color::from_rgb8(196, 55, 55)
    } else {
        SURFACE
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: TEXT,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

pub fn nav_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background =
            if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                Some(Background::Color(SURFACE_ACTIVE))
            } else {
                None
            };
        button::Style {
            background,
            text_color: if selected { TEXT } else { TEXT_MUTED },
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

pub fn device_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background =
            if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                SURFACE_ACTIVE
            } else {
                SURFACE
            };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: if selected { TEXT } else { TEXT_MUTED },
            border: Border {
                radius: 8.0.into(),
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
                SURFACE_ACTIVE
            } else {
                SURFACE
            },
        ),
        border: Border {
            color: if matches!(status, text_input::Status::Focused { is_hovered: _ }) {
                ACCENT
            } else {
                BORDER
            },
            width: 1.0,
            radius: 8.0.into(),
        },
        icon: TEXT_MUTED,
        placeholder: TEXT_MUTED,
        value: TEXT,
        selection: Color::from_rgba8(235, 237, 242, 0.28),
    }
}

pub fn checkbox(_: &Theme, status: iced_checkbox::Status) -> iced_checkbox::Style {
    let checked = matches!(
        status,
        iced_checkbox::Status::Active { is_checked: true }
            | iced_checkbox::Status::Hovered { is_checked: true }
    );
    iced_checkbox::Style {
        background: Background::Color(if checked { ACCENT } else { SURFACE }),
        icon_color: ACCENT_TEXT,
        border: Border {
            color: if checked { ACCENT } else { BORDER },
            width: 1.0,
            radius: 4.0.into(),
        },
        text_color: Some(TEXT),
    }
}
