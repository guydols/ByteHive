//! Sync statistics panel — displayed in the Stats tab of the side panel.
//!
//! Shows totals for files, directories, transfer volume, and the last
//! activity timestamp. Each stat is rendered as a labelled card.

use iced::{
    widget::{column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::state::SyncSnapshot;
use crate::gui::theme;

pub fn view(snap: &SyncSnapshot) -> Element<'static, Message> {
    let total_files = format_count(snap.file_count as u64);
    let total_dirs = format_count(snap.dir_count as u64);
    let total_bytes = format_bytes(snap.total_bytes);
    let bytes_sent = format_bytes(snap.bytes_sent);
    let bytes_received = format_bytes(snap.bytes_received);

    let row1 = stat_row(
        stat_card("Files", total_files, theme::AMBER),
        stat_card("Directories", total_dirs, theme::AMBER),
    );
    let row2 = stat_row(
        stat_card("Total Size", total_bytes, theme::TEXT_SECONDARY),
        stat_card("Sent", bytes_sent, theme::GREEN),
    );
    let row3 = stat_row(
        stat_card("Received", bytes_received, theme::TEXT_SECONDARY),
        last_active_card(
            snap.last_connected
                .map(|t| format!("{} s ago", t.elapsed().as_secs()))
                .unwrap_or_else(|| "\u{2014}".into()),
        ),
    );

    column![row1, row2, row3]
        .spacing(10)
        .width(Length::Fill)
        .into()
}

// ─── Individual card ──────────────────────────────────────────────────────────

fn stat_card(label: &'static str, value: String, value_color: Color) -> Element<'static, Message> {
    let value_widget =
        text(value)
            .size(22)
            .style(move |_: &iced::Theme| iced::widget::text::Style {
                color: Some(value_color),
            });

    let label_widget = text(label).size(11).style(theme::muted);

    let inner = column![value_widget, Space::new().height(4), label_widget].spacing(0);

    container(inner)
        .width(Length::Fill)
        .padding(iced::Padding::from([14, 14]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color {
                    r: 0.14,
                    g: 0.14,
                    b: 0.17,
                    a: 1.0,
                })),
                border: Border {
                    color: theme::BORDER,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

fn last_active_card(timestamp: String) -> Element<'static, Message> {
    let value_widget = text(timestamp).size(13).style(theme::secondary);

    let label_widget = text("Last Active").size(11).style(theme::muted);

    let inner = column![value_widget, Space::new().height(4), label_widget].spacing(0);

    container(inner)
        .width(Length::Fill)
        .padding(iced::Padding::from([14, 14]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color {
                    r: 0.14,
                    g: 0.14,
                    b: 0.17,
                    a: 1.0,
                })),
                border: Border {
                    color: theme::BORDER,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

/// Lays two cards side by side with equal width.
fn stat_row(
    left: Element<'static, Message>,
    right: Element<'static, Message>,
) -> Element<'static, Message> {
    row![left, Space::new().width(10), right]
        .align_y(Alignment::Start)
        .width(Length::Fill)
        .into()
}

// ─── Formatting helpers ───────────────────────────────────────────────────────

fn format_count(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
