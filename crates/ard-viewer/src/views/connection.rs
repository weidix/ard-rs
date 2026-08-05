use iced::widget::{button, checkbox, column, container, row, scrollable, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::theme::{self, BACKGROUND, SURFACE, SURFACE_ACTIVE, TEXT, TEXT_MUTED, TEXT_WARM};
use crate::widgets::{primary, secondary, window_titlebar};
use crate::{ArdViewer, Message};

pub fn connection(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    column![
        window_titlebar(
            window_id,
            "ARD Viewer",
            "安全连接到远程设备",
            Some(("⚙", Message::OpenSettings)),
            52
        ),
        row![device_sidebar(app), form(app)]
            .height(Fill)
            .width(Fill),
    ]
    .height(Fill)
    .width(Fill)
    .into()
}

fn device_sidebar(app: &ArdViewer) -> Element<'_, Message> {
    let query = app.search.trim().to_lowercase();
    let mut devices = column![].spacing(12);
    for (index, device) in app.devices.iter().enumerate().filter(|(_, device)| {
        query.is_empty()
            || device.name.to_lowercase().contains(&query)
            || device.address.to_lowercase().contains(&query)
    }) {
        let selected = index == app.selected_device;
        let state = match device.state {
            crate::state::DeviceState::Online => "在线",
            crate::state::DeviceState::Saved => "已保存",
            crate::state::DeviceState::RecentlyUsed => "12 分钟前",
        };
        let content = row![
            container(text("▣").size(14).color(TEXT_WARM))
                .width(34)
                .height(34)
                .center_x(34)
                .center_y(34)
                .style(theme::rounded_panel(BACKGROUND, 8.0)),
            column![
                text(&device.name).size(11).color(TEXT),
                text(format!(
                    "{} · {state}",
                    device.address.trim_end_matches(":5900")
                ))
                .size(9)
                .color(if selected { TEXT_WARM } else { TEXT_MUTED }),
            ]
            .spacing(0)
            .width(Fill),
            text("›").size(15).color(TEXT_MUTED),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        devices = devices.push(
            button(content)
                .height(58)
                .width(Fill)
                .padding(10)
                .style(theme::device_button(selected))
                .on_press(Message::DeviceSelected(index)),
        );
    }

    let body = column![
        column![
            text("已保存设备").size(15).color(TEXT),
            text("点击即可快速连接").size(10).color(TEXT_MUTED)
        ]
        .spacing(2),
        iced::widget::text_input("⌕  搜索设备", &app.search)
            .on_input(Message::SearchChanged)
            .padding([8, 10])
            .style(theme::input),
        scrollable(devices).height(Fill),
        secondary("管理设备", Message::ManageDevices)
            .width(Fill)
            .height(34),
    ]
    .spacing(12)
    .height(Fill);

    container(body)
        .width(270)
        .height(Fill)
        .padding(16)
        .style(theme::shaped_panel(SURFACE, iced::border::bottom_left(12)))
        .into()
}

fn form(app: &ArdViewer) -> Element<'_, Message> {
    let heading = column![
        text("连接到远程设备").size(15).color(TEXT),
        text("输入远程地址和凭据，密码由系统安全存储。")
            .size(10)
            .color(TEXT_MUTED),
    ]
    .spacing(2);

    let address = column![
        text("远程地址").size(11).color(TEXT_MUTED),
        iced::widget::text_input("mac-studio.local", &app.address)
            .on_input(Message::AddressChanged)
            .padding([10, 12])
            .style(theme::input),
    ]
    .spacing(5);
    let password = column![
        text("密码").size(11).color(TEXT_MUTED),
        iced::widget::text_input("••••••••••••", &app.password)
            .on_input(Message::PasswordChanged)
            .secure(true)
            .padding([10, 12])
            .style(theme::input),
    ]
    .spacing(5);
    let remembers = row![
        checkbox(app.remember_password)
            .label("记住密码")
            .on_toggle(Message::RememberPasswordChanged)
            .size(16)
            .text_size(11)
            .style(theme::checkbox),
        checkbox(app.remember_device)
            .label("记住此设备")
            .on_toggle(Message::RememberDeviceChanged)
            .size(16)
            .text_size(11)
            .style(theme::checkbox),
    ]
    .spacing(18);

    let advanced = container(
        column![
            row![
                text("高级选项").size(11).color(TEXT).width(Fill),
                text("⌄").size(11).color(TEXT_MUTED)
            ]
            .align_y(Alignment::Center),
            container(space().height(1))
                .width(Fill)
                .style(theme::panel(crate::theme::BORDER)),
            text("端口  3283        像素格式  自动        编码  自适应")
                .size(10)
                .color(TEXT_MUTED),
        ]
        .spacing(9),
    )
    .height(106)
    .padding(12)
    .width(Fill)
    .style(theme::bordered_panel(SURFACE, 9.0));

    let security = container(
        row![
            text("◈").size(12).color(TEXT_WARM),
            text("密码使用操作系统密钥库加密保存，不写入配置文件。")
                .size(10)
                .color(TEXT_MUTED)
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .height(42)
    .padding(10)
    .width(Fill)
    .style(theme::bordered_panel(SURFACE_ACTIVE, 8.0));

    let actions = row![
        secondary("导出快捷方式", Message::ExportShortcuts)
            .width(132)
            .height(34),
        space().width(Fill),
        secondary("取消", Message::Cancel).width(76).height(34),
        primary("连接", Message::Connect).width(88).height(34),
    ]
    .spacing(10);

    let body = column![
        heading,
        address,
        password,
        remembers,
        advanced,
        security,
        space().height(Fill),
        actions,
    ]
    .spacing(15)
    .height(Fill)
    .width(Fill);

    container(body)
        .padding([24, 28])
        .height(Fill)
        .width(Fill)
        .style(theme::shaped_panel(
            BACKGROUND,
            iced::border::bottom_right(12),
        ))
        .into()
}
