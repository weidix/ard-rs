use std::f32::consts::TAU;

use iced::widget::{button, column, container, mouse_area, row, space, stack, text, text_input};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::session_renderer;
use crate::session_runtime::ConnectionState;
use crate::theme::{self, CONTROL_RADIUS, MICRO_SIZE, WINDOW_RADIUS};
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
    let desktop: Element<'static, Message> = if let Some(runtime) = &app.session_runtime {
        container(session_renderer::remote_display(
            runtime.mailbox(),
            app.session_zoom,
        ))
        .height(650)
        .width(Fill)
        .style(theme::rounded_panel(theme::palette().remote_canvas, 6.0))
        .into()
    } else {
        container(text(app.session_connection.label()).color(theme::palette().text_muted))
            .height(650)
            .width(Fill)
            .center(Fill)
            .style(theme::rounded_panel(theme::palette().remote_canvas, 6.0))
            .into()
    };
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
    let connected = app.session_connection == ConnectionState::Connected;
    let status_color = if connected {
        theme::mix(
            theme::palette().success,
            iced::Color::from_rgb8(126, 196, 145),
            pulse,
        )
    } else {
        theme::palette().text_muted
    };
    let decoder = if app.session_metrics.gpu_mvs {
        "GPU MVS"
    } else {
        "RGBA"
    };
    let metrics = &app.session_metrics;
    let status_text = app.session_error.clone().unwrap_or_else(|| {
        if metrics.width > 0 && metrics.height > 0 {
            format!(
                "{decoder} · {} · {}×{} · {:.1} fps · ↓{:.2} Mbit/s · {}",
                app.quality.label(),
                metrics.width,
                metrics.height,
                metrics.frames_per_second,
                metrics.megabits_per_second,
                app.session_connection.label(),
            )
        } else {
            app.session_connection.label()
        }
    });
    let status = container(
        container(
            row![
                dot(status_color, 6.0 + pulse * 1.4),
                text(status_text)
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

    let ime_sink = container(
        text_input("", &app.ime_sink)
            .id(iced::widget::Id::new(crate::SESSION_IME_ID))
            .on_input(Message::ImeSinkChanged)
            .size(1)
            .padding(0)
            .width(1),
    )
    .width(1)
    .height(1)
    .clip(true);

    stack![base, controls, status, ime_sink]
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

fn dot(color: iced::Color, size: f32) -> Element<'static, Message> {
    container(space())
        .width(size)
        .height(size)
        .style(theme::rounded_panel(color, size / 2.0))
        .into()
}
