use iced::widget::{button, column, container, mouse_area, row, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::Message;
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

pub fn window_titlebar<'a>(
    window_id: window::Id,
    title: &'a str,
    subtitle: &'a str,
    action: Option<(&'a str, Message)>,
    height: u16,
) -> Element<'a, Message> {
    #[cfg(target_os = "macos")]
    let native_controls: Element<'a, Message> = space().width(52).into();
    #[cfg(not(target_os = "macos"))]
    let native_controls: Element<'a, Message> = space().width(0).into();

    #[cfg(target_os = "macos")]
    let platform_controls: Element<'a, Message> = space().width(1).into();
    #[cfg(not(target_os = "macos"))]
    let platform_controls: Element<'a, Message> = row![
        titlebar_control("—", Message::MinimizeWindow(window_id), false),
        titlebar_control("□", Message::ToggleMaximizeWindow(window_id), false),
        titlebar_control("×", Message::CloseWindow(window_id), true),
    ]
    .spacing(4)
    .into();
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
        Some((label, message)) => icon_button(label, message).into(),
        None => space().width(1).into(),
    };
    mouse_area(
        container(
            row![native_controls, titles, action, platform_controls]
                .spacing(10)
                .align_y(Alignment::Center),
        )
        .height(u32::from(height))
        .width(Fill)
        .padding([0, 14])
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
    label: &'static str,
    message: Message,
    close: bool,
) -> iced::widget::Button<'a, Message> {
    button(centered_label(label, ICON_SIZE, TEXT))
        .width(34)
        .height(28)
        .padding(0)
        .style(if close {
            theme::close_button
        } else {
            theme::secondary_button
        })
        .on_press(message)
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

pub fn icon_button<'a>(label: &'a str, message: Message) -> iced::widget::Button<'a, Message> {
    button(centered_label(label, ICON_SIZE, TEXT))
        .width(CONTROL_HEIGHT)
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::secondary_button)
        .on_press(message)
}

pub fn card<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title).color(TEXT).size(BODY_SIZE), body.into()].spacing(10))
        .width(Fill)
        .padding(16)
        .style(theme::bordered_panel(SURFACE, CARD_RADIUS))
        .into()
}
