//! Conflicts panel — shown alongside stats when conflicts exist.

use iced::{
    Alignment, Background, Border, Color, Element, Length,
    widget::{button, column, container, row, scrollable, text, Space},
};

use crate::gui::app::Message;
use crate::gui::state::Conflict;
use crate::gui::theme;

pub fn view(conflicts: &[Conflict]) -> Element<'_, Message> {
    let count = conflicts.len();

    let heading_row = row![
        text("\u{26A0}  Conflicts")
            .size(13)
            .style(theme::amber_text),
        Space::new().width(8),
        container(
            text(count.to_string()).size(11).style(theme::amber_text),
        )
        .padding(iced::Padding::from([2, 6]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color {
                    r: 1.0, g: 0.67, b: 0.055, a: 0.18,
                })),
                border: Border {
                    color: Color { r: 1.0, g: 0.67, b: 0.055, a: 0.45 },
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..Default::default()
            }
        }),
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    let hint = text("Review each conflict and choose which version to keep.")
        .size(11)
        .style(theme::muted);

    let cards: Vec<Element<Message>> = conflicts
        .iter()
        .map(|c| conflict_card(c))
        .collect();

    let list = scrollable(column(cards).spacing(10))
        .width(Length::Fill)
        .height(Length::Fill);

    let inner = column![heading_row, Space::new().height(4), hint, Space::new().height(12), list]
        .spacing(0);

    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(theme::panel)
        .into()
}

/// A single conflict resolution card.
fn conflict_card(conflict: &Conflict) -> Element<'_, Message> {
    let id = conflict.id;

    let filename = text(conflict.filename.clone())
        .size(13)
        .style(|_: &iced::Theme| iced::widget::text::Style {
            color: Some(theme::TEXT_PRIMARY),
        });

    let kind_badge = container(
        text(conflict.kind.label())
            .size(10)
            .style(theme::red_text),
    )
    .padding(iced::Padding::from([2, 6]))
    .style(|_| {
        use iced::widget::container;
        container::Style {
            background: Some(Background::Color(Color { r: 0.94, g: 0.32, b: 0.25, a: 0.12 })),
            border: Border {
                color: Color { r: 0.94, g: 0.32, b: 0.25, a: 0.30 },
                width: 1.0,
                radius: 3.0.into(),
            },
            ..Default::default()
        }
    });

    let header_row = row![filename, Space::new().width(8), kind_badge]
        .align_y(Alignment::Center);

    let path_text = text(truncate_path(&conflict.folder_path, 36))
        .size(11)
        .style(theme::muted);

    let local_row = row![
        text("Local:  ").size(11).style(theme::muted),
        text(conflict.local_modified.clone()).size(11).style(theme::secondary),
    ];

    let remote_row = row![
        text("Remote: ").size(11).style(theme::muted),
        text(conflict.remote_modified.clone()).size(11).style(theme::secondary),
    ];

    let open_btn = button(
        row![
            text("\u{1F4C2}").size(12),
            Space::new().width(4),
            text("Open Folder").size(12),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::OpenConflictFolder(id))
    .style(theme::btn_ghost)
    .padding(iced::Padding::from([5, 10]));

    let keep_local_btn = button(
        text("Keep Local").size(12),
    )
    .on_press(Message::KeepLocalVersion(id))
    .style(theme::btn_primary)
    .padding(iced::Padding::from([5, 10]));

    let keep_remote_btn = button(
        text("Keep Remote").size(12),
    )
    .on_press(Message::KeepRemoteVersion(id))
    .style(theme::btn_ghost)
    .padding(iced::Padding::from([5, 10]));

    let dismiss_btn = button(
        text("\u{2715}").size(11).style(theme::muted),
    )
    .on_press(Message::DismissConflict(id))
    .style(theme::btn_flat)
    .padding(iced::Padding::from([4, 6]));

    let action_row = row![
        open_btn,
        Space::new().width(6),
        keep_local_btn,
        Space::new().width(6),
        keep_remote_btn,
        Space::new().width(Length::Fill),
        dismiss_btn,
    ]
    .align_y(Alignment::Center);

    let card_inner = column![
        header_row,
        Space::new().height(2),
        path_text,
        Space::new().height(6),
        local_row,
        remote_row,
        Space::new().height(10),
        action_row,
    ]
    .spacing(0);

    container(card_inner)
        .width(Length::Fill)
        .padding(12)
        .style(theme::conflict_item_panel)
        .into()
}

/// Shortens a path so it fits inside the conflict card.
fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let trimmed = &path[path.len() - (max_len - 3)..];
        format!("\u{2026}{trimmed}")
    }
}
