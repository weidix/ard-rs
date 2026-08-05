use iced::widget::overlay::menu;
use iced::widget::{
    button, checkbox as iced_checkbox, container, pick_list as iced_pick_list, text_input,
};
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

// Layout tokens. Keep geometry here so every window uses the same, predictable
// control metrics instead of relying on each widget's intrinsic size.
pub const WINDOW_RADIUS: f32 = 12.0;
pub const CARD_RADIUS: f32 = 9.0;
pub const CONTROL_RADIUS: f32 = 8.0;
pub const CHECKBOX_RADIUS: f32 = 4.0;

pub const TITLE_SIZE: f32 = 15.0;
pub const WINDOW_TITLE_SIZE: f32 = 13.0;
pub const ICON_SIZE: f32 = 16.0;
pub const BODY_SIZE: f32 = 11.0;
pub const CAPTION_SIZE: f32 = 10.0;
pub const MICRO_SIZE: f32 = 9.0;

pub const CONTROL_HEIGHT: f32 = 34.0;
pub const CONTROL_PADDING_X: f32 = 12.0;
pub const CONTENT_PADDING_X: f32 = 28.0;
pub const CONTENT_PADDING_Y: f32 = 24.0;
pub const CONTENT_PADDING_BOTTOM: f32 = 16.0;

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

pub fn modal_backdrop(_: &Theme) -> container::Style {
    container::Style::default().background(Color::from_rgba8(0, 0, 0, 0.68))
}

pub fn toolbar_marker(_: &Theme, status: button::Status) -> button::Style {
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
            radius: CONTROL_RADIUS.into(),
        },
        ..button::Style::default()
    }
}

pub fn toolbar_embedded_button(_: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then_some(Background::Color(SURFACE_ACTIVE)),
        text_color: TEXT,
        border: Border {
            radius: CONTROL_RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

pub fn toggle_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style + Copy {
    move |_, status| {
        let background =
            if selected || matches!(status, button::Status::Hovered | button::Status::Pressed) {
                SURFACE_ACTIVE
            } else {
                SURFACE
            };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: TEXT,
            border: Border {
                color: if selected { TEXT_MUTED } else { BORDER },
                width: 1.0,
                radius: CONTROL_RADIUS.into(),
            },
            ..button::Style::default()
        }
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
            radius: CONTROL_RADIUS.into(),
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
            text_color: TEXT,
            border: Border::default(),
            ..button::Style::default()
        }
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
                radius: CONTROL_RADIUS.into(),
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
                radius: CONTROL_RADIUS.into(),
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
            radius: CONTROL_RADIUS.into(),
        },
        icon: TEXT_MUTED,
        placeholder: TEXT_MUTED,
        value: TEXT,
        selection: Color::from_rgba8(235, 237, 242, 0.28),
    }
}

pub fn inline_input(_: &Theme, _: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: TEXT_MUTED,
        placeholder: TEXT_MUTED,
        value: TEXT,
        selection: Color::from_rgba8(235, 237, 242, 0.28),
    }
}

pub fn pick_list(_: &Theme, status: iced_pick_list::Status) -> iced_pick_list::Style {
    iced_pick_list::Style {
        text_color: TEXT,
        placeholder_color: TEXT_MUTED,
        handle_color: TEXT_MUTED,
        background: Background::Color(SURFACE_ACTIVE),
        border: Border {
            color: if matches!(status, iced_pick_list::Status::Active) {
                BORDER
            } else {
                TEXT_MUTED
            },
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
    }
}

pub fn pick_list_menu(_: &Theme) -> menu::Style {
    menu::Style {
        background: Background::Color(SURFACE_ACTIVE),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: CONTROL_RADIUS.into(),
        },
        text_color: TEXT,
        selected_text_color: TEXT,
        selected_background: Background::Color(Color::from_rgb8(58, 58, 62)),
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
        background: Background::Color(if checked { ACCENT } else { SURFACE }),
        icon_color: ACCENT_TEXT,
        border: Border {
            color: if checked { ACCENT } else { BORDER },
            width: 1.0,
            radius: CHECKBOX_RADIUS.into(),
        },
        text_color: Some(TEXT),
    }
}
