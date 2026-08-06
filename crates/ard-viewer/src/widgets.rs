use iced::widget::{button, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Element, Fill, window};

use crate::Message;
use crate::icons::{Icon, icon};
use crate::theme::{self, BODY_SIZE, CAPTION_SIZE, CONTROL_HEIGHT, ICON_SIZE};

fn centered_label<'a>(
    label: impl Into<String>,
    size: f32,
    color: iced::Color,
) -> Element<'a, Message> {
    container(text(label.into()).size(size).color(color))
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

fn centered_icon(kind: Icon, size: f32, color: iced::Color) -> Element<'static, Message> {
    container(icon(kind, size, color))
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .into()
}

pub fn window_chrome_with_title(
    window_id: window::Id,
    drag_height: f32,
    title: impl Into<String>,
    detail: Option<String>,
) -> Element<'static, Message> {
    let leading = if cfg!(target_os = "macos") {
        78.0
    } else {
        12.0
    };
    let label = row![
        text(title.into())
            .size(BODY_SIZE)
            .color(theme::palette().text),
        text(detail.unwrap_or_default())
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    stack![
        window_drag_region_with_height(window_id, drag_height),
        container(label)
            .padding([0.0, leading])
            .height(drag_height)
            .align_y(Alignment::Center),
        window_platform_controls(window_id),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn window_drag_region_with_height(window_id: window::Id, height: f32) -> Element<'static, Message> {
    let leading_controls_width = if cfg!(target_os = "macos") { 72.0 } else { 8.0 };
    let trailing_controls_width = if cfg!(target_os = "windows") {
        138.0
    } else if cfg!(target_os = "macos") {
        0.0
    } else {
        110.0
    };

    let drag_handle = mouse_area(container(space()).width(Fill).height(height))
        .on_press(Message::DragWindow(window_id))
        .on_double_click(Message::ToggleMaximizeWindow(window_id));

    container(
        row![
            space().width(leading_controls_width),
            drag_handle,
            space().width(trailing_controls_width),
        ]
        .height(height)
        .align_y(Alignment::Start),
    )
    .width(Fill)
    .height(Fill)
    .align_y(Alignment::Start)
    .into()
}

pub fn window_platform_controls(window_id: window::Id) -> Element<'static, Message> {
    #[cfg(target_os = "macos")]
    let _ = window_id;

    #[cfg(target_os = "macos")]
    let platform_buttons: Element<'static, Message> = space().width(1).into();
    #[cfg(target_os = "windows")]
    let platform_buttons: Element<'static, Message> = row![
        titlebar_control(Icon::Minimize, Message::MinimizeWindow(window_id), false),
        titlebar_control(
            Icon::Maximize,
            Message::ToggleMaximizeWindow(window_id),
            false
        ),
        titlebar_control(Icon::Close, Message::CloseWindow(window_id), true),
    ]
    .into();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let platform_buttons: Element<'static, Message> = row![
        titlebar_control(Icon::Minimize, Message::MinimizeWindow(window_id), false),
        titlebar_control(
            Icon::Maximize,
            Message::ToggleMaximizeWindow(window_id),
            false
        ),
        titlebar_control(Icon::Close, Message::CloseWindow(window_id), true),
    ]
    .spacing(4)
    .into();

    container(platform_buttons)
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::Start)
        .into()
}

#[cfg(not(target_os = "macos"))]
fn titlebar_control<'a>(
    kind: Icon,
    message: Message,
    close: bool,
) -> iced::widget::Button<'a, Message> {
    button(centered_icon(kind, 10.0, theme::palette().text))
        .width(if cfg!(target_os = "windows") { 46 } else { 34 })
        .height(if cfg!(target_os = "windows") { 32 } else { 28 })
        .padding(0)
        .style(titlebar_control_style(close))
        .on_press(message)
}

#[cfg(target_os = "windows")]
fn titlebar_control_style(
    close: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style + Copy {
    theme::windows_caption_button(close)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn titlebar_control_style(
    close: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style + Copy {
    move |theme, status| {
        if close {
            theme::close_button(theme, status)
        } else {
            theme::secondary_button(theme, status)
        }
    }
}

pub fn muted<'a>(value: impl Into<String>) -> iced::widget::Text<'a> {
    text(value.into())
        .color(theme::palette().text_muted)
        .size(CAPTION_SIZE)
}

pub fn secondary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(centered_label(label, BODY_SIZE, theme::palette().text))
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::secondary_button)
        .on_press(message)
}

pub fn secondary_with_icon<'a>(
    kind: Icon,
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(
        container(
            row![
                icon(kind, ICON_SIZE, theme::palette().text),
                text(label.into()).size(BODY_SIZE)
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill),
    )
    .height(CONTROL_HEIGHT)
    .padding(0)
    .style(theme::secondary_button)
    .on_press(message)
}

pub fn primary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(centered_label(
        label,
        BODY_SIZE,
        theme::palette().accent_text,
    ))
    .height(CONTROL_HEIGHT)
    .padding(0)
    .style(theme::primary_button)
    .on_press(message)
}

pub fn icon_button(kind: Icon, message: Message) -> iced::widget::Button<'static, Message> {
    button(centered_icon(kind, ICON_SIZE, theme::palette().text))
        .width(CONTROL_HEIGHT)
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::secondary_button)
        .on_press(message)
}
