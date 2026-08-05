use iced::widget::{button, checkbox, column, container, row, scrollable, space, stack, text};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::theme::{
    self, BODY_SIZE, CAPTION_SIZE, CARD_RADIUS, CONTENT_PADDING_BOTTOM, CONTENT_PADDING_X,
    CONTROL_HEIGHT, CONTROL_PADDING_X, CONTROL_RADIUS, ICON_SIZE, MICRO_SIZE, TITLE_SIZE,
    WINDOW_RADIUS,
};
use crate::widgets::{icon_button, primary, secondary, window_chrome_with_drag_height};
use crate::{ArdViewer, Message};

const TITLEBAR_HEIGHT: f32 = 44.0;

pub fn connection(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.is_window_maximized(window_id);
    stack![
        row![device_sidebar(app, maximized), form(app, maximized)]
            .height(Fill)
            .width(Fill),
        window_chrome_with_drag_height(window_id, TITLEBAR_HEIGHT),
    ]
    .height(Fill)
    .width(Fill)
    .into()
}

fn device_sidebar(app: &ArdViewer, maximized: bool) -> Element<'_, Message> {
    let query = app.search.trim().to_lowercase();
    let mut devices = column![].spacing(2);
    for (index, device) in app.devices.iter().enumerate().filter(|(_, device)| {
        query.is_empty()
            || device.name.to_lowercase().contains(&query)
            || device.address.to_lowercase().contains(&query)
    }) {
        let selected = index == app.selected_device;
        let selection = if selected {
            app.device_transition
        } else if index == app.previous_selected_device {
            1.0 - app.device_transition
        } else {
            0.0
        };
        let selection = selection * selection * (3.0 - 2.0 * selection);
        let state = match device.state {
            crate::state::DeviceState::Online => "在线",
            crate::state::DeviceState::Saved => "已保存",
            crate::state::DeviceState::RecentlyUsed => "12 分钟前",
        };
        let content = row![
            container(icon(Icon::Monitor, ICON_SIZE, theme::palette().text_warm))
                .width(24)
                .height(24)
                .center_x(24)
                .center_y(24),
            column![
                text(&device.name)
                    .size(BODY_SIZE)
                    .color(theme::palette().text),
                text(format!(
                    "{} · {state}",
                    device.address.trim_end_matches(":5900")
                ))
                .size(MICRO_SIZE)
                .color(theme::mix(
                    theme::palette().text_muted,
                    theme::palette().text_warm,
                    selection
                )),
            ]
            .spacing(1)
            .width(Fill),
            container(icon(Icon::ChevronRight, 12.0, theme::palette().text_muted))
                .height(Fill)
                .center_y(Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        devices = devices.push(
            button(content)
                .height(42)
                .width(Fill)
                .padding([5, 8])
                .style(theme::device_button(selection))
                .on_press(Message::DeviceSelected(index)),
        );
    }

    let body = column![
        row![
            column![
                text("已保存设备")
                    .size(TITLE_SIZE)
                    .color(theme::palette().text),
                text("点击即可快速连接")
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
            ]
            .spacing(2)
            .width(Fill),
            icon_button(Icon::Settings, Message::OpenSettings),
        ]
        .align_y(Alignment::Center),
        container(
            row![
                icon(Icon::Search, 14.0, theme::palette().text_muted),
                iced::widget::text_input("搜索设备", &app.search)
                    .on_input(Message::SearchChanged)
                    .padding([0, 0])
                    .size(BODY_SIZE)
                    .style(theme::inline_input),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .height(Fill),
        )
        .height(30)
        .padding([0, 8])
        .style(theme::bordered_panel(
            theme::palette().surface,
            CONTROL_RADIUS
        )),
        scrollable(devices).height(Fill),
        secondary("移除所选设备", Message::ManageDevices)
            .width(Fill)
            .height(CONTROL_HEIGHT),
    ]
    .spacing(8)
    .height(Fill);

    container(body)
        .width(252)
        .height(Fill)
        .padding(iced::Padding {
            top: TITLEBAR_HEIGHT,
            right: 12.0,
            bottom: 12.0,
            left: 12.0,
        })
        .style(theme::shaped_panel(
            theme::palette().surface,
            iced::border::left(if maximized { 0.0 } else { WINDOW_RADIUS }),
        ))
        .into()
}

fn form(app: &ArdViewer, maximized: bool) -> Element<'_, Message> {
    let heading = column![
        text("连接到远程设备")
            .size(TITLE_SIZE)
            .color(theme::palette().text),
        text("输入远程地址和凭据，密码由系统安全存储。")
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
    ]
    .spacing(2);

    let address = column![
        text("远程地址")
            .size(BODY_SIZE)
            .color(theme::palette().text_muted),
        iced::widget::text_input("mac-studio.local", &app.address)
            .on_input(Message::AddressChanged)
            .padding([10.0, CONTROL_PADDING_X])
            .size(BODY_SIZE)
            .style(theme::input),
    ]
    .spacing(5);
    let username = column![
        text("用户名")
            .size(BODY_SIZE)
            .color(theme::palette().text_muted),
        iced::widget::text_input("远程账户名", &app.username)
            .on_input(Message::UsernameChanged)
            .padding([10.0, CONTROL_PADDING_X])
            .size(BODY_SIZE)
            .style(theme::input),
    ]
    .spacing(5);
    let password = column![
        text("密码")
            .size(BODY_SIZE)
            .color(theme::palette().text_muted),
        iced::widget::text_input("••••••••••••", &app.password)
            .on_input(Message::PasswordChanged)
            .secure(true)
            .padding([10.0, CONTROL_PADDING_X])
            .size(BODY_SIZE)
            .style(theme::input),
    ]
    .spacing(5);
    let remembers = row![
        checkbox(app.remember_password)
            .label("记住密码")
            .on_toggle(Message::RememberPasswordChanged)
            .size(16)
            .text_size(BODY_SIZE)
            .style(theme::checkbox),
        checkbox(app.remember_device)
            .label("记住此设备")
            .on_toggle(Message::RememberDeviceChanged)
            .size(16)
            .text_size(BODY_SIZE)
            .style(theme::checkbox),
    ]
    .spacing(18);

    let advanced = container(
        column![
            row![
                text("连接参数")
                    .size(BODY_SIZE)
                    .color(theme::palette().text)
                    .width(Fill),
                icon(Icon::Sliders, 14.0, theme::palette().text_muted)
            ]
            .align_y(Alignment::Center),
            container(space().height(1))
                .width(Fill)
                .style(theme::panel(theme::palette().border)),
            text(format!(
                "端口  默认 5900        像素格式  自动        编码  {}",
                app.quality.label()
            ))
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
        ]
        .spacing(9),
    )
    .height(106)
    .padding(12)
    .width(Fill)
    .style(theme::bordered_panel(theme::palette().surface, CARD_RADIUS));

    let security = container(
        row![
            icon(Icon::Shield, ICON_SIZE, theme::palette().text_warm),
            text("密码使用操作系统密钥库加密保存，不写入配置文件。")
                .size(CAPTION_SIZE)
                .color(theme::palette().text_muted)
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .height(Fill),
    )
    .height(42)
    .padding(10)
    .width(Fill)
    .align_y(Alignment::Center)
    .style(theme::bordered_panel(
        theme::palette().surface_active,
        CONTROL_RADIUS,
    ));

    let actions = row![
        secondary("导出快捷方式", Message::ExportShortcuts)
            .width(132)
            .height(CONTROL_HEIGHT),
        space().width(Fill),
        secondary("保存", Message::SaveDevice)
            .width(76)
            .height(CONTROL_HEIGHT),
        primary("连接", Message::Connect)
            .width(88)
            .height(CONTROL_HEIGHT),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let body = column![
        heading,
        address,
        username,
        password,
        remembers,
        advanced,
        security,
        text(&app.status)
            .size(CAPTION_SIZE)
            .color(theme::palette().text_warm),
        space().height(Fill),
        actions,
    ]
    .spacing(15)
    .height(Fill)
    .width(Fill);

    container(body)
        .padding(iced::Padding {
            top: TITLEBAR_HEIGHT,
            right: CONTENT_PADDING_X,
            bottom: CONTENT_PADDING_BOTTOM,
            left: CONTENT_PADDING_X,
        })
        .height(Fill)
        .width(Fill)
        .style(theme::shaped_panel(
            theme::palette().background,
            iced::border::right(if maximized { 0.0 } else { WINDOW_RADIUS }),
        ))
        .into()
}
