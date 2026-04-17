//! Statistics panel — shown in the main content area when there are no conflicts.
//! Displays file/dir counts, transfer stats, and config info.

use iced::{
    Alignment, Background, Border, Element, Length,
    widget::{column, container, row, text, Space},
};

use crate::gui::app::Message;
use crate::gui::state::SyncSnapshot;
use crate::gui::theme;

pub fn view(snap: &SyncSnapshot, server_addr: String, sync_root: String) -> Element<'static, Message> {
    let heading = text("Sync Statistics")
        .size(13)
        .style(theme::muted);

    let stats_row = row![
        stat_card("Files", snap.file_count.to_string()),
        Space::new().width(12),
        stat_card("Dirs", snap.dir_count.to_string()),
        Space::new().width(12),
        stat_card("Total size", fmt_bytes(snap.total_bytes)),
    ]
    .width(Length::Fill);

    let transfer_row = row![
        stat_card("Sent", fmt_bytes(snap.bytes_sent)),
        Space::new().width(12),
        stat_card("Received", fmt_bytes(snap.bytes_received)),
        Space::new().width(12),
        stat_card(
            "Last active",
            snap.last_connected
                .map(|t| format!("{} s ago", t.elapsed().as_secs()))
                .unwrap_or_else(|| "\u{2014}".into()),
        ),
    ]
    .width(Length::Fill);

    let config_row = row![
        text(server_addr)
            .size(13)
            .style(theme::secondary),
        Space::new().width(8),
        text("\u{2192}").size(13).style(theme::muted),
        Space::new().width(8),
        text(sync_root)
            .size(13)
            .style(theme::amber_text),
    ]
    .align_y(Alignment::Center);

    let config_container = container(config_row)
        .width(Length::Fill)
        .padding([10, 16])
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(theme::BG_SURFACE)),
                border: Border {
                    color: theme::BORDER,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        });

    let inner = column![
        heading,
        Space::new().height(12),
        stats_row,
        Space::new().height(12),
        transfer_row,
        Space::new().height(16),
        config_container,
    ]
    .spacing(0);

    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(theme::panel)
        .into()
}

fn stat_card(label_str: &'static str, value_str: String) -> Element<'static, Message> {
    container(
        column![
            text(value_str)
                .size(20)
                .style(|_: &iced::Theme| iced::widget::text::Style {
                    color: Some(theme::TEXT_PRIMARY),
                }),
            Space::new().height(2),
            text(label_str)
                .size(12)
                .style(theme::muted),
        ]
        .spacing(0),
    )
    .width(Length::Fill)
    .padding([14, 16])
    .style(|_| {
        use iced::widget::container;
        container::Style {
            background: Some(Background::Color(theme::BG_SURFACE)),
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

fn fmt_bytes(b: u64) -> String {
    match b {
        0..1_024 => format!("{b} B"),
        1_024..1_048_576 => format!("{:.1} KiB", b as f64 / 1_024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GiB", b as f64 / 1_073_741_824.0),
    }
}
