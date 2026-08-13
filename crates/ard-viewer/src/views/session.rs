use std::f32::consts::TAU;

use iced::widget::{
    button, column, container, mouse_area, progress_bar, row, space, stack, text, text_input,
};
use iced::{Alignment, Element, Fill, Padding, window};

use crate::icons::{Icon, icon};
use crate::session_renderer;
use crate::session_runtime::ConnectionState;
use crate::state::ToolbarButton;
use crate::theme::{self, MICRO_SIZE, WINDOW_RADIUS};
use crate::widgets::window_chrome_with_title;
use crate::{ArdViewer, Message, SESSION_TOOLBAR_COLLAPSED_WIDTH, SessionAction};

pub(crate) const SESSION_TITLEBAR_HEIGHT: f32 = 50.0;

pub fn session(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.session_fullscreen || app.is_window_maximized(window_id);
    let endpoint = app
        .remote_endpoint()
        .unwrap_or_else(|_| app.address.trim().to_owned());
    let detail = if app.username.trim().is_empty() {
        None
    } else {
        Some(if app.language == crate::i18n::Language::English {
            format!("User {}", app.username.trim())
        } else {
            format!("用户 {}", app.username.trim())
        })
    };
    let canvas = remote_canvas(app, maximized, app.effective_dark(), app.session_fullscreen);
    if app.session_fullscreen {
        canvas
    } else {
        let titlebar = stack![
            container(space())
                .width(Fill)
                .height(SESSION_TITLEBAR_HEIGHT)
                .style(theme::panel(theme::palette().surface)),
            window_chrome_with_title(
                window_id,
                SESSION_TITLEBAR_HEIGHT,
                maximized,
                endpoint,
                detail,
            ),
            windowed_session_toolbar(app, app.effective_dark()),
        ]
        .height(SESSION_TITLEBAR_HEIGHT);
        column![
            container(titlebar)
                .id("session-window-chrome")
                .width(Fill)
                .height(SESSION_TITLEBAR_HEIGHT),
            canvas,
        ]
        .spacing(0)
        .width(Fill)
        .height(Fill)
        .into()
    }
}

fn remote_canvas(
    app: &ArdViewer,
    maximized: bool,
    is_dark: bool,
    show_toolbar: bool,
) -> Element<'_, Message> {
    let desktop: Element<'_, Message> = if let Some(runtime) = &app.session_runtime {
        container(session_renderer::remote_display(
            runtime.mailbox(),
            app.session_zoom,
            app.session_actual_size,
            runtime.should_interpolate(),
            runtime.sharp_sampling(),
        ))
        .height(Fill)
        .width(Fill)
        .style(theme::panel(theme::palette().remote_canvas))
        .into()
    } else {
        container(space())
            .height(Fill)
            .width(Fill)
            .style(theme::panel(theme::palette().remote_canvas))
            .into()
    };
    let base = container(desktop)
        .id("session-remote-canvas")
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

    let progress = connection_progress(app);
    let performance_hud = performance_hud(app, is_dark);

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

    if show_toolbar {
        stack![
            base,
            fullscreen_session_toolbar(app, is_dark),
            progress,
            performance_hud,
            ime_sink
        ]
        .width(Fill)
        .height(Fill)
        .clip(true)
        .into()
    } else {
        stack![base, progress, performance_hud, ime_sink]
            .width(Fill)
            .height(Fill)
            .clip(true)
            .into()
    }
}

