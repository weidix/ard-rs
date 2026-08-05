use iced::widget::{button, column, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Background, Border, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::theme::{
    self, BODY_SIZE, CAPTION_SIZE, CONTROL_RADIUS, MICRO_SIZE, REMOTE_CANVAS, SUCCESS, SURFACE,
    TEXT, TEXT_MUTED, WINDOW_RADIUS,
};
use crate::widgets::{icon_button, icon_toggle_button, window_titlebar};
use crate::{ArdViewer, Message, SessionAction};

pub fn session(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    column![
        window_titlebar(window_id, "Studio Mac", "安全连接 · 18 ms", None, 48),
        remote_canvas(app),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn remote_canvas(app: &ArdViewer) -> Element<'static, Message> {
    let desktop = remote_desktop();
    let base = container(column![
        space().height(62),
        row![space().width(85), desktop, space().width(85)].height(650),
        space().height(Fill),
    ])
    .width(Fill)
    .height(Fill)
    .style(theme::shaped_panel(
        REMOTE_CANVAS,
        iced::border::bottom(WINDOW_RADIUS),
    ));

    let controls: Element<'static, Message> = if app.session_toolbar_visible {
        container(
            mouse_area(control_bar(app.session_toolbar_pinned))
                .on_enter(Message::SessionToolbarInteraction)
                .on_move(|_| Message::SessionToolbarInteraction),
        )
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Start)
        .into()
    } else {
        container(
            button(
                container(icon(Icon::ChevronDown, 14.0, TEXT))
                    .width(Fill)
                    .height(Fill)
                    .center_x(Fill)
                    .center_y(Fill),
            )
            .width(46)
            .height(22)
            .padding(0)
            .style(theme::toolbar_marker)
            .on_press(Message::ShowSessionToolbar),
        )
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Start)
        .into()
    };
    let status = container(
        container(
            row![
                dot(SUCCESS, 6.0),
                text("RGBA · 自适应质量 · 60 fps")
                    .size(MICRO_SIZE)
                    .color(TEXT_MUTED)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(8)
        .style(theme::bordered_panel(SURFACE, CONTROL_RADIUS)),
    )
    .width(Fill)
    .height(Fill)
    .padding([23, 18])
    .align_x(Alignment::Start)
    .align_y(Alignment::End);

    stack![base, controls, status]
        .width(Fill)
        .height(Fill)
        .clip(true)
        .into()
}

fn control_bar(pinned: bool) -> Element<'static, Message> {
    let action_button = |kind, action| icon_button(kind, Message::SessionAction(action));
    let collapse = button(
        container(icon(Icon::ChevronUp, 16.0, TEXT))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill),
    )
    .width(theme::CONTROL_HEIGHT)
    .height(theme::CONTROL_HEIGHT)
    .padding(0)
    .style(theme::toolbar_embedded_button)
    .on_press(Message::HideSessionToolbar);
    container(
        row![
            row![
                action_button(Icon::Scan, SessionAction::Fit),
                action_button(Icon::ZoomIn, SessionAction::Zoom),
                action_button(Icon::Pointer, SessionAction::Input),
                action_button(Icon::Keyboard, SessionAction::SystemShortcut),
                action_button(Icon::Clipboard, SessionAction::Clipboard),
                action_button(Icon::Undo, SessionAction::Undo),
                icon_toggle_button(Icon::Pin, pinned, Message::ToggleSessionToolbarPin),
                icon_button(Icon::Fullscreen, Message::ToggleFullscreen),
            ]
            .spacing(5),
            collapse,
        ]
        .spacing(0)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(theme::bordered_panel(
        iced::Color::from_rgb8(26, 26, 28),
        12.0,
    ))
    .into()
}

