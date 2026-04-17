//! Collapsible log panel.
//!
//! Collapsed: shows only a clickable toggle bar with the last log message.
//! Expanded: shows a fixed-height scrollable list of all log entries.

use iced::{
    Alignment, Background, Border, Color, Element, Length,
    widget::{button, column, container, row, scrollable, text, Space},
};

use crate::gui::app::Message;
use crate::gui::state::EventLog;
use crate::gui::theme;

/// Height of the log panel when expanded, in logical pixels.
const EXPANDED_HEIGHT: f32 = 180.0;
/// Height of the collapsed toggle bar.
const COLLAPSED_HEIGHT: f32 = 34.0;

pub fn view(log: &EventLog, expanded: bool) -> Element<'_, Message> {
    let toggle_bar = toggle_bar(log, expanded);

    if !expanded {
        return toggle_bar;
    }

    let entries: Vec<Element<Message>> = log
        .entries()
        .iter()
        .rev()
        .map(|line| {
            text(line.clone())
                .size(12)
                .style(theme::secondary)
                .into()
        })
        .collect();

    let log_list = scrollable(
        column(entries).spacing(4).padding(iced::Padding::from([4, 8])),
    )
    .width(Length::Fill)
    .height(Length::Fixed(EXPANDED_HEIGHT));

    let panel = column![toggle_bar, log_list].spacing(0);

    container(panel)
        .width(Length::Fill)
        .style(theme::log_panel)
        .into()
}

/// The clickable header bar that collapses/expands the panel.
fn toggle_bar(log: &EventLog, expanded: bool) -> Element<'_, Message> {
    let arrow = if expanded { "\u{25BE}" } else { "\u{25B8}" };
    let entry_count = log.entries().len();

    let label = text(format!("{arrow}  Logs  ({} entries)", entry_count))
        .size(12)
        .style(theme::muted);

    // Show the last log line as a preview when collapsed.
    let preview: Element<Message> = if !expanded {
        if let Some(last) = log.entries().last() {
            text(last.clone())
                .size(11)
                .style(theme::muted)
                .into()
        } else {
            Space::new().width(0).into()
        }
    } else {
        Space::new().width(0).into()
    };

    let inner = row![
        Space::new().width(12),
        label,
        Space::new().width(20),
        preview,
        Space::new().width(Length::Fill),
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    button(inner)
        .on_press(Message::ToggleLogPanel)
        .width(Length::Fill)
        .height(Length::Fixed(COLLAPSED_HEIGHT))
        .style(|_, status| {
            use iced::widget::button;
            let bg = match status {
                button::Status::Hovered | button::Status::Pressed =>
                    Color { r: 0.15, g: 0.15, b: 0.18, a: 1.0 },
                _ =>
                    Color { r: 0.08, g: 0.08, b: 0.10, a: 1.0 },
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: theme::TEXT_MUTED,
                border: Border {
                    color: theme::BORDER,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }
        })
        .padding(0)
        .into()
}