fn performance_hud(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    if !app.show_performance_hud {
        return space().into();
    }
    let metrics = app.session_metrics;
    let requested = if metrics.requested_width == 0 || metrics.requested_height == 0 {
        if app.language == crate::i18n::Language::English {
            "auto/server".to_owned()
        } else {
            "自动/服务器".to_owned()
        }
    } else {
        format!("{}×{}", metrics.requested_width, metrics.requested_height)
    };
    let actual = if metrics.width == 0 || metrics.height == 0 {
        "—".to_owned()
    } else {
        format!("{}×{}", metrics.width, metrics.height)
    };
    let resolution_state = if metrics.requested_width == 0
        || metrics.requested_height == 0
        || metrics.width == 0
        || metrics.height == 0
    {
        ""
    } else if metrics.width == metrics.requested_width && metrics.height == metrics.requested_height
    {
        " ✓"
    } else {
        " ✗"
    };
    let negotiated = if metrics.negotiated_width == 0 || metrics.negotiated_height == 0 {
        "—".to_owned()
    } else {
        format!("{}×{}", metrics.negotiated_width, metrics.negotiated_height)
    };
    let scale = if metrics.presentation_scale > 0.0 {
        format!("{:.3}×", metrics.presentation_scale)
    } else {
        "—".to_owned()
    };
    let path = if metrics.native_nv12 {
        "NV12"
    } else if metrics.gpu_mvs {
        "MVS/GPU"
    } else {
        "RGBA"
    };
    let video_latency = if metrics.avc_timing_valid {
        if app.language == crate::i18n::Language::English {
            format!(
                "RTP reassembly {:.2} ms  DON reorder {:.2} ms\nrelease→decode {:.2} ms  receive→decode {:.2} ms",
                metrics.packet_reassembly_ms,
                metrics.don_reorder_ms,
                metrics.release_to_decode_ms,
                metrics.receive_to_decode_ms,
            )
        } else {
            format!(
                "RTP 重组 {:.2} ms  DON 排序 {:.2} ms\n释放→解码 {:.2} ms  收包→解码 {:.2} ms",
                metrics.packet_reassembly_ms,
                metrics.don_reorder_ms,
                metrics.release_to_decode_ms,
                metrics.receive_to_decode_ms,
            )
        }
    } else if app.language == crate::i18n::Language::English {
        "AVC stage timing —".to_owned()
    } else {
        "AVC 阶段耗时 —".to_owned()
    };
    let render_latency = if metrics.render_timing_valid {
        if app.language == crate::i18n::Language::English {
            format!(
                "decode→render encode {:.2} ms  receive→render encode {:.2} ms",
                metrics.decode_to_render_ms, metrics.receive_to_render_ms,
            )
        } else {
            format!(
                "解码→渲染编码 {:.2} ms  收包→渲染编码 {:.2} ms",
                metrics.decode_to_render_ms, metrics.receive_to_render_ms,
            )
        }
    } else if app.language == crate::i18n::Language::English {
        "render encode timing —".to_owned()
    } else {
        "渲染编码耗时 —".to_owned()
    };
    let input_frame_proxy = if metrics.input_to_next_frame_valid {
        if app.language == crate::i18n::Language::English {
            format!(
                "input flush→next received frame {:.2} ms (non-causal proxy)",
                metrics.input_to_next_frame_ms
            )
        } else {
            format!(
                "输入写出→下一收到帧 {:.2} ms（非因果代理值）",
                metrics.input_to_next_frame_ms
            )
        }
    } else if app.language == crate::i18n::Language::English {
        "input flush→next received frame —".to_owned()
    } else {
        "输入写出→下一收到帧 —".to_owned()
    };
    let label = if app.language == crate::i18n::Language::English {
        format!(
            "source {actual}  requested {requested}{resolution_state}  negotiated {negotiated}\ndisplay {scale}  {path}  {:.1} fps  {:.2} Mb/s\n{video_latency}\n{render_latency}\n{input_frame_proxy}\ninput queue avg {:.2} / peak {:.2} ms  depth {}\nTCP encode/write latest {:.2} / peak {:.2} ms  coalesced {}",
            metrics.frames_per_second,
            metrics.megabits_per_second,
            metrics.input_queue_average_ms,
            metrics.input_queue_peak_ms,
            metrics.input_queue_depth,
            metrics.input_write_ms,
            metrics.input_write_peak_ms,
            metrics.input_coalesced_pointer_moves,
        )
    } else {
        format!(
            "源图像 {actual}  请求 {requested}{resolution_state}  协商声明 {negotiated}\n显示比例 {scale}  {path}  {:.1} fps  {:.2} Mb/s\n{video_latency}\n{render_latency}\n{input_frame_proxy}\n输入队列 平均 {:.2} / 峰值 {:.2} ms  深度 {}\nTCP 编码/写入 最近 {:.2} / 峰值 {:.2} ms  位置合并 {}",
            metrics.frames_per_second,
            metrics.megabits_per_second,
            metrics.input_queue_average_ms,
            metrics.input_queue_peak_ms,
            metrics.input_queue_depth,
            metrics.input_write_ms,
            metrics.input_write_peak_ms,
            metrics.input_coalesced_pointer_moves,
        )
    };
    let panel = container(text(label).size(10).color(theme::palette().text))
        .padding(10)
        .style(theme::toolbar_glass(is_dark, 8.0.into()));
    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(12)
        .align_x(Alignment::End)
        .align_y(Alignment::End)
        .into()
}

fn fullscreen_session_toolbar(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    let toolbar_progress = app.session_toolbar_progress
        * app.session_toolbar_progress
        * (3.0 - 2.0 * app.session_toolbar_progress);
    let toolbar_visible = toolbar_progress > 0.01;
    let toolbar: Element<'static, Message> = if toolbar_visible {
        container(
            mouse_area(control_bar(app, is_dark))
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
        app.session_toolbar_width()
    } else {
        SESSION_TOOLBAR_COLLAPSED_WIDTH
    };
    positioned_session_toolbar(app, toolbar, toolbar_width, Alignment::Start)
}

fn windowed_session_toolbar(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    container(windowed_toolbar_controls(app, is_dark))
        .id("session-windowed-toolbar")
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
}

