use ard_rs::ArdVideoQuality;
use iced::widget::{button, column, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Element, Event, Fill, Length, Point, Rectangle, Size, Vector, window};

use crate::Message;
use crate::i18n::Language;
use crate::icons::{Icon, icon};
use crate::theme::{self, BODY_SIZE, CAPTION_SIZE, CONTROL_HEIGHT, ICON_SIZE};

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

pub fn window_chrome_with_title(
    window_id: window::Id,
    drag_height: f32,
    maximized: bool,
    title: impl Into<String>,
    detail: Option<String>,
) -> Element<'static, Message> {
    let leading = if cfg!(target_os = "macos") {
        78.0
    } else {
        12.0
    };
    let label = row![
        text(title.into())
            .size(BODY_SIZE)
            .color(theme::palette().text),
        text(detail.unwrap_or_default())
            .size(CAPTION_SIZE)
            .color(theme::palette().text_muted),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    stack![
        window_drag_region_with_height(window_id, drag_height),
        container(label)
            .padding([0.0, leading])
            .height(drag_height)
            .align_y(Alignment::Center),
        window_platform_controls(window_id, maximized),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn window_drag_region_with_height(window_id: window::Id, height: f32) -> Element<'static, Message> {
    let leading_controls_width = if cfg!(target_os = "macos") { 72.0 } else { 8.0 };
    let trailing_controls_width = if cfg!(target_os = "windows") {
        150.0
    } else if cfg!(target_os = "macos") {
        0.0
    } else {
        128.0
    };

    let drag_handle = mouse_area(container(space()).width(Fill).height(height))
        .on_press(Message::DragWindow(window_id))
        .on_double_click(Message::ToggleMaximizeWindow(window_id));

    container(
        row![
            space().width(leading_controls_width),
            drag_handle,
            space().width(trailing_controls_width),
        ]
        .height(height)
        .align_y(Alignment::Start),
    )
    .width(Fill)
    .height(Fill)
    .align_y(Alignment::Start)
    .into()
}

pub fn window_platform_controls(
    window_id: window::Id,
    maximized: bool,
) -> Element<'static, Message> {
    #[cfg(target_os = "macos")]
    let _ = (window_id, maximized);

    #[cfg(target_os = "macos")]
    let platform_buttons: Element<'static, Message> = space().width(1).into();
    #[cfg(target_os = "windows")]
    let platform_buttons: Element<'static, Message> = row![
        titlebar_control(Icon::Minimize, Message::MinimizeWindow(window_id), false),
        titlebar_control(
            if maximized {
                Icon::Restore
            } else {
                Icon::Maximize
            },
            Message::ToggleMaximizeWindow(window_id),
            false
        ),
        titlebar_control(Icon::Close, Message::CloseWindow(window_id), true),
    ]
    .into();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let platform_buttons: Element<'static, Message> = row![
        titlebar_control(Icon::Minimize, Message::MinimizeWindow(window_id), false),
        titlebar_control(
            if maximized {
                Icon::Restore
            } else {
                Icon::Maximize
            },
            Message::ToggleMaximizeWindow(window_id),
            false
        ),
        titlebar_control(Icon::Close, Message::CloseWindow(window_id), true),
    ]
    .spacing(4)
    .into();

    container(platform_buttons)
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::Start)
        .into()
}

#[cfg(not(target_os = "macos"))]
fn titlebar_control<'a>(
    kind: Icon,
    message: Message,
    close: bool,
) -> iced::widget::Button<'a, Message> {
    button(centered_icon(kind, 12.0, theme::palette().text))
        .width(if cfg!(target_os = "windows") { 50 } else { 40 })
        .height(32)
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
    text(value.into())
        .color(theme::palette().text_muted)
        .size(CAPTION_SIZE)
}

pub fn secondary<'a>(
    label: impl Into<String>,
    message: Message,
) -> iced::widget::Button<'a, Message> {
    button(centered_label(label, BODY_SIZE, theme::palette().text))
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
                icon(kind, ICON_SIZE, theme::palette().text),
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
    button(centered_label(
        label,
        BODY_SIZE,
        theme::palette().accent_text,
    ))
    .height(CONTROL_HEIGHT)
    .padding(0)
    .style(theme::primary_button)
    .on_press(message)
}

