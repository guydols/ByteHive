//! Conflicts panel — shown alongside stats when conflicts exist.

use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::state::Conflict;
use crate::gui::theme;

pub fn view(conflicts: &[Conflict]) -> Element<'_, Message> {
    let hint = text("Review each conflict and choose which version to keep.")
        .size(11)
        .style(theme::muted);

    let cards: Vec<Element<Message>> = conflicts.iter().map(|c| conflict_card(c)).collect();

    let list = scrollable(column(cards).spacing(10))
        .width(Length::Fill)
        .height(Length::Fill);

    column![hint, Space::new().height(12), list]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
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

    let kind_badge = container(text(conflict.kind.label()).size(10).style(theme::red_text))
        .padding(iced::Padding::from([2, 6]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color {
                    r: 0.94,
                    g: 0.32,
                    b: 0.25,
                    a: 0.12,
                })),
                border: Border {
                    color: Color {
                        r: 0.94,
                        g: 0.32,
                        b: 0.25,
                        a: 0.30,
                    },
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            }
        });

    let header_row = row![filename, Space::new().width(8), kind_badge].align_y(Alignment::Center);

    let path_text = text(truncate_path(&conflict.folder_path, 36))
        .size(11)
        .style(theme::muted);

    let local_row = row![
        text("Local:  ").size(11).style(theme::muted),
        text(conflict.local_modified.clone())
            .size(11)
            .style(theme::secondary),
    ];

    let remote_row = row![
        text("Remote: ").size(11).style(theme::muted),
        text(conflict.remote_modified.clone())
            .size(11)
            .style(theme::secondary),
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

    let keep_local_btn = button(text("Keep Local").size(12))
        .on_press(Message::KeepLocalVersion(id))
        .style(theme::btn_primary)
        .padding(iced::Padding::from([5, 10]));

    let keep_remote_btn = button(text("Keep Remote").size(12))
        .on_press(Message::KeepRemoteVersion(id))
        .style(theme::btn_ghost)
        .padding(iced::Padding::from([5, 10]));

    let dismiss_btn = button(text("\u{2715}").size(11).style(theme::muted))
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

#[cfg(test)]
mod tests {
    use super::{truncate_path, view};
    use crate::gui::state::{Conflict, ConflictKind};

    // ─── truncate_path ────────────────────────────────────────────────────────

    #[test]
    fn truncate_path_short_path_returned_unchanged() {
        let path = "/home/user/sync";
        assert_eq!(truncate_path(path, 36), path);
    }

    #[test]
    fn truncate_path_exactly_max_len_returned_unchanged() {
        // Build a path that is exactly 36 ASCII chars.
        let path = "/home/user/sync/docs/exactly36chars!";
        assert_eq!(
            path.len(),
            36,
            "test precondition: path must be exactly 36 bytes"
        );
        assert_eq!(truncate_path(path, 36), path);
    }

    #[test]
    fn truncate_path_long_path_starts_with_ellipsis() {
        let path = "/very/long/path/that/clearly/exceeds/the/maximum/allowed/length/file.txt";
        let result = truncate_path(path, 36);
        assert!(
            result.starts_with('\u{2026}'),
            "truncated path must start with the ellipsis character"
        );
    }

    #[test]
    fn truncate_path_long_path_char_count_is_max_len_minus_2() {
        // The result is "…" (1 char) + last (max_len - 3) bytes = max_len - 2 chars (for ASCII).
        let path = "/very/long/path/that/clearly/exceeds/the/maximum/allowed/length/file.txt";
        let max_len = 36usize;
        let result = truncate_path(path, max_len);
        let char_count = result.chars().count();
        assert_eq!(
            char_count,
            max_len - 2,
            "truncated result should be {} chars, got {}",
            max_len - 2,
            char_count
        );
    }

    #[test]
    fn truncate_path_long_path_preserves_suffix() {
        let path = "/very/long/path/that/clearly/exceeds/the/maximum/allowed/length/file.txt";
        let max_len = 36usize;
        let result = truncate_path(path, max_len);
        // The last (max_len - 3) bytes of the original path appear at the end of the result.
        let expected_suffix = &path[path.len() - (max_len - 3)..];
        assert!(
            result.ends_with(expected_suffix),
            "truncated path must end with '{}', got '{}'",
            expected_suffix,
            result
        );
    }

    #[test]
    fn truncate_path_one_byte_over_max_len_is_truncated() {
        // path.len() == max_len + 1, so it should be truncated.
        let path = "/home/user/sync/docs/exactly37chars!!";
        assert_eq!(
            path.len(),
            37,
            "test precondition: path must be exactly 37 bytes"
        );
        let result = truncate_path(path, 36);
        assert!(result.starts_with('\u{2026}'));
    }

    // ─── view smoke tests ─────────────────────────────────────────────────────

    fn make_conflict(id: usize, kind: ConflictKind) -> Conflict {
        Conflict {
            id,
            filename: format!("file_{id}.txt"),
            folder_path: "/home/user/sync/docs".into(),
            local_modified: "2024-06-01 10:00".into(),
            remote_modified: "2024-06-01 11:30".into(),
            kind,
        }
    }

    #[test]
    fn view_empty_conflicts_does_not_panic() {
        let _ = view(&[]);
    }

    #[test]
    fn view_single_both_modified_conflict_does_not_panic() {
        let c = make_conflict(1, ConflictKind::BothModified);
        let _ = view(&[c]);
    }

    #[test]
    fn view_single_local_only_conflict_does_not_panic() {
        let c = make_conflict(2, ConflictKind::LocalOnly);
        let _ = view(&[c]);
    }

    #[test]
    fn view_single_remote_only_conflict_does_not_panic() {
        let c = make_conflict(3, ConflictKind::RemoteOnly);
        let _ = view(&[c]);
    }

    #[test]
    fn view_single_both_created_conflict_does_not_panic() {
        let c = make_conflict(4, ConflictKind::BothCreated);
        let _ = view(&[c]);
    }

    #[test]
    fn view_multiple_conflicts_does_not_panic() {
        let conflicts = vec![
            make_conflict(1, ConflictKind::BothModified),
            make_conflict(2, ConflictKind::LocalOnly),
            make_conflict(3, ConflictKind::RemoteOnly),
            make_conflict(4, ConflictKind::BothCreated),
        ];
        let _ = view(&conflicts);
    }

    #[test]
    fn view_conflict_with_long_folder_path_does_not_panic() {
        let c = Conflict {
            id: 99,
            filename: "report.pdf".into(),
            folder_path: "/very/deep/nested/directory/structure/that/exceeds/the/maximum/display/width/by/a/lot".into(),
            local_modified: "2024-01-01".into(),
            remote_modified: "2024-01-02".into(),
            kind: ConflictKind::BothModified,
        };
        let _ = view(&[c]);
    }

    #[test]
    fn view_conflict_with_short_folder_path_does_not_panic() {
        let c = Conflict {
            id: 1,
            filename: "a.txt".into(),
            folder_path: "/s".into(),
            local_modified: "t1".into(),
            remote_modified: "t2".into(),
            kind: ConflictKind::LocalOnly,
        };
        let _ = view(&[c]);
    }
}
