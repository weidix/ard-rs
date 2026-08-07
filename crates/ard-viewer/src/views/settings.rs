use iced::widget::{checkbox, column, container, row, space, stack, text};
use iced::{Alignment, Element, Fill, window};

use crate::icons::{Icon, icon};
use crate::state::{SettingsSection, ThemePreference};
use crate::theme::{
    self, BODY_SIZE, CAPTION_SIZE, CONTENT_PADDING_X, CONTROL_HEIGHT, CONTROL_RADIUS, ICON_SIZE,
    TITLE_SIZE, WINDOW_RADIUS,
};
use crate::widgets::{
    DropdownOption, DropdownSection, dropdown, muted, quality_dropdown_sections, secondary,
    secondary_with_icon, window_chrome_with_title,
};
use crate::{ArdViewer, DropdownMenu, Message};

pub fn settings(app: &ArdViewer, window_id: window::Id) -> Element<'_, Message> {
    let maximized = app.is_window_maximized(window_id);
    stack![
        row![sidebar(app, maximized), content(app, maximized)]
            .width(Fill)
            .height(Fill),
        window_chrome_with_title(window_id, 32.0, maximized, app.language.tr("设置"), None,),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn sidebar(app: &ArdViewer, maximized: bool) -> Element<'_, Message> {
    let mut nav = column![
        text(app.language.tr("偏好设置"))
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
                    text(section.label(app.language)).size(BODY_SIZE),
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
            top: 44.0,
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
        SettingsSection::About => about(app),
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
        app.language.tr("当前使用深色外观")
    } else {
        app.language.tr("当前使用浅色外观")
    };

    column![
        page_heading(
            app.language.tr("常规"),
            app.language.tr("配置应用的通用行为。"),
        ),
        settings_group(
            app.language.tr("外观"),
            column![
                setting_field(
                    app.language.tr("主题模式"),
                    dropdown(
                        app.theme_preference.label(app.language),
                        vec![DropdownSection::new(
                            None,
                            ThemePreference::ALL
                                .into_iter()
                                .map(|value| {
                                    DropdownOption::new(
                                        value.label(app.language),
                                        app.theme_preference == value,
                                        Message::ThemePreferenceChanged(value),
                                    )
                                    .id(match value {
                                        ThemePreference::System => "theme-option-system",
                                        ThemePreference::Light => "theme-option-light",
                                        ThemePreference::Dark => "theme-option-dark",
                                    })
                                })
                                .collect(),
                        )],
                        280.0,
                        BODY_SIZE,
                        app.open_dropdown == Some(DropdownMenu::Theme),
                        Message::ToggleDropdown(DropdownMenu::Theme),
                        Message::CloseDropdown,
                    ),
                ),
                setting_field(
                    app.language.tr("语言"),
                    dropdown(
                        app.language.label(),
                        vec![DropdownSection::new(
                            None,
                            crate::i18n::Language::ALL
                                .into_iter()
                                .map(|value| {
                                    DropdownOption::new(
                                        value.label(),
                                        app.language == value,
                                        Message::LanguageChanged(value),
                                    )
                                    .id(match value {
                                        crate::i18n::Language::SimplifiedChinese => {
                                            "language-option-zh"
                                        }
                                        crate::i18n::Language::English => "language-option-en",
                                    })
                                })
                                .collect(),
                        )],
                        280.0,
                        BODY_SIZE,
                        app.open_dropdown == Some(DropdownMenu::Language),
                        Message::ToggleDropdown(DropdownMenu::Language),
                        Message::CloseDropdown,
                    ),
                ),
                text(active_theme)
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted),
                checkbox(app.show_performance_hud)
                    .label(app.language.tr("显示会话性能信息"))
                    .on_toggle(Message::PerformanceHudChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
            ]
            .spacing(12),
        ),
        settings_group(
            app.language.tr("输入控制"),
            column![
                checkbox(app.reverse_scroll)
                    .label(app.language.tr("反转滚轮方向"))
                    .on_toggle(Message::ReverseScrollChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
                muted(app.language.tr("同时反转垂直和水平滚动方向。")),
            ]
            .spacing(8),
        ),
    ]
    .spacing(24)
    .into()
}

fn page_heading<'a>(title: impl Into<String>, subtitle: impl Into<String>) -> Element<'a, Message> {
    column![
        text(title.into())
            .size(TITLE_SIZE)
            .color(theme::palette().text),
        muted(subtitle.into()),
    ]
    .spacing(2)
    .into()
}

fn settings_group<'a>(
    title: impl Into<String>,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![
        text(title.into())
            .size(BODY_SIZE)
            .color(theme::palette().text),
        content.into(),
    ]
    .spacing(10)
    .into()
}

