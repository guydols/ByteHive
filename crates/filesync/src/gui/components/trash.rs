use std::collections::HashMap;

use iced::{
    widget::{button, column, container, row, scrollable, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::state::TrashItem;
use crate::gui::theme;

pub fn view<'a>(
    trash: &'a [TrashItem],
    dest_inputs: &'a HashMap<String, String>,
    error: Option<&'a str>,
    info: Option<&'a str>,
) -> Element<'a, Message> {
    let hint = text("Deleted files wait here. Restore them or delete them forever.")
        .size(11)
        .style(theme::muted);

    let refresh_btn = button(text("Refresh").size(12))
        .on_press(Message::RefreshTrash)
        .style(theme::btn_ghost)
        .padding(iced::Padding::from([5, 10]));

    let empty_btn = button(text("Empty trash").size(12))
        .on_press(Message::EmptyTrash)
        .style(theme::btn_ghost)
        .padding(iced::Padding::from([5, 10]));

    let purge_expired_btn = button(text("Remove expired").size(12))
        .on_press(Message::PurgeExpiredTrash)
        .style(theme::btn_ghost)
        .padding(iced::Padding::from([5, 10]));

    let top_row = row![refresh_btn, Space::new().width(6), empty_btn, Space::new().width(6), purge_expired_btn]
        .align_y(Alignment::Center);

    let mut content = column![
        hint,
        Space::new().height(10),
        top_row,
    ]
    .spacing(0);

    if let Some(e) = error {
        content = content
            .push(Space::new().height(8))
            .push(text(e).size(12).style(theme::red_text));
    }

    if let Some(i) = info {
        content = content
            .push(Space::new().height(8))
            .push(text(i).size(12).style(theme::green_text));
    }

    if trash.is_empty() {
        content = content.push(Space::new().height(12)).push(
            container(text("Trash is empty.").size(13).style(theme::secondary))
                .width(Length::Fill)
                .padding(16)
                .style(theme::panel),
        );
    } else {
        let cards: Vec<Element<Message>> =
            trash.iter().map(|item| trash_card(item, dest_inputs)).collect();
        let list = scrollable(column(cards).spacing(10))
            .width(Length::Fill)
            .height(Length::Fill);
        content = content
            .push(Space::new().height(10))
            .push(text(format!("{} item(s) in trash", trash.len())).size(11).style(theme::muted))
            .push(Space::new().height(6))
            .push(list);
    }

    content
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn trash_card<'a>(
    item: &'a TrashItem,
    dest_inputs: &'a HashMap<String, String>,
) -> Element<'a, Message> {
    let id = item.id.clone();

    let name = item
        .original_path
        .rsplit('/')
        .next()
        .unwrap_or(&item.original_path)
        .to_string();
    let filename = text(name).size(13).style(|_: &iced::Theme| {
        iced::widget::text::Style {
            color: Some(theme::TEXT_PRIMARY),
        }
    });

    let kind_label = if item.is_dir { "Folder" } else { "File" };
    let kind_badge = container(text(kind_label).size(10).style(theme::secondary))
        .padding(iced::Padding::from([2, 6]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(Color {
                    r: 1.0,
                    g: 0.67,
                    b: 0.055,
                    a: 0.12,
                })),
                border: Border {
                    color: Color {
                        r: 1.0,
                        g: 0.67,
                        b: 0.055,
                        a: 0.30,
                    },
                    width: 1.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            }
        });

    let header_row = row![filename, Space::new().width(8), kind_badge].align_y(Alignment::Center);

    let path_text = text(truncate_path(&item.original_path, 44))
        .size(11)
        .style(theme::muted);

    let meta_row = row![
        text(format!("{}  •  {}", format_size(item.size), format_deleted_at(item.deleted_at_ms)))
            .size(11)
            .style(theme::secondary),
    ];

    let dest_value = dest_inputs
        .get(&item.id)
        .cloned()
        .unwrap_or_else(|| item.original_path.clone());

    let dest_field = text_input("Restore to this place…", &dest_value)
        .on_input(move |v| Message::TrashDestInput {
            id: id.clone(),
            value: v,
        })
        .padding(8)
        .size(12);

    let restore_id = item.id.clone();
    let dest_for_msg: Option<std::path::PathBuf> = {
        let trimmed = dest_value.trim();
        if trimmed.is_empty() || trimmed == item.original_path {
            None
        } else {
            Some(std::path::PathBuf::from(trimmed))
        }
    };
    let restore_btn = button(text("Restore").size(12))
        .on_press(Message::RestoreTrash {
            id: restore_id,
            dest: dest_for_msg,
        })
        .style(theme::btn_primary)
        .padding(iced::Padding::from([5, 10]));

    let pick_id = item.id.clone();
    let choose_btn = button(text("Choose place…").size(12))
        .on_press(Message::PickRestoreDest(pick_id))
        .style(theme::btn_ghost)
        .padding(iced::Padding::from([5, 10]));

    let purge_id = item.id.clone();
    let delete_btn = button(text("Delete forever").size(12))
        .on_press(Message::PurgeTrash(purge_id))
        .style(theme::btn_danger)
        .padding(iced::Padding::from([5, 10]));

    let action_row = row![
        restore_btn,
        Space::new().width(6),
        choose_btn,
        Space::new().width(6),
        delete_btn,
    ]
    .align_y(Alignment::Center);

    let note = text("Deleting forever also clears related sync history.")
        .size(10)
        .style(theme::muted);

    let card_inner = column![
        header_row,
        Space::new().height(2),
        path_text,
        Space::new().height(4),
        meta_row,
        Space::new().height(8),
        dest_field,
        Space::new().height(8),
        action_row,
        Space::new().height(4),
        note,
    ]
    .spacing(0);

    container(card_inner)
        .width(Length::Fill)
        .padding(12)
        .style(theme::panel)
        .into()
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let trimmed = &path[path.len() - (max_len - 3)..];
        format!("\u{2026}{trimmed}")
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_deleted_at(deleted_at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(deleted_at_ms);
    let age_ms = now_ms.saturating_sub(deleted_at_ms);
    let secs = age_ms / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_deleted_at, format_size, truncate_path, view};
    use crate::gui::state::TrashItem;
    use std::collections::HashMap;

    fn make_item(id: &str) -> TrashItem {
        TrashItem {
            id: id.into(),
            original_path: format!("docs/{id}.txt"),
            size: 2048,
            deleted_at_ms: 0,
            is_dir: false,
        }
    }

    #[test]
    fn view_empty_trash_does_not_panic() {
        let dests = HashMap::new();
        let _ = view(&[], &dests, None, None);
    }

    #[test]
    fn view_single_item_does_not_panic() {
        let items = vec![make_item("abc")];
        let dests = HashMap::new();
        let _ = view(&items, &dests, None, None);
    }

    #[test]
    fn view_with_error_and_info_does_not_panic() {
        let items = vec![make_item("abc")];
        let dests = HashMap::new();
        let _ = view(
            &items,
            &dests,
            Some("That place already has a file. Pick a different place."),
            Some("Restored."),
        );
    }

    #[test]
    fn view_with_custom_dest_does_not_panic() {
        let items = vec![make_item("abc")];
        let mut dests = HashMap::new();
        dests.insert("abc".to_string(), "restored/copy.txt".to_string());
        let _ = view(&items, &dests, None, None);
    }

    #[test]
    fn truncate_path_short_unchanged() {
        assert_eq!(truncate_path("a.txt", 44), "a.txt");
    }

    #[test]
    fn truncate_path_long_starts_with_ellipsis() {
        let long = "a/very/long/path/that/definitely/exceeds/the/limit/file.txt";
        let out = truncate_path(long, 20);
        assert!(out.starts_with('\u{2026}'));
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn format_deleted_at_epoch_is_days_ago() {
        let out = format_deleted_at(0);
        assert!(out.ends_with("ago") || out == "just now");
    }
}
