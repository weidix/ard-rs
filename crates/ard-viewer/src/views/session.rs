use std::f32::consts::TAU;

use iced::widget::{button, column, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Background, Border, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::theme::{self, BODY_SIZE, CAPTION_SIZE, CONTROL_RADIUS, MICRO_SIZE, WINDOW_RADIUS};
use crate::widgets::{window_drag_region, window_platform_controls};
use crate::{
    ArdViewer, Message, SESSION_TOOLBAR_COLLAPSED_WIDTH, SESSION_TOOLBAR_WIDTH, SessionAction,
};

pub fn session(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.session_fullscreen || app.is_window_maximized(window_id);
    stack![
        window_drag_region(window_id),
        remote_canvas(
            app,
            maximized,
            app.effective_dark(),
            app.session_toolbar_x,
            app.session_toolbar_window_width,
        ),
        window_platform_controls(window_id),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn remote_canvas(
    app: &ArdViewer,
    maximized: bool,
    is_dark: bool,
    toolbar_x: Option<f32>,
    window_width: f32,
) -> Element<'static, Message> {
    let desktop = remote_desktop();
    let base = container(column![
        space().height(62),
        row![space().width(85), desktop, space().width(85)].height(650),
        space().height(Fill),
    ])
    .width(Fill)
    .height(Fill)
    .style(theme::shaped_panel(
        theme::palette().remote_canvas,
        if maximized {
            0.0.into()
        } else {
            WINDOW_RADIUS.into()
        },
    ));

    let toolbar_progress = app.session_toolbar_progress
        * app.session_toolbar_progress
        * (3.0 - 2.0 * app.session_toolbar_progress);
    let toolbar_visible = toolbar_progress > 0.01;
    let toolbar: Element<'static, Message> = if toolbar_visible {
        container(
            mouse_area(control_bar(app.session_toolbar_pinned, is_dark))
                .on_enter(Message::SessionToolbarInteraction)
                .on_press(Message::SessionToolbarInteraction),
        )
        .height(50.0 * toolbar_progress)
        .clip(true)
        .into()
    } else {
        toolbar_handle(Icon::ChevronDown, Message::ShowSessionToolbar, is_dark).into()
    };
    let toolbar_width = if toolbar_visible {
        SESSION_TOOLBAR_WIDTH
    } else {
        SESSION_TOOLBAR_COLLAPSED_WIDTH
    };
    let toolbar = container(toolbar).width(toolbar_width);
    let positioned: Element<'static, Message> = if let Some(center_x) = toolbar_x {
        let left =
            (center_x - toolbar_width / 2.0).clamp(0.0, (window_width - toolbar_width).max(0.0));
        row![space().width(left), toolbar, space().width(Fill)]
            .width(Fill)
            .align_y(Alignment::Start)
            .into()
    } else {
        container(toolbar)
            .width(Fill)
            .align_x(Alignment::Center)
            .into()
    };
    let controls = mouse_area(
        container(positioned)
            .width(Fill)
            .height(Fill)
            .align_y(Alignment::Start),
    )
    .on_move(Message::SessionToolbarPointerMoved)
    .on_release(Message::SessionToolbarDragEnded);
    let pulse = ((app.ui_time * TAU / 2.4).sin() + 1.0) * 0.5;
    let status = container(
        container(
            row![
                dot(
                    theme::mix(
                        theme::palette().success,
                        iced::Color::from_rgb8(126, 196, 145),
                        pulse
                    ),
                    6.0 + pulse * 1.4
                ),
                text("RGBA · 自适应质量 · 60 fps")
                    .size(MICRO_SIZE)
                    .color(theme::palette().text_muted)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding(8)
        .style(theme::bordered_panel(
            theme::palette().surface,
            CONTROL_RADIUS,
        )),
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

fn control_bar(pinned: bool, is_dark: bool) -> Element<'static, Message> {
    let action_button =
        |kind, action| toolbar_button(kind, false, Message::SessionAction(action), is_dark);
    let controls = container(
        row![
            toolbar_drag_handle(is_dark),
            action_button(Icon::Scan, SessionAction::Fit),
            action_button(Icon::ZoomIn, SessionAction::Zoom),
            action_button(Icon::Pointer, SessionAction::Input),
            action_button(Icon::Keyboard, SessionAction::SystemShortcut),
            action_button(Icon::Clipboard, SessionAction::Clipboard),
            action_button(Icon::Undo, SessionAction::Undo),
            toolbar_button(Icon::Pin, pinned, Message::ToggleSessionToolbarPin, is_dark,),
            toolbar_button(Icon::Fullscreen, false, Message::ToggleFullscreen, is_dark,),
        ]
        .spacing(2)
        .align_y(Alignment::Center),
    )
    .padding([3, 4])
    .style(theme::toolbar_glass(is_dark, 0.0.into()));

    column![
        controls,
        container(toolbar_handle(
            Icon::ChevronUp,
            Message::HideSessionToolbar,
            is_dark,
        ))
        .width(Fill)
        .center_x(Fill),
    ]
    .spacing(0)
    .align_x(Alignment::Center)
    .into()
}

fn toolbar_drag_handle(is_dark: bool) -> Element<'static, Message> {
    let mut color = theme::toolbar_foreground(is_dark);
    color.a = 0.5;
    mouse_area(
        container(icon(Icon::MoreHorizontal, 14.0, color))
            .width(22)
            .height(30)
            .center_x(22)
            .center_y(30),
    )
    .on_press(Message::SessionToolbarDragStarted)
    .on_release(Message::SessionToolbarDragEnded)
    .interaction(iced::mouse::Interaction::Grab)
    .into()
}

fn toolbar_button(
    kind: Icon,
    selected: bool,
    message: Message,
    is_dark: bool,
) -> iced::widget::Button<'static, Message> {
    button(
        container(icon(kind, 15.0, theme::toolbar_foreground(is_dark)))
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill),
    )
    .width(30)
    .height(30)
    .padding(0)
    .style(theme::toolbar_glass_button(is_dark, selected))
    .on_press(message)
}

fn toolbar_handle(
    kind: Icon,
    message: Message,
    is_dark: bool,
) -> iced::widget::Button<'static, Message> {
    let id = match kind {
        Icon::ChevronUp => "session-toolbar-collapse-handle",
        _ => "session-toolbar-expand-handle",
    };
    button(
        container(icon(kind, 12.0, theme::toolbar_foreground(is_dark)))
            .id(id)
            .width(Fill)
            .height(Fill)
            .center_x(Fill)
            .center_y(Fill),
    )
    .width(SESSION_TOOLBAR_COLLAPSED_WIDTH)
    .height(14)
    .padding(0)
    .style(theme::toolbar_handle(is_dark))
    .on_press(message)
}

fn remote_desktop() -> Element<'static, Message> {
    let menu = container(
        row![
            dot(theme::palette().text, 6.0),
            text("Finder")
                .size(CAPTION_SIZE)
                .color(theme::palette().text),
            text("文件   编辑   显示   前往   窗口   帮助")
                .size(MICRO_SIZE)
                .color(theme::palette().text_muted),
            space().width(Fill),
            text("Wi‑Fi   14:32")
                .size(MICRO_SIZE)
                .color(theme::palette().text_muted),
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