fn setting_field<'a>(
    label: impl Into<String>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![
        text(label.into())
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
        control.into(),
    ]
    .spacing(5)
    .into()
}

fn key_mapping(app: &ArdViewer) -> Element<'_, Message> {
    let presets = ["macOS 默认", "Windows 默认", "Linux 默认"];
    let heading = page_heading(
        app.language.tr("按键映射"),
        app.language.tr("配置本地按键如何发送到远程设备。"),
    );
    let controls = row![
        column![
            text(app.language.tr("预设"))
                .size(BODY_SIZE)
                .color(theme::palette().text_muted),
            dropdown(
                app.language.tr(&app.key_profile),
                vec![DropdownSection::new(
                    None,
                    presets
                        .into_iter()
                        .map(|value| {
                            DropdownOption::new(
                                app.language.tr(value),
                                app.key_profile == value,
                                Message::KeyProfileChanged(value.to_owned()),
                            )
                            .id(match value {
                                "Windows 默认" => "key-profile-option-windows",
                                "Linux 默认" => "key-profile-option-linux",
                                _ => "key-profile-option-macos",
                            })
                        })
                        .collect(),
                )],
                410.0,
                BODY_SIZE,
                app.open_dropdown == Some(DropdownMenu::KeyProfile),
                Message::ToggleDropdown(DropdownMenu::KeyProfile),
                Message::CloseDropdown,
            ),
        ]
        .spacing(5),
        secondary(app.language.tr("复制预设"), Message::CopyPreset)
            .width(104)
            .height(CONTROL_HEIGHT),
        secondary(app.language.tr("重置"), Message::ResetMappings)
            .width(76)
            .height(CONTROL_HEIGHT),
    ]
    .spacing(10)
    .align_y(Alignment::End);

    let mut table = column![
        container(
            row![
                text(app.language.tr("本地按键"))
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
                    .width(170),
                text(app.language.tr("远程动作"))
                    .size(CAPTION_SIZE)
                    .color(theme::palette().text_muted)
                    .width(220),
                text(app.language.tr("作用域"))
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
            iced::border::top(CONTROL_RADIUS),
        )),
    ]
    .spacing(0);
    for (index, mapping) in app.mappings.iter().enumerate() {
        table = table.push(
            container(
                row![
                    text(&mapping.local)
                        .size(BODY_SIZE)
                        .color(theme::palette().text)
                        .width(170),
                    text(app.language.tr(&mapping.remote))
                        .size(BODY_SIZE)
                        .color(theme::palette().text)
                        .width(220),
                    text(app.language.tr(&mapping.scope))
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
    let table = container(table).width(Fill).style(theme::shaped_panel(
        theme::palette().surface,
        CONTROL_RADIUS.into(),
    ));
    let add = row![
        secondary_with_icon(
            Icon::Plus,
            app.language.tr("添加常用映射"),
            Message::AddMapping
        )
        .width(112)
        .height(CONTROL_HEIGHT),
        container(
            text(app.language.tr("拖动可调整优先级"))
                .size(CAPTION_SIZE)
                .color(theme::palette().text_dim)
        )
        .height(CONTROL_HEIGHT)
        .center_y(CONTROL_HEIGHT),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let common = settings_group(
        app.language.tr("常用选项"),
        column![
            checkbox(app.auto_adapt_keyboard)
                .label(app.language.tr("自动适配远程键盘布局"))
                .on_toggle(Message::AutoAdaptChanged)
                .size(16)
                .text_size(BODY_SIZE)
                .style(theme::checkbox),
            checkbox(app.capture_system_shortcuts)
                .label(app.language.tr("在窗口获取焦点时屏蔽系统快捷键"))
                .on_toggle(Message::CaptureShortcutsChanged)
                .size(16)
                .text_size(BODY_SIZE)
                .style(theme::checkbox),
        ]
        .spacing(10),
    );

    column![
        heading,
        controls,
        table,
        add,
        space().height(4),
        common,
        space().height(Fill)
    ]
    .spacing(18)
    .width(Fill)
    .height(Fill)
    .into()
}

fn display(app: &ArdViewer) -> Element<'_, Message> {
    column![
        page_heading(
            app.language.tr("显示与性能"),
            app.language.tr("设置远程画面的质量和性能。"),
        ),
        settings_group(
            app.language.tr("远程画面"),
            row![
                container(setting_field(
                    app.language.tr("视频质量"),
                    dropdown(
                        app.language.tr(app.quality.label()),
                        quality_dropdown_sections(app.language, app.quality),
                        Fill,
                        BODY_SIZE,
                        app.open_dropdown == Some(DropdownMenu::DisplayQuality),
                        Message::ToggleDropdown(DropdownMenu::DisplayQuality),
                        Message::CloseDropdown,
                    ),
                ))
                .width(Fill),
                container(setting_field(
                    app.language.tr("帧率 (FPS)"),
                    iced::widget::text_input(app.language.tr("自动"), &app.frame_rate)
                        .on_input(Message::FrameRateChanged)
                        .padding([10, 12])
                        .size(BODY_SIZE)
                        .width(Fill)
                        .style(theme::input),
                ))
                .width(Fill),
            ]
            .spacing(12)
        ),
    ]
    .spacing(24)
    .into()
}

fn security(app: &ArdViewer) -> Element<'_, Message> {
    column![
        page_heading(
            app.language.tr("安全"),
            app.language.tr("管理本地凭据与连接数据。"),
        ),
        settings_group(
            app.language.tr("凭据存储"),
            column![
                checkbox(app.remember_password)
                    .label(app.language.tr("在应用本地加密凭据库中保存当前设备密码"))
                    .on_toggle(Message::RememberPasswordChanged)
                    .size(16)
                    .text_size(BODY_SIZE)
                    .style(theme::checkbox),
                muted(
                    app.language
                        .tr("密码保存在独立的 AES-256-GCM 加密文件中，不会写入明文配置。")
                ),
            ]
            .spacing(10)
        ),
    ]
    .spacing(24)
    .into()
}

fn about(app: &ArdViewer) -> Element<'static, Message> {
    column![
        page_heading(
            app.language.tr("关于"),
            app.language.tr("Apple Remote Desktop 原生 Rust 客户端。"),
        ),
        settings_group(
            "ARD Viewer",
            column![
                text(if app.language == crate::i18n::Language::English {
                    format!("Version {}", env!("CARGO_PKG_VERSION"))
                } else {
                    format!("版本 {}", env!("CARGO_PKG_VERSION"))
                })
                .size(BODY_SIZE),
                muted(
                    app.language
                        .tr("支持 ARD 认证、加密传输、MVS GPU 解码、键鼠输入、剪贴板与自动重连。")
                ),
                muted(app.language.tr("许可证：MIT OR Apache-2.0")),
            ]
            .spacing(10)
        ),
    ]
    .spacing(24)
    .into()
}
