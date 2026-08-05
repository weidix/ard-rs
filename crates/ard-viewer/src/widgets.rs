use iced::widget::{button, column, container, mouse_area, row, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::Message;
use crate::theme::{self, SURFACE, TEXT, TEXT_MUTED};

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
    let titles = column![
        text(title).size(13).color(TEXT),
        text(subtitle).size(9).color(TEXT_MUTED),
    ]
    .spacing(0)
    .width(Fill);
    let action: Element<'a, Message> = match action {
        Some((label, message)) => secondary(label, message).width(34).height(34).into(),
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
        .style(theme::shaped_panel(SURFACE, iced::border::top(12))),
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
    button(text(label).size(13))
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
    text(value.into()).color(TEXT_MUTED).size(13)
}

pub fn secondary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(text(label.into()).size(13))
        .padding([8, 12])
        .style(theme::secondary_button)
        .on_press(message)
}

pub fn primary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(text(label.into()).size(13))
        .padding([8, 16])
        .style(theme::primary_button)
        .on_press(message)
}

pub fn card<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title).color(TEXT).size(14), body.into()].spacing(10))
        .width(Fill)
        .padding(16)
        .style(theme::panel(SURFACE))
        .into()
}