pub fn icon_button(kind: Icon, message: Message) -> iced::widget::Button<'static, Message> {
    button(centered_icon(kind, ICON_SIZE, theme::palette().text))
        .width(CONTROL_HEIGHT)
        .height(CONTROL_HEIGHT)
        .padding(0)
        .style(theme::secondary_button)
        .on_press(message)
}

pub struct DropdownOption {
    pub label: String,
    pub selected: bool,
    pub message: Message,
    pub id: Option<&'static str>,
}

impl DropdownOption {
    pub fn new(label: impl Into<String>, selected: bool, message: Message) -> Self {
        Self {
            label: label.into(),
            selected,
            message,
            id: None,
        }
    }

    pub fn id(mut self, id: &'static str) -> Self {
        self.id = Some(id);
        self
    }
}

pub struct DropdownSection {
    pub label: Option<String>,
    pub options: Vec<DropdownOption>,
}

impl DropdownSection {
    pub fn new(label: Option<&str>, options: Vec<DropdownOption>) -> Self {
        Self {
            label: label.map(str::to_owned),
            options,
        }
    }
}

pub fn quality_dropdown_sections(
    language: Language,
    selected: ArdVideoQuality,
) -> Vec<DropdownSection> {
    let option = |quality: ArdVideoQuality, id: &'static str| {
        DropdownOption::new(
            language.tr(quality.label()),
            selected == quality,
            Message::QualityChanged(quality),
        )
        .id(id)
    };

    vec![
        DropdownSection::new(
            Some("Zlib"),
            vec![
                option(ArdVideoQuality::Full, "quality-option-full"),
                option(ArdVideoQuality::Low, "quality-option-low"),
                option(ArdVideoQuality::Medium, "quality-option-medium"),
                option(ArdVideoQuality::High, "quality-option-high"),
            ],
        ),
        DropdownSection::new(
            Some("MVS"),
            vec![option(ArdVideoQuality::Adaptive, "quality-option-adaptive")],
        ),
        DropdownSection::new(
            Some(language.tr("高性能编码")),
            vec![
                option(ArdVideoQuality::HighPerformanceHevc, "quality-option-hevc"),
                option(ArdVideoQuality::HighPerformanceAvc, "quality-option-avc"),
            ],
        ),
    ]
}

pub fn dropdown(
    selected_label: impl Into<String>,
    sections: Vec<DropdownSection>,
    width: impl Into<Length>,
    text_size: f32,
    open: bool,
    on_toggle: Message,
    on_dismiss: Message,
) -> Element<'static, Message> {
    let width = width.into();
    let trigger = button(
        container(
            row![
                text(selected_label.into())
                    .size(text_size)
                    .color(theme::palette().text)
                    .width(Fill),
                icon(
                    if open {
                        Icon::ChevronUp
                    } else {
                        Icon::ChevronDown
                    },
                    12.0,
                    theme::palette().text_muted,
                ),
            ]
            .width(Fill)
            .align_y(Alignment::Center),
        )
        .height(Fill)
        .width(Fill)
        .align_y(Alignment::Center),
    )
    .width(width)
    .height(CONTROL_HEIGHT)
    .padding([0, 12])
    .style(theme::quality_selector_button(open))
    .on_press(on_toggle);

    let mut items = column![].spacing(2);
    for (index, section) in sections.into_iter().enumerate() {
        if let Some(label) = section.label {
            if index > 0 {
                items = items.push(space().height(4));
            }
            items = items.push(
                container(
                    text(label)
                        .size((text_size - 1.0).max(8.0))
                        .color(theme::palette().text_dim),
                )
                .height(18)
                .width(Fill)
                .padding([0, 8])
                .align_y(Alignment::Center),
            );
        }

        for option in section.options {
            let option_button = button(
                container(
                    text(option.label)
                        .size(text_size)
                        .color(theme::palette().text),
                )
                .height(Fill)
                .width(Fill)
                .align_y(Alignment::Center),
            )
            .height(28)
            .width(Fill)
            .padding([0, 8])
            .style(theme::quality_menu_button(option.selected))
            .on_press(option.message);
            let mut item = container(option_button).height(28).width(Fill);
            if let Some(id) = option.id {
                item = item.id(id);
            }
            items = items.push(item);
        }
    }

    let menu = container(items)
        .width(Fill)
        .padding(6)
        .style(theme::context_menu_panel);

    popover(trigger, menu, open, on_dismiss).into()
}

