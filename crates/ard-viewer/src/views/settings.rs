use ard_rs::ArdVideoQuality;
use iced::widget::{checkbox, column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::state::SettingsSection;
use crate::theme::{
    self, BACKGROUND, BODY_SIZE, BORDER, CAPTION_SIZE, CARD_RADIUS, CONTENT_PADDING_X,
    CONTROL_HEIGHT, ICON_SIZE, SURFACE, SURFACE_ACTIVE, TEXT, TEXT_DIM, TEXT_MUTED, TITLE_SIZE,
    WINDOW_RADIUS,
};
use crate::widgets::{card, muted, secondary, secondary_with_icon, window_titlebar};
use crate::{ArdViewer, Message};

pub fn settings(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    column![
        window_titlebar(window_id, "配置", "ARD Viewer 偏好设置", None, 52),
        row![sidebar(app), content(app)].width(Fill).height(Fill),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn sidebar(app: &ArdViewer) -> Element<'_, Message> {
    let mut nav = column![text("设置").size(CAPTION_SIZE).color(TEXT_MUTED)].spacing(6);
    for section in SettingsSection::ALL {
        let selected = section == app.settings_section;
        nav = nav.push(
            iced::widget::button(container(
                row![
                    container(icon(
                        section.icon(),
                        ICON_SIZE,
                        if selected { TEXT } else { TEXT_MUTED }
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
        .padding(12)
        .style(theme::shaped_panel(
            SURFACE,
            iced::border::bottom_left(WINDOW_RADIUS),
        ))
        .into()
}

fn content(app: &ArdViewer) -> Element<'_, Message> {
    let section: Element<'_, Message> = match app.settings_section {
        SettingsSection::General => generic_section(app, "常规"),
        SettingsSection::Display => display(app),
        SettingsSection::KeyMapping => key_mapping(app),
        SettingsSection::Security => generic_section(app, "安全"),
        SettingsSection::About => generic_section(app, "关于"),
    };
    container(section)
        .padding([22.0, CONTENT_PADDING_X])
        .width(Fill)
        .height(Fill)
        .style(theme::shaped_panel(
            BACKGROUND,
            iced::border::bottom_right(WINDOW_RADIUS),
        ))
        .into()
}

fn key_mapping(app: &ArdViewer) -> Element<'_, Message> {
    let presets = ["macOS 默认", "Windows 默认", "Linux 默认"];
    let heading = column![
        text("按键映射").size(TITLE_SIZE).color(TEXT),
        text("配置本地按键如何发送到远程设备。")
            .size(CAPTION_SIZE)
            .color(TEXT_MUTED),
    ]
    .spacing(2);
    let controls = row![
        column![
            text("预设").size(BODY_SIZE).color(TEXT_MUTED),
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
                    .color(TEXT_MUTED)
                    .width(170),
                text("远程动作")
                    .size(CAPTION_SIZE)
                    .color(TEXT_MUTED)
                    .width(220),
                text("作用域")
                    .size(CAPTION_SIZE)
                    .color(TEXT_MUTED)
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
            SURFACE_ACTIVE,
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
                    .style(theme::panel(BORDER)),
            )
            .push(
                container(
                    row![
                        text(mapping.local).size(BODY_SIZE).color(TEXT).width(170),
                        text(mapping.remote).size(BODY_SIZE).color(TEXT).width(220),
                        text(mapping.scope).size(BODY_SIZE).color(TEXT).width(160),
                        iced::widget::button(
                            container(icon(Icon::MoreHorizontal, ICON_SIZE, TEXT_MUTED))
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
                .style(theme::panel(SURFACE)),
            );
    }
    let table = container(table)
        .width(Fill)
        .style(theme::bordered_panel(SURFACE, CARD_RADIUS));
    let add = row![
        secondary_with_icon(Icon::Plus, "添加映射", Message::AddMapping)
            .width(112)
            .height(CONTROL_HEIGHT),
        container(text("拖动可调整优先级").size(CAPTION_SIZE).color(TEXT_DIM))
            .height(CONTROL_HEIGHT)
            .center_y(CONTROL_HEIGHT),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let common = container(
        column![
            text("常用选项").size(BODY_SIZE).color(TEXT),
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
    .style(theme::bordered_panel(SURFACE, CARD_RADIUS));

    column![heading, controls, table, add, common, space().height(Fill)]
        .spacing(13)
        .width(Fill)
        .height(Fill)
        .into()
}

fn display(app: &ArdViewer) -> Element<'_, Message> {
    const QUALITIES: [&str; 5] = ["低", "中", "高", "自适应", "完整"];
    column![
        text("显示与性能").size(TITLE_SIZE).color(TEXT),
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

fn generic_section<'a>(app: &'a ArdViewer, title: &'static str) -> Element<'a, Message> {
    column![
        text(title).size(TITLE_SIZE).color(TEXT),
        muted(app.settings_section.subtitle()),
        card(
            "共享设置",
            column![
                checkbox(app.show_performance_hud)
                    .label("显示会话性能信息")
                    .on_toggle(Message::PerformanceHudChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
                muted("这些设置由三个应用窗口共享。"),
            ]
            .spacing(10)
        ),
    ]
    .spacing(13)
    .into()
}