fn remote_desktop() -> Element<'static, Message> {
    let menu = container(
        row![
            dot(TEXT, 6.0),
            text("Finder").size(CAPTION_SIZE).color(TEXT),
            text("文件   编辑   显示   前往   窗口   帮助")
                .size(MICRO_SIZE)
                .color(TEXT_MUTED),
            space().width(Fill),
            text("Wi‑Fi   14:32").size(MICRO_SIZE).color(TEXT_MUTED),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .height(28)
    .padding([0, 12])
    .width(Fill)
    .style(theme::shaped_panel(
        iced::Color::from_rgb8(20, 20, 22),
        iced::border::top(6),
    ));

    let remote_app = remote_app();
    let contents = column![
        menu,
        space().height(90),
        row![space().width(Fill), remote_app, space().width(Fill)],
        space().height(Fill),
    ]
    .height(Fill)
    .width(Fill);
    let gradient = iced::gradient::Linear::new(iced::Degrees(150.0))
        .add_stop(0.14, iced::Color::from_rgb8(23, 23, 26))
        .add_stop(0.86, iced::Color::from_rgb8(77, 74, 69));
    container(contents)
        .height(650)
        .width(Fill)
        .style(move |_| iced::widget::container::Style {
            background: Some(Background::Gradient(gradient.into())),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..iced::widget::container::Style::default()
        })
        .into()
}

fn remote_app() -> Element<'static, Message> {
    let chrome = container(
        row![
            dot(iced::Color::from_rgb8(51, 51, 51), 8.0),
            dot(iced::Color::from_rgb8(51, 51, 51), 8.0),
            dot(iced::Color::from_rgb8(51, 51, 51), 8.0),
            text("Remote Project")
                .size(BODY_SIZE)
                .color(iced::Color::from_rgb8(51, 51, 51)),
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .height(42)
    .width(Fill)
    .padding([0, 12])
    .center_y(42)
    .style(theme::shaped_panel(
        iced::Color::from_rgb8(214, 214, 212),
        iced::border::top(10),
    ));
    let sidebar = container(
        column![
            text("Overview")
                .size(CAPTION_SIZE)
                .color(iced::Color::from_rgb8(64, 64, 64)),
            text("Sessions")
                .size(CAPTION_SIZE)
                .color(iced::Color::from_rgb8(64, 64, 64)),
            text("Devices")
                .size(CAPTION_SIZE)
                .color(iced::Color::from_rgb8(64, 64, 64)),
            text("Settings")
                .size(CAPTION_SIZE)
                .color(iced::Color::from_rgb8(64, 64, 64)),
        ]
        .spacing(10),
    )
    .width(150)
    .height(Fill)
    .padding(14)
    .style(theme::shaped_panel(
        iced::Color::from_rgb8(222, 222, 219),
        iced::border::bottom_left(10),
    ));
    let session_card = |label: &'static str, status: &'static str| {
        container(
            row![
                icon(Icon::Monitor, 12.0, iced::Color::from_rgb8(56, 56, 56)),
                text(label)
                    .size(BODY_SIZE)
                    .color(iced::Color::from_rgb8(56, 56, 56)),
                text(status)
                    .size(BODY_SIZE)
                    .color(iced::Color::from_rgb8(56, 56, 56)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .height(52)
        .width(Fill)
        .padding(12)
        .center_y(52)
        .style(theme::rounded_panel(
            iced::Color::from_rgb8(227, 227, 224),
            CONTROL_RADIUS,
        ))
    };
    let pane = container(
        column![
            text("Active Sessions")
                .size(19)
                .color(iced::Color::from_rgb8(33, 33, 33)),
            text("2 devices currently connected")
                .size(CAPTION_SIZE)
                .color(iced::Color::from_rgb8(107, 107, 107)),
            session_card("Studio Mac", "Connected"),
            session_card("Office Mini", "Idle"),
        ]
        .spacing(14),
    )
    .width(470)
    .height(Fill)
    .padding(22)
    .style(theme::shaped_panel(
        iced::Color::from_rgb8(242, 242, 240),
        iced::border::bottom_right(10),
    ));
    container(column![chrome, row![sidebar, pane].height(Fill)])
        .width(620)
        .height(390)
        .style(theme::rounded_panel(
            iced::Color::from_rgb8(237, 237, 235),
            10.0,
        ))
        .into()
}

fn dot(color: iced::Color, size: f32) -> Element<'static, Message> {
    container(space())
        .width(size)
        .height(size)
        .style(theme::rounded_panel(color, size / 2.0))
        .into()
}