pub fn popover<'a>(
    content: impl Into<Element<'a, Message>>,
    popup: impl Into<Element<'a, Message>>,
    open: bool,
    on_dismiss: Message,
) -> Popover<'a> {
    Popover {
        content: content.into(),
        popup: popup.into(),
        open,
        on_dismiss,
        gap: 4.0,
    }
}

pub struct Popover<'a> {
    content: Element<'a, Message>,
    popup: Element<'a, Message>,
    open: bool,
    on_dismiss: Message,
    gap: f32,
}

impl iced::advanced::Widget<Message, iced::Theme, iced::Renderer> for Popover<'_> {
    fn diff(&mut self, tree: &mut iced::advanced::widget::Tree) {
        tree.diff_children(&mut [self.content.as_widget_mut(), self.popup.as_widget_mut()]);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &iced::Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        event: &Event,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut iced::advanced::Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &iced::advanced::widget::Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> iced::advanced::mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut iced::advanced::widget::Tree,
        layout: iced::advanced::Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<iced::advanced::overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        if !self.open {
            return self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            );
        }

        Some(iced::advanced::overlay::Element::new(Box::new(
            PopoverOverlay {
                popup: &mut self.popup,
                tree: &mut tree.children[1],
                anchor: layout.bounds() + translation,
                viewport: *viewport,
                gap: self.gap,
                on_dismiss: self.on_dismiss.clone(),
            },
        )))
    }
}

impl<'a> From<Popover<'a>> for Element<'a, Message> {
    fn from(popover: Popover<'a>) -> Self {
        Element::new(popover)
    }
}

struct PopoverOverlay<'a, 'b> {
    popup: &'b mut Element<'a, Message>,
    tree: &'b mut iced::advanced::widget::Tree,
    anchor: Rectangle,
    viewport: Rectangle,
    gap: f32,
    on_dismiss: Message,
}

impl iced::advanced::Overlay<Message, iced::Theme, iced::Renderer> for PopoverOverlay<'_, '_> {
    fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> iced::advanced::layout::Node {
        let mut popup = self.popup.as_widget_mut().layout(
            self.tree,
            renderer,
            &iced::advanced::layout::Limits::new(Size::ZERO, bounds)
                .width(Length::Fixed(self.anchor.width)),
        );
        let size = popup.size();
        let margin = 4.0;
        let x = (self.anchor.x + self.anchor.width - size.width)
            .clamp(margin, (bounds.width - size.width - margin).max(margin));
        let below = self.anchor.y + self.anchor.height + self.gap;
        let y = if below + size.height <= bounds.height - margin {
            below
        } else {
            (self.anchor.y - self.gap - size.height).max(margin)
        };
        popup.move_to_mut(Point::new(x, y));
        popup
    }

    fn update(
        &mut self,
        event: &Event,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
        renderer: &iced::Renderer,
        shell: &mut iced::advanced::Shell<'_, Message>,
    ) {
        let pressed = matches!(
            event,
            Event::Mouse(iced::mouse::Event::ButtonPressed(_))
                | Event::Touch(iced::touch::Event::FingerPressed { .. })
        );
        let escape = matches!(
            event,
            Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                ..
            })
        );
        if escape || (pressed && !cursor.is_over(layout.bounds())) {
            shell.publish(self.on_dismiss.clone());
            shell.capture_event();
            return;
        }

        let bounds = layout.bounds();
        self.popup
            .as_widget_mut()
            .update(self.tree, event, layout, cursor, renderer, shell, &bounds);
    }

    fn draw(
        &self,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
    ) {
        let bounds = layout.bounds();
        self.popup
            .as_widget()
            .draw(self.tree, renderer, theme, style, layout, cursor, &bounds);
    }

    fn mouse_interaction(
        &self,
        layout: iced::advanced::Layout<'_>,
        cursor: iced::advanced::mouse::Cursor,
        renderer: &iced::Renderer,
    ) -> iced::advanced::mouse::Interaction {
        let bounds = layout.bounds();
        self.popup
            .as_widget()
            .mouse_interaction(self.tree, layout, cursor, &bounds, renderer)
    }

    fn operate(
        &mut self,
        layout: iced::advanced::Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.popup
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn overlay<'a>(
        &'a mut self,
        layout: iced::advanced::Layout<'a>,
        renderer: &iced::Renderer,
    ) -> Option<iced::advanced::overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        self.popup.as_widget_mut().overlay(
            self.tree,
            layout,
            renderer,
            &self.viewport,
            Vector::ZERO,
        )
    }
}
