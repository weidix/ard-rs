use ard_rs::ArdVideoQuality;
use iced::widget::{checkbox, column, container, pick_list, row, space, text};
use iced::{Alignment, Element, Fill, window};

use crate::state::SettingsSection;
use crate::theme::{self, BACKGROUND, BORDER, SURFACE, SURFACE_ACTIVE, TEXT, TEXT_DIM, TEXT_MUTED};
use crate::widgets::{card, muted, secondary, window_titlebar};
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
    let mut nav = column![text("设置").size(10).color(TEXT_MUTED)].spacing(6);
    for section in SettingsSection::ALL {
        nav = nav.push(
            iced::widget::button(
                text(format!("{}   {}", section.icon(), section.label())).size(11),
            )
            .height(36)
            .padding(10)
            .width(Fill)
            .style(theme::nav_button(section == app.settings_section))
            .on_press(Message::SettingsSectionSelected(section)),
        );
    }
    container(nav)
        .width(210)
        .height(Fill)
        .padding(12)
        .style(theme::shaped_panel(SURFACE, iced::border::bottom_left(12)))
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
        .padding([22, 28])
        .width(Fill)
        .height(Fill)
        .style(theme::shaped_panel(
            BACKGROUND,
            iced::border::bottom_right(12),
        ))
        .into()
}

fn key_mapping(app: &ArdViewer) -> Element<'_, Message> {
    let presets = ["macOS 默认", "Windows 默认", "Linux 默认"];
    let heading = column![
        text("按键映射").size(15).color(TEXT),
        text("配置本地按键如何发送到远程设备。")
            .size(10)
            .color(TEXT_MUTED),
    ]
    .spacing(2);
    let controls = row![
        column![
            text("预设").size(11).color(TEXT_MUTED),
            pick_list(presets, Some(app.key_profile.as_str()), |value| {
                Message::KeyProfileChanged(value.to_owned())
            })
            .width(410)
            .padding([10, 12]),
        ]
        .spacing(5),
        secondary("复制预设", Message::CopyPreset)
            .width(104)
            .height(34),
        secondary("重置", Message::ResetMappings)
            .width(76)
            .height(34),
    ]
    .spacing(10)
    .align_y(Alignment::End);

    let mut table = column![
        container(row![
            text("本地按键").size(10).color(TEXT_MUTED).width(170),
            text("远程动作").size(10).color(TEXT_MUTED).width(220),
            text("作用域").size(10).color(TEXT_MUTED).width(160),
            text("").width(44),
        ])
        .height(34)
        .padding(10)
        .width(Fill)
        .style(theme::panel(SURFACE_ACTIVE)),
    ]
    .spacing(0);
    for (index, mapping) in app.mappings.iter().enumerate() {
        table = table.push(
            container(
                row![
                    text(mapping.local).size(11).color(TEXT).width(170),
                    text(mapping.remote).size(11).color(TEXT).width(220),
                    text(mapping.scope).size(11).color(TEXT).width(160),
                    iced::widget::button(text("•••").size(11).color(TEXT_MUTED))
                        .padding(0)
                        .width(44)
                        .style(theme::nav_button(false))
                        .on_press(Message::EditMapping(index)),
                ]
                .align_y(Alignment::Center),
            )
            .height(49)
            .padding(10)
            .width(Fill)
            .style(move |_| iced::widget::container::Style {
                background: Some(SURFACE.into()),
                border: iced::Border {
                    color: BORDER,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..iced::widget::container::Style::default()
            }),
        );
    }
    let table = container(table)
        .height(306)
        .width(Fill)
        .style(theme::bordered_panel(SURFACE, 9.0));
    let add = row![
        secondary("＋ 添加映射", Message::AddMapping)
            .width(112)
            .height(34),
        text("拖动可调整优先级").size(10).color(TEXT_DIM),
    ]
    .spacing(8)
    .align_y(Alignment::Start);
    let common = container(
        column![
            text("常用选项").size(11).color(TEXT),
            checkbox(app.auto_adapt_keyboard)
                .label("自动适配远程键盘布局")
                .on_toggle(Message::AutoAdaptChanged)
                .size(16)
                .text_size(11)
                .style(theme::checkbox),
            checkbox(app.capture_system_shortcuts)
                .label("在全屏模式中捕获系统快捷键")
                .on_toggle(Message::CaptureShortcutsChanged)
                .size(16)
                .text_size(11)
                .style(theme::checkbox),
        ]
        .spacing(10),
    )
    .height(110)
    .padding(12)
    .width(Fill)
    .style(theme::bordered_panel(SURFACE, 9.0));

    column![heading, controls, table, add, common, space().height(Fill)]
        .spacing(13)
        .width(Fill)
        .height(Fill)
        .into()
}

fn display(app: &ArdViewer) -> Element<'_, Message> {
    const QUALITIES: [&str; 5] = ["低", "中", "高", "自适应", "完整"];
    column![
        text("显示与性能").size(15).color(TEXT),
        muted("设置远程画面的质量和性能。"),
        card(
            "远程画面",
            column![
                row![
                    text("视频质量").width(150),
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
                ],
                row![
                    text("更新频率").width(150),
                    iced::widget::text_input("由服务器控制", &app.frame_interval_ms)
                        .on_input(Message::FrameIntervalChanged)
                        .width(180)
                        .style(theme::input)
                ],
            ]
            .spacing(12)
        ),
    ]
    .spacing(13)
    .into()
}

fn generic_section<'a>(app: &'a ArdViewer, title: &'static str) -> Element<'a, Message> {
    column![
        text(title).size(15).color(TEXT),
        muted(app.settings_section.subtitle()),
        card(
            "共享设置",
            column![
                checkbox(app.show_performance_hud)
                    .label("显示会话性能信息")
                    .on_toggle(Message::PerformanceHudChanged)
                    .style(theme::checkbox),
                muted("这些设置由三个应用窗口共享。"),
            ]
            .spacing(10)
        ),
    ]
    .spacing(13)
    .into()
}