fn positioned_session_toolbar(
    app: &ArdViewer,
    toolbar: Element<'static, Message>,
    toolbar_width: f32,
    vertical_alignment: Alignment,
) -> Element<'static, Message> {
    let toolbar = container(toolbar).width(toolbar_width);
    let positioned: Element<'static, Message> = if let Some(center_x) = app.session_toolbar_x {
        let left = (center_x - toolbar_width / 2.0).clamp(
            0.0,
            (app.session_toolbar_window_width - toolbar_width).max(0.0),
        );
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
            .align_y(vertical_alignment),
    )
    .on_move(Message::SessionToolbarPointerMoved)
    .on_release(Message::SessionToolbarDragEnded);
    container(controls).width(Fill).height(Fill).into()
}

fn connection_progress(app: &ArdViewer) -> Element<'_, Message> {
    if app.session_connection == ConnectionState::Connected && app.session_error.is_none() {
        return space().into();
    }

    let label = app
        .session_error
        .clone()
        .unwrap_or_else(|| app.session_connection.label(app.language));
    let active = matches!(
        app.session_connection,
        ConnectionState::Connecting | ConnectionState::Reconnecting { .. }
    );
    let pulse = ((app.ui_time * TAU / 1.8).sin() + 1.0) * 0.5;
    let mut content = column![
        text(label)
            .size(MICRO_SIZE)
            .color(theme::palette().text_muted)
    ]
    .spacing(10)
    .align_x(Alignment::Center);
    if active {
        content = content.push(progress_bar(0.0..=1.0, pulse).length(180).girth(3));
    }

    let indicator = container(content).id("session-connection-progress");
    container(indicator)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .into()
}

fn control_bar(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    let controls = toolbar_controls(app, is_dark);
    let handle = container(embedded_toolbar_handle(is_dark))
        .width(SESSION_TOOLBAR_COLLAPSED_WIDTH)
        .height(14)
        .padding(Padding {
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
            left: 1.0,
        })
        .style(theme::toolbar_handle_shell(is_dark));

    column![controls, handle]
        .spacing(-2.0)
        .align_x(Alignment::Center)
        .into()
}

fn toolbar_controls(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    let mut controls = row![toolbar_drag_handle(is_dark)];
    for button in &app.toolbar_buttons {
        controls = controls.push(quick_button(*button, app, is_dark));
    }
    controls = controls
        .push(toolbar_button(
            Icon::Fullscreen,
            false,
            Message::ToggleFullscreen,
            is_dark,
        ))
        .push(toolbar_button(
            Icon::Pin,
            app.session_toolbar_pinned,
            Message::ToggleSessionToolbarPin,
            is_dark,
        ));
    let controls = controls.spacing(2).align_y(Alignment::Center);

    container(controls)
        .padding([3, 4])
        .style(theme::toolbar_glass(is_dark, 8.0.into()))
        .into()
}

fn windowed_toolbar_controls(app: &ArdViewer, is_dark: bool) -> Element<'static, Message> {
    let mut controls = row![];
    for button in &app.toolbar_buttons {
        controls = controls.push(quick_button(*button, app, is_dark));
    }
    controls = controls.push(toolbar_button(
        Icon::Fullscreen,
        false,
        Message::ToggleFullscreen,
        is_dark,
    ));
    controls.spacing(2).align_y(Alignment::Center).into()
}

fn quick_button(
    button: ToolbarButton,
    app: &ArdViewer,
    is_dark: bool,
) -> iced::widget::Button<'static, Message> {
    let selected = match button {
        ToolbarButton::SystemShortcut => app.capture_system_shortcuts,
        ToolbarButton::ActualSize => app.session_actual_size,
        _ => false,
    };
    let action = match button {
        ToolbarButton::Screenshot => SessionAction::Screenshot,
        ToolbarButton::AppSwitcher => SessionAction::AppSwitcher,
        ToolbarButton::MissionControl => SessionAction::MissionControl,
        ToolbarButton::Desktop => SessionAction::Desktop,
        ToolbarButton::ZoomOut => SessionAction::ZoomOut,
        ToolbarButton::ZoomIn => SessionAction::ZoomIn,
        ToolbarButton::ActualSize => SessionAction::ActualSize,
        ToolbarButton::FitToWindow => SessionAction::FitToWindow,
        ToolbarButton::RemoteKeyboard => SessionAction::RemoteKeyboard,
        ToolbarButton::Pointer => SessionAction::Pointer,
        ToolbarButton::Clipboard => SessionAction::Clipboard,
        ToolbarButton::SystemShortcut => SessionAction::SystemShortcut,
        ToolbarButton::Undo => SessionAction::Undo,
    };
    toolbar_button(
        button.icon(),
        selected,
        Message::SessionAction(action),
        is_dark,
    )
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

fn embedded_toolbar_handle(is_dark: bool) -> iced::widget::Button<'static, Message> {
    button(
        container(icon(
            Icon::ChevronUp,
            12.0,
            theme::toolbar_foreground(is_dark),
        ))
        .id("session-toolbar-collapse-handle")
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill),
    )
    .width(Fill)
    .height(Fill)
    .padding(0)
    .style(theme::toolbar_embedded_handle(is_dark))
    .on_press(Message::HideSessionToolbar)
}
