use ard_rs::ArdVideoQuality;
use iced::widget::{checkbox, column, container, pick_list, row, space, stack, text};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::state::{SettingsSection, ThemePreference};
use crate::theme::{
    self, BODY_SIZE, CAPTION_SIZE, CARD_RADIUS, CONTENT_PADDING_X, CONTROL_HEIGHT, ICON_SIZE,
    TITLE_SIZE, WINDOW_RADIUS,
};
use crate::widgets::{card, muted, secondary, secondary_with_icon, window_chrome};
use crate::{ArdViewer, Message};

pub fn settings(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.is_window_maximized(window_id);
    stack![
        row![sidebar(app, maximized), content(app, maximized)]
            .width(Fill)
            .height(Fill),
        window_chrome(window_id),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn sidebar(app: &ArdViewer, maximized: bool) -> Element<'_, Message> {
    let mut nav = column![
        text("设置")
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted)
    ]
    .spacing(6);
    for section in SettingsSection::ALL {
        let selected = section == app.settings_section;
        nav = nav.push(
            iced::widget::button(container(
                row![
                    container(icon(
                        section.icon(),
                        ICON_SIZE,
                        if selected {
                            theme::palette().text
                        } else {
                            theme::palette().text_muted
                        }
                    ))
                    .width(20)
                    .height(Fill)
                    .center_x(20)
                    .center_y(Fill),
                    text(section.label()).size(BODY_SIZE),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .height(Fill)
                .width(Fill),
            ))
            .height(36)
            .padding([0, 10])
            .width(Fill)
            .style(theme::nav_button(selected))
            .on_press(Message::SettingsSectionSelected(section)),
        );
    }
    container(nav)
        .width(210)
        .height(Fill)
        .padding(iced::Padding {
            top: if cfg!(target_os = "macos") {
                44.0
            } else {
                12.0
            },
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

fn content(app: &ArdViewer, maximized: bool) -> Element<'_, Message> {
    let progress =
        app.settings_transition * app.settings_transition * (3.0 - 2.0 * app.settings_transition);
    let offset = 12.0 * (1.0 - progress);
    let section: Element<'_, Message> = match app.settings_section {
        SettingsSection::General => general(app),
        SettingsSection::Display => display(app),
        SettingsSection::KeyMapping => key_mapping(app),
        SettingsSection::Security => security(app),
        SettingsSection::About => about(),
    };
    container(section)
        .padding(iced::Padding {
            top: 22.0 + offset,
            right: CONTENT_PADDING_X,
            bottom: 22.0 - offset,
            left: CONTENT_PADDING_X,
        })
        .width(Fill)
        .height(Fill)
        .style(theme::shaped_panel(
            theme::palette().background,
            iced::border::right(if maximized { 0.0 } else { WINDOW_RADIUS }),
        ))
        .into()
}

fn general(app: &ArdViewer) -> Element<'_, Message> {
    let active_theme = if app.effective_dark() {
        "当前使用深色外观"
    } else {
        "当前使用浅色外观"
    };

    column![
        text("常规").size(TITLE_SIZE).color(theme::palette().text),
        muted("配置应用的通用行为。"),
        card(
            "外观",
            column![
                row![
                    text("主题模式").size(BODY_SIZE).width(150),
                    pick_list(
                        ThemePreference::ALL,
                        Some(app.theme_preference),
                        Message::ThemePreferenceChanged,
                    )
                    .width(180)
                    .padding([10, 12])
                    .text_size(BODY_SIZE)
                    .style(theme::pick_list)
                    .menu_style(theme::pick_list_menu),
                ]
                .align_y(Alignment::Center),
                text(active_theme)
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted),
                checkbox(app.show_performance_hud)
                    .label("显示会话性能信息")
                    .on_toggle(Message::PerformanceHudChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
            ]
            .spacing(12),
        ),
    ]
    .spacing(13)
    .into()
}

fn key_mapping(app: &ArdViewer) -> Element<'_, Message> {
    let presets = ["macOS 默认", "Windows 默认", "Linux 默认"];
    let heading = column![
        text("按键映射")
            .size(TITLE_SIZE)
            .color(theme::palette().text),
        text("配置本地按键如何发送到远程设备。")
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
    ]
    .spacing(2);
    let controls = row![
        column![
            text("预设")
                .size(BODY_SIZE)
                .color(theme::palette().text_muted),
            pick_list(presets, Some(app.key_profile.as_str()), |value| {
                Message::KeyProfileChanged(value.to_owned())
            })
            .width(410)
            .padding([10, 12])
            .text_size(BODY_SIZE)
            .style(theme::pick_list)
            .menu_style(theme::pick_list_menu),
        ]
        .spacing(5),
        secondary("复制预设", Message::CopyPreset)
            .width(104)
            .height(CONTROL_HEIGHT),
        secondary("重置", Message::ResetMappings)
            .width(76)
            .height(CONTROL_HEIGHT),
    ]
    .spacing(10)
    .align_y(Alignment::End);

    let mut table = column![
        container(
            row![
                text("本地按键")
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
                    .width(170),
                text("远程动作")
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
                    .width(220),
                text("作用域")
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
                    .width(160),
                text("").width(44),
            ]
            .align_y(Alignment::Center)
        )
        .height(34)
        .padding([0, 10])
        .width(Fill)
        .align_y(Alignment::Center)
        .style(theme::shaped_panel(
            theme::palette().surface_active,
            iced::border::top(CARD_RADIUS),
        )),
    ]
    .spacing(0);
    for (index, mapping) in app.mappings.iter().enumerate() {
        table = table
            .push(
                container(space())
                    .height(1)
                    .width(Fill)
                    .style(theme::panel(theme::palette().border)),
            )
            .push(
                container(
                    row![
                        text(&mapping.local)
                            .size(BODY_SIZE)
                            .color(theme::palette().text)
                            .width(170),
                        text(&mapping.remote)
                            .size(BODY_SIZE)
                            .color(theme::palette().text)
                            .width(220),
                        text(&mapping.scope)
                            .size(BODY_SIZE)
                            .color(theme::palette().text)
                            .width(160),
                        iced::widget::button(
                            container(icon(Icon::Minus, ICON_SIZE, theme::palette().text_muted))
                                .width(Fill)
                                .height(Fill)
                                .center_x(Fill)
                                .center_y(Fill)
                        )
                        .padding(0)
                        .width(44)
                        .height(32)
                        .style(theme::nav_button(false))
                        .on_press(Message::EditMapping(index)),
                    ]
                    .align_y(Alignment::Center),
                )
                .height(49)
                .padding([0, 10])
                .width(Fill)
                .align_y(Alignment::Center)
                .style(theme::panel(theme::palette().surface)),
            );
    }
    let table = container(table)
        .width(Fill)
        .style(theme::bordered_panel(theme::palette().surface, CARD_RADIUS));
    let add = row![
        secondary_with_icon(Icon::Plus, "添加常用映射", Message::AddMapping)
            .width(112)
            .height(CONTROL_HEIGHT),
        container(
            text("拖动可调整优先级")
                .size(CAPTION_SIZE)
                .color(theme::palette().text_dim)
        )
        .height(CONTROL_HEIGHT)
        .center_y(CONTROL_HEIGHT),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let common = container(
        column![
            text("常用选项")
                .size(BODY_SIZE)
                .color(theme::palette().text),
            checkbox(app.auto_adapt_keyboard)
                .label("自动适配远程键盘布局")
                .on_toggle(Message::AutoAdaptChanged)
                .size(16)
                .text_size(BODY_SIZE)
                .style(theme::checkbox),
            checkbox(app.capture_system_shortcuts)
                .label("在全屏模式中捕获系统快捷键")
                .on_toggle(Message::CaptureShortcutsChanged)
                .size(16)
                .text_size(BODY_SIZE)
                .style(theme::checkbox),
        ]
        .spacing(10),
    )
    .height(110)
    .padding(12)
    .width(Fill)
    .style(theme::bordered_panel(theme::palette().surface, CARD_RADIUS));

    column![heading, controls, table, add, common, space().height(Fill)]
        .spacing(13)
        .width(Fill)
        .height(Fill)
        .into()
}

fn display(app: &ArdViewer) -> Element<'_, Message> {
    const QUALITIES: [&str; 5] = ["低", "中", "高", "自适应", "完整"];
    column![
        text("显示与性能")
            .size(TITLE_SIZE)
            .color(theme::palette().text),
        muted("设置远程画面的质量和性能。"),
        card(
            "远程画面",
            column![
                row![
                    text("视频质量").size(BODY_SIZE).width(150),
                    pick_list(
                        QUALITIES,
                        Some(match app.quality {
                            ArdVideoQuality::Low => "低",
                            ArdVideoQuality::Medium => "中",
                            ArdVideoQuality::High => "高",
                            ArdVideoQuality::Full => "完整",
                            _ => "自适应",
                        }),
                        |value| Message::QualityChanged(match value {
                            "低" => ArdVideoQuality::Low,
                            "中" => ArdVideoQuality::Medium,
                            "高" => ArdVideoQuality::High,
                            "完整" => ArdVideoQuality::Full,
                            _ => ArdVideoQuality::Adaptive,
                        })
                    )
                    .padding([10, 12])
                    .text_size(BODY_SIZE)
                    .style(theme::pick_list)
                    .menu_style(theme::pick_list_menu)
                ]
                .align_y(Alignment::Center),
                row![
                    text("更新频率").size(BODY_SIZE).width(150),
                    iced::widget::text_input("由服务器控制", &app.frame_interval_ms)
                        .on_input(Message::FrameIntervalChanged)
                        .width(180)
                        .padding([10, 12])
                        .size(BODY_SIZE)
                        .style(theme::input)
                ]
                .align_y(Alignment::Center),
            ]
            .spacing(12)
        ),
    ]
    .spacing(13)
    .into()
}

fn security(app: &ArdViewer) -> Element<'_, Message> {
    column![
        text("安全").size(TITLE_SIZE).color(theme::palette().text),
        muted("管理本地凭据与连接数据。"),
        card(
            "凭据存储",
            column![
                checkbox(app.remember_password)
                    .label("在操作系统密钥库中保存当前设备密码")
                    .on_toggle(Message::RememberPasswordChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
                muted("配置文件只保存设备地址、用户名和界面偏好；密码不会写入配置文件。"),
            ]
            .spacing(10)
        ),
    ]
    .spacing(13)
    .into()
}

fn about() -> Element<'static, Message> {
    column![
        text("关于").size(TITLE_SIZE).color(theme::palette().text),
        muted("Apple Remote Desktop 原生 Rust 客户端。"),
        card(
            "ARD Viewer",
            column![
                text(format!("版本 {}", env!("CARGO_PKG_VERSION"))).size(BODY_SIZE),
                muted("支持 ARD 认证、加密传输、MVS GPU 解码、键鼠输入、剪贴板与自动重连。"),
                muted("许可证：MIT OR Apache-2.0"),
            ]
            .spacing(10)
        ),
    ]
    .spacing(13)
    .into()
}
