//! Sync-folder tree view.
//!
//! Renders a flat list of [`FlatNode`]s derived from the recursive tree,
//! with indentation, expand/collapse arrows, and per-node inclusion checkboxes.

use iced::{
    widget::{button, checkbox, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::state::{flatten_tree, FileNode, FlatNode};
use crate::gui::theme;

pub fn view(file_tree: &[FileNode]) -> Element<'_, Message> {
    let heading = text("Sync Folder").size(13).style(theme::muted);

    let flat = flatten_tree(file_tree);

    let rows: Vec<Element<Message>> = flat.iter().map(|node| tree_row(node)).collect();

    let tree_content = column(rows).spacing(2);

    let scrollable_tree = scrollable(tree_content)
        .width(Length::Fill)
        .height(Length::Fill);

    let inner = column![heading, Space::new().height(10), scrollable_tree,].spacing(0);

    container(inner)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(theme::panel)
        .into()
}

/// Renders a single row in the tree.
fn tree_row<'a>(node: &FlatNode) -> Element<'a, Message> {
    let indent = Space::new().width(Length::Fixed((node.depth as f32) * 20.0));

    // Expand/collapse arrow for directories, fixed-width spacer for files.
    let toggle: Element<Message> = if node.is_dir {
        let arrow = if node.expanded {
            "\u{25BE}"
        } else {
            "\u{25B8}"
        };
        let node_id = node.id;
        button(text(arrow).size(12).style(theme::secondary))
            .on_press(Message::ToggleNodeExpanded(node_id))
            .style(theme::btn_flat)
            .padding(iced::Padding::from([2, 4]))
            .into()
    } else {
        Space::new().width(Length::Fixed(20.0)).into()
    };

    // Icon: folder or file (emoji, works without nerd fonts).
    let icon_char = if node.is_dir {
        "\u{1F4C1} "
    } else {
        "\u{1F4C4} "
    };
    let icon_color = if node.is_dir {
        theme::AMBER
    } else {
        theme::TEXT_SECONDARY
    };

    let icon = text(icon_char)
        .size(13)
        .style(move |_: &iced::Theme| iced::widget::text::Style {
            color: Some(icon_color),
        });

    // Node label — greyed out when excluded.
    let label_color = if node.included {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_MUTED
    };
    let label = text(node.name.clone())
        .size(13)
        .style(move |_: &iced::Theme| iced::widget::text::Style {
            color: Some(label_color),
        });

    // Excluded badge.
    let excluded_badge: Element<Message> = if !node.included {
        container(text("excluded").size(10).style(theme::muted))
            .padding(iced::Padding::from([2, 6]))
            .style(|_| {
                use iced::widget::container;
                container::Style {
                    background: Some(Background::Color(Color {
                        r: 0.6,
                        g: 0.2,
                        b: 0.2,
                        a: 0.25,
                    })),
                    border: Border {
                        color: Color {
                            r: 0.6,
                            g: 0.2,
                            b: 0.2,
                            a: 0.4,
                        },
                        width: 1.0,
                        radius: 3.0.into(),
                    },
                    ..Default::default()
                }
            })
            .into()
    } else {
        Space::new().width(0).into()
    };

    let node_id = node.id;
    let included = node.included;

    // Inclusion checkbox.
    let chk: Element<Message> = checkbox(included)
        .on_toggle(move |_| Message::ToggleNodeIncluded(node_id))
        .size(14)
        .into();

    let row_inner = row![
        indent,
        toggle,
        Space::new().width(4),
        icon,
        label,
        Space::new().width(6),
        excluded_badge,
        Space::new().width(Length::Fill),
        chk,
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    container(row_inner)
        .width(Length::Fill)
        .padding(iced::Padding::from([4, 6]))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                border: Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        })
        .into()
}
