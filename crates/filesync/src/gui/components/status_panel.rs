//! Status panel: sync status indicator, progress bar, pause/resume and open folder buttons.

use iced::{
    Alignment, Background, Border, Color, Element, Length,
    widget::{button, column, container, row, text, Space},
};

use crate::gui::app::Message;
use crate::gui::state::{ConnectionStatus, SyncSnapshot};
use crate::gui::theme;

pub fn view(snap: &SyncSnapshot, is_paused: bool) -> Element<'_, Message> {
    let status_label = text(snap.status.label()).size(13).style(theme::secondary);

    let right_detail: Element<Message> = if matches!(snap.status, ConnectionStatus::InitialSync) {
        let transferred = snap.bytes_sent + snap.bytes_received;
        if snap.transfer_total > 0 {
            let pct = (transferred as f32 / snap.transfer_total as f32 * 100.0) as u32;
            text(format!("{} ({pct}%)", fmt_bytes(transferred)))
                .size(12)
                .style(theme::muted)
                .into()
        } else {
            text(format!("{} transferred", fmt_bytes(transferred)))
                .size(12)
                .style(theme::muted)
                .into()
        }
    } else {
        Space::new().width(0).into()
    };

    let pause_label = if is_paused { "▶  Resume" } else { "⏸  Pause" };
    let pause_style: fn(&iced::Theme, button::Status) -> button::Style =
        if is_paused { theme::btn_primary } else { theme::btn_ghost };
    let pause_btn = button(text(pause_label).size(12))
        .on_press(Message::TogglePause)
        .style(pause_style)
        .padding(iced::Padding::from([6, 14]));

    let folder_btn = button(
        row![
            text("📂").size(12),
            Space::new().width(4),
            text("Open Folder").size(12),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::OpenSyncFolder)
    .style(theme::btn_ghost)
    .padding(iced::Padding::from([6, 14]));

    let top_row = row![
        status_indicator_dot(&snap.status),
        Space::new().width(8),
        status_label,
        Space::new().width(Length::Fill),
        right_detail,
        Space::new().width(12),
        folder_btn,
        Space::new().width(8),
        pause_btn,
    ]
    .align_y(Alignment::Center);

    let progress: Element<Message> = if matches!(snap.status, ConnectionStatus::InitialSync) {
        let transferred = snap.bytes_sent + snap.bytes_received;
        let fraction = if snap.transfer_total > 0 {
            (transferred as f32 / snap.transfer_total as f32).clamp(0.0, 1.0)
        } else {
            0.5
        };
        progress_bar(fraction)
    } else {
        Space::new().height(4).into()
    };

    let content = column![top_row, Space::new().height(6), progress].spacing(0);

    container(content)
        .width(Length::Fill)
        .padding(iced::Padding::from([10, 20]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(theme::BG_ELEVATED)),
                ..Default::default()
            }
        })
        .into()
}

/// Small dot indicating the current status category.
fn status_indicator_dot(status: &ConnectionStatus) -> Element<'static, Message> {
    let color = match status {
        ConnectionStatus::Idle => theme::GREEN,
        ConnectionStatus::InitialSync => theme::AMBER,
        ConnectionStatus::Connecting => theme::AMBER,
        ConnectionStatus::Paused => theme::YELLOW,
        ConnectionStatus::AwaitingApproval => theme::YELLOW,
        ConnectionStatus::Error(_) => theme::RED,
        ConnectionStatus::Disconnected => theme::RED,
    };

    container(Space::new().width(0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(color)),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            }
        })
        .into()
}

/// A custom progress bar rendered as two layered containers.
fn progress_bar(fraction: f32) -> Element<'static, Message> {
    let fraction = fraction.clamp(0.0, 1.0);

    let fill = container(Space::new().width(0))
        .width(Length::FillPortion((fraction * 1000.0) as u16))
        .height(Length::Fixed(3.0))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(theme::AMBER)),
                border: Border { radius: 2.0.into(), ..Default::default() },
                ..Default::default()
            }
        });

    let remaining_portions = ((1.0 - fraction) * 1000.0) as u16;
    let remaining: Element<Message> = if remaining_portions > 0 {
        container(Space::new().width(0))
            .width(Length::FillPortion(remaining_portions))
            .height(Length::Fixed(3.0))
            .style(|_| {
                use iced::widget::container;
                container::Style {
                    background: Some(Background::Color(Color { r: 0.25, g: 0.25, b: 0.28, a: 1.0 })),
                    ..Default::default()
                }
            })
            .into()
    } else {
        Space::new().width(0).into()
    };

    let bar = row![fill, remaining]
        .width(Length::Fill)
        .height(Length::Fixed(3.0));

    container(bar)
        .width(Length::Fill)
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color { r: 0.20, g: 0.20, b: 0.23, a: 1.0 })),
                border: Border { radius: 2.0.into(), ..Default::default() },
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
