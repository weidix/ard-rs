use iced::widget::{button, column, container, mouse_area, row, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::Message;
use crate::icons::{Icon, icon};
use crate::theme::{
    self, ACCENT_TEXT, BODY_SIZE, CAPTION_SIZE, CARD_RADIUS, CONTROL_HEIGHT, ICON_SIZE, SURFACE,
    TEXT, TEXT_MUTED, WINDOW_RADIUS, WINDOW_TITLE_SIZE,
};

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

pub fn window_titlebar<'a>(
    window_id: window::Id,
    title: &'a str,
    subtitle: &'a str,
    action: Option<(Icon, Message)>,
    height: u16,
) -> Element<'a, Message> {
    #[cfg(target_os = "macos")]
    let native_controls: Element<'a, Message> = space().width(52).into();
    #[cfg(target_os = "windows")]
    let native_controls: Element<'a, Message> = space().width(14).into();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let native_controls: Element<'a, Message> = space().width(0).into();

    #[cfg(target_os = "macos")]
    let platform_buttons: Element<'a, Message> = space().width(1).into();
    #[cfg(target_os = "windows")]
    let platform_buttons: Element<'a, Message> = row![
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
    let platform_buttons: Element<'a, Message> = row![
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
    let platform_controls = container(platform_buttons)
        .height(Fill)
        .align_y(Alignment::Start);
    let titles = container(
        column![
            text(title).size(WINDOW_TITLE_SIZE).color(TEXT),
            text(subtitle).size(CAPTION_SIZE).color(TEXT_MUTED),
        ]
        .spacing(0),
    )
    .height(CONTROL_HEIGHT)
    .width(Fill)
    .align_y(Alignment::Start);
    let action: Element<'a, Message> = match action {
        Some((kind, message)) => icon_button(kind, message).into(),
        None => space().width(1).into(),
    };
    mouse_area(
        container(
            row![native_controls, titles, action, platform_controls]
                .spacing(10)
                .height(Fill)
                .align_y(Alignment::Center),
        )
        .height(u32::from(height))
        .width(Fill)
        .padding([0, if cfg!(target_os = "windows") { 0 } else { 14 }])
        .align_y(Alignment::Center)
        .style(theme::shaped_panel(
            SURFACE,
            iced::border::top(WINDOW_RADIUS),
        )),
    )
    .on_press(Message::DragWindow(window_id))
    .on_double_click(Message::ToggleMaximizeWindow(window_id))
    .into()
}

#[cfg(not(target_os = "macos"))]
fn titlebar_control<'a>(
    kind: Icon,
    message: Message,
    close: bool,
) -> iced::widget::Button<'a, Message> {
    button(centered_icon(kind, 10.0, TEXT))
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
    text(value.into()).color(TEXT_MUTED).size(CAPTION_SIZE)
}

pub fn secondary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(centered_label(label, BODY_SIZE, TEXT))
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
                icon(kind, ICON_SIZE, TEXT),
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
    button(centered_label(label, BODY_SIZE, ACCENT_TEXT))
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::primary_button)
        .on_press(message)
}

pub fn icon_button(kind: Icon, message: Message) -> iced::widget::Button<'static, Message> {
    button(centered_icon(kind, ICON_SIZE, TEXT))
        .width(CONTROL_HEIGHT)
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::secondary_button)
        .on_press(message)
}

pub fn icon_toggle_button(
    kind: Icon,
    selected: bool,
    message: Message,
) -> iced::widget::Button<'static, Message> {
    button(centered_icon(
        kind,
        ICON_SIZE,
        if selected { TEXT } else { TEXT_MUTED },
    ))
    .width(CONTROL_HEIGHT)
    .height(CONTROL_HEIGHT)
    .padding(0)
    .style(theme::toggle_button(selected))
    .on_press(message)
}

pub fn card<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title).color(TEXT).size(BODY_SIZE), body.into()].spacing(10))
        .width(Fill)
        .padding(16)
        .style(theme::bordered_panel(SURFACE, CARD_RADIUS))
        .into()
}
