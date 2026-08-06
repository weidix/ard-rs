use ard_rs::ArdVideoQuality;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, space, stack, text,
};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::theme::{
    self, BODY_SIZE, CAPTION_SIZE, CONTENT_PADDING_BOTTOM, CONTENT_PADDING_X, CONTROL_HEIGHT,
    CONTROL_PADDING_X, CONTROL_RADIUS, ICON_SIZE, MICRO_SIZE, TITLE_SIZE, WINDOW_RADIUS,
};
use crate::widgets::{icon_button, primary, secondary, window_chrome_with_title};
use crate::{ArdViewer, Message};

const TITLEBAR_HEIGHT: f32 = 44.0;

pub fn connection(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.is_window_maximized(window_id);
    stack![
        row![device_sidebar(app, maximized), form(app, maximized)]
            .height(Fill)
            .width(Fill),
        window_chrome_with_title(
            window_id,
            TITLEBAR_HEIGHT,
            if cfg!(target_os = "macos") {
                ""
            } else {
                "ARD Viewer"
            },
            None,
        ),
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
            crate::state::DeviceState::Online => "可连接",
            crate::state::DeviceState::Saved => "历史记录",
            crate::state::DeviceState::RecentlyUsed => "最近连接",
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
                text("历史连接")
                    .size(TITLE_SIZE)
                    .color(theme::palette().text),
                text("选择记录可快速填写")
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
                iced::widget::text_input("搜索历史连接", &app.search)
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
        if app.devices.is_empty() {
            container(
                column![
                    icon(Icon::Monitor, 24.0, theme::palette().text_dim),
                    text("暂无历史连接")
                        .size(BODY_SIZE)
                        .color(theme::palette().text_muted),
                    text("连接成功后会显示在这里")
                        .size(CAPTION_SIZE)
                        .color(theme::palette().text_dim),
                ]
                .spacing(6)
                .align_x(Alignment::Center),
            )
            .height(Fill)
            .width(Fill)
            .center(Fill)
        } else {
            container(scrollable(devices)).height(Fill).width(Fill)
        },
        secondary("移除所选记录", Message::ManageDevices)
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

    let address = row![
        column![
            text("远程地址")
                .size(BODY_SIZE)
                .color(theme::palette().text_muted),
            iced::widget::text_input("mac-studio.local", &app.address)
                .on_input(Message::AddressChanged)
                .padding([10.0, CONTROL_PADDING_X])
                .size(BODY_SIZE)
                .style(theme::input),
        ]
        .spacing(5)
        .width(Fill),
        column![
            text("端口")
                .size(BODY_SIZE)
                .color(theme::palette().text_muted),
            iced::widget::text_input("默认 5900", &app.port)
                .on_input(Message::PortChanged)
                .padding([10.0, CONTROL_PADDING_X])
                .size(BODY_SIZE)
                .style(theme::input),
        ]
        .spacing(5)
        .width(92),
    ]
    .spacing(14)
    .align_y(Alignment::End);
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
        row![
            iced::widget::text_input(
                if app.has_saved_password {
                    "已安全保存"
                } else {
                    "输入远程密码"
                },
                &app.password,
            )
            .on_input(Message::PasswordChanged)
            .secure(!app.password_visible)
            .padding([10.0, CONTROL_PADDING_X])
            .size(BODY_SIZE)
            .width(Fill)
            .style(theme::input),
            icon_button(
                if app.password_visible {
                    Icon::EyeOff
                } else {
                    Icon::Eye
                },
                Message::TogglePasswordVisibility,
            ),
        ]
        .height(CONTROL_HEIGHT)
        .align_y(Alignment::Center),
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
            .label("添加到历史连接")
            .on_toggle(Message::RememberDeviceChanged)
            .size(16)
            .text_size(BODY_SIZE)
            .style(theme::checkbox),
    ]
    .spacing(18);

    const QUALITIES: [&str; 5] = ["低画质", "中画质", "高画质", "自适应 MVS", "全画质"];
    let advanced = container(
        column![
            row![
                icon(Icon::Sliders, 14.0, theme::palette().text_muted),
                text("连接参数")
                    .size(BODY_SIZE)
                    .color(theme::palette().text),
            ]
            .spacing(7)
            .align_y(Alignment::Center),
            row![
                column![
                    text("视频质量")
                        .size(CAPTION_SIZE)
                        .color(theme::palette().text_muted),
                    pick_list(Some(app.quality.label()), QUALITIES, |value| {
                        (*value).to_owned()
                    })
                    .on_select(|value| Message::QualityChanged(match value {
                        "低画质" => ArdVideoQuality::Low,
                        "中画质" => ArdVideoQuality::Medium,
                        "高画质" => ArdVideoQuality::High,
                        "全画质" => ArdVideoQuality::Full,
                        _ => ArdVideoQuality::Adaptive,
                    }))
                    .padding([9, 12])
                    .text_size(BODY_SIZE)
                    .style(theme::pick_list)
                    .menu_style(theme::pick_list_menu),
                ]
                .spacing(5)
                .width(Fill),
                column![
                    text("帧率 (FPS)")
                        .size(CAPTION_SIZE)
                        .color(theme::palette().text_muted),
                    iced::widget::text_input("自动", &app.frame_rate)
                        .on_input(Message::FrameRateChanged)
                        .padding([10.0, CONTROL_PADDING_X])
                        .size(BODY_SIZE)
                        .style(theme::input),
                ]
                .spacing(5)
                .width(Fill),
            ]
            .spacing(12),
            text("像素格式：服务器原生  ·  缩放：适应窗口  ·  自动重连：已启用")
                .size(CAPTION_SIZE)
                .color(theme::palette().text_muted),
        ]
        .spacing(10),
    )
    .padding(12)
    .width(Fill)
    .style(theme::shaped_panel(
        theme::palette().surface,
        CONTROL_RADIUS.into(),
    ));

    let security = container(
        row![
            icon(Icon::Shield, ICON_SIZE, theme::palette().text_warm),
            text("密码使用应用本地加密凭据库保存，不写入明文配置文件。")
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
    .style(theme::shaped_panel(
        theme::palette().surface_active,
        CONTROL_RADIUS.into(),
    ));

    let actions = row![
        secondary("导出快捷方式", Message::ExportShortcuts)
            .width(132)
            .height(CONTROL_HEIGHT),
        space().width(Fill),
        secondary("加入历史", Message::SaveDevice)
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
