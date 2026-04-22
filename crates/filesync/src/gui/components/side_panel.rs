//! Side panel with a two-tab bar: Stats and Conflicts.
//!
//! The tab bar sits at the top of the panel. Each tab button is styled as
//! active (amber underline + bright text) or inactive (muted). An amber
//! badge on the Conflicts tab shows the pending conflict count when > 0.

use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::components::{conflicts, stats_panel};
use crate::gui::state::{SideTab, SyncSnapshot};
use crate::gui::theme;

pub fn view<'a>(snap: &'a SyncSnapshot, active_tab: &'a SideTab) -> Element<'a, Message> {
    let tab_bar = build_tab_bar(snap, active_tab);
    let content = match active_tab {
        SideTab::Stats => stats_panel::view(snap),
        SideTab::Conflicts => conflicts::view(&snap.conflicts),
    };

    let panel = column![tab_bar, Space::new().height(14), content,]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

    container(panel)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(theme::panel)
        .into()
}

// ─── Tab bar ─────────────────────────────────────────────────────────────────

fn build_tab_bar<'a>(snap: &'a SyncSnapshot, active_tab: &'a SideTab) -> Element<'a, Message> {
    let conflict_count = snap.conflicts.len();

    let stats_tab = tab_button("Stats", SideTab::Stats, active_tab, 0);
    let conflicts_tab = tab_button("Conflicts", SideTab::Conflicts, active_tab, conflict_count);

    let bar = row![
        stats_tab,
        Space::new().width(4),
        conflicts_tab,
        Space::new().width(Length::Fill),
    ]
    .align_y(Alignment::End)
    .spacing(0);

    column![
        bar,
        // Full-width bottom border acting as the underline rail
        container(Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(|_| {
                use iced::widget::container;
                container::Style {
                    background: Some(Background::Color(theme::BORDER)),
                    ..Default::default()
                }
            }),
    ]
    .spacing(0)
    .into()
}

/// A single tab button. Active tabs get an amber bottom bar and full-bright
/// text; inactive tabs are muted with a subtle hover.
fn tab_button<'a>(
    label: &'a str,
    tab: SideTab,
    active: &SideTab,
    badge_count: usize,
) -> Element<'a, Message> {
    let is_active = &tab == active;

    let label_color = if is_active {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };

    let label_widget =
        text(label)
            .size(13)
            .style(move |_: &iced::Theme| iced::widget::text::Style {
                color: Some(label_color),
            });

    // Optional badge showing conflict count.
    let badge: Element<Message> = if badge_count > 0 {
        let badge_color = if is_active {
            theme::AMBER
        } else {
            Color {
                r: 0.80,
                g: 0.55,
                b: 0.04,
                a: 0.85,
            }
        };
        let bg_color = if is_active {
            Color {
                r: 1.0,
                g: 0.67,
                b: 0.055,
                a: 0.18,
            }
        } else {
            Color {
                r: 1.0,
                g: 0.67,
                b: 0.055,
                a: 0.10,
            }
        };
        container(
            text(badge_count.to_string())
                .size(10)
                .style(move |_: &iced::Theme| iced::widget::text::Style {
                    color: Some(badge_color),
                }),
        )
        .padding(iced::Padding::from([1, 5]))
        .style(move |_| {
            use iced::widget::container;
            container::Style {
                background: Some(Background::Color(bg_color)),
                border: Border {
                    color: Color {
                        r: 1.0,
                        g: 0.67,
                        b: 0.055,
                        a: 0.30,
                    },
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
    } else {
        Space::new().width(0).into()
    };

    let inner = row![label_widget, Space::new().width(5), badge]
        .align_y(Alignment::Center)
        .spacing(0);

    // The amber underline for the active tab
    let underline: Element<Message> = if is_active {
        container(Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(2.0))
            .style(|_| {
                use iced::widget::container;
                container::Style {
                    background: Some(Background::Color(theme::AMBER)),
                    border: Border {
                        radius: 2.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            })
            .into()
    } else {
        container(Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(2.0))
            .style(|_| iced::widget::container::Style::default())
            .into()
    };

    let btn_content = column![
        container(inner).padding(iced::Padding::from([6, 10])),
        underline,
    ]
    .spacing(0)
    .align_x(iced::alignment::Horizontal::Center);

    button(btn_content)
        .on_press(Message::SelectTab(tab))
        .style(move |_, status| {
            use iced::widget::button;
            let bg = match (is_active, status) {
                (false, button::Status::Hovered | button::Status::Pressed) => Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.04,
                },
                _ => Color::TRANSPARENT,
            };
            button::Style {
                background: Some(Background::Color(bg)),
                text_color: theme::TEXT_PRIMARY,
                border: Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        })
        .padding(0)
        .into()
}

#[cfg(test)]
mod tests {
    use crate::gui::state::{Conflict, ConflictKind, SideTab, SyncSnapshot};

    // ─── view smoke tests ─────────────────────────────────────────────────────

    #[test]
    fn view_stats_tab_default_snapshot_does_not_panic() {
        let _ = super::view(&SyncSnapshot::default(), &SideTab::Stats);
    }

    #[test]
    fn view_conflicts_tab_no_conflicts_does_not_panic() {
        let _ = super::view(&SyncSnapshot::default(), &SideTab::Conflicts);
    }

    #[test]
    fn view_conflicts_tab_with_one_conflict_does_not_panic() {
        let mut snap = SyncSnapshot::default();
        snap.conflicts.push(Conflict {
            id: 1,
            filename: "document.txt".into(),
            folder_path: "/sync/docs".into(),
            local_modified: "2024-01-01".into(),
            remote_modified: "2024-01-02".into(),
            kind: ConflictKind::BothModified,
        });
        let _ = super::view(&snap, &SideTab::Conflicts);
    }

    #[test]
    fn view_conflicts_tab_with_many_conflicts_does_not_panic() {
        let mut snap = SyncSnapshot::default();
        let kinds = [
            ConflictKind::BothModified,
            ConflictKind::LocalOnly,
            ConflictKind::RemoteOnly,
            ConflictKind::BothCreated,
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            snap.conflicts.push(Conflict {
                id: i,
                filename: format!("file_{i}.txt"),
                folder_path: "/sync".into(),
                local_modified: "t1".into(),
                remote_modified: "t2".into(),
                kind,
            });
        }
        let _ = super::view(&snap, &SideTab::Conflicts);
    }

    #[test]
    fn view_stats_tab_badge_visible_when_conflicts_present_does_not_panic() {
        // Viewing Stats tab while conflicts exist — the badge count should render without panic.
        let mut snap = SyncSnapshot::default();
        snap.conflicts.push(Conflict {
            id: 1,
            filename: "x.txt".into(),
            folder_path: "/sync".into(),
            local_modified: "t1".into(),
            remote_modified: "t2".into(),
            kind: ConflictKind::BothModified,
        });
        let _ = super::view(&snap, &SideTab::Stats);
    }

    #[test]
    fn view_stats_tab_with_populated_snapshot_does_not_panic() {
        let mut snap = SyncSnapshot::default();
        snap.file_count = 1_000;
        snap.dir_count = 50;
        snap.total_bytes = 2_147_483_648;
        snap.bytes_sent = 1_048_576;
        snap.bytes_received = 524_288;
        let _ = super::view(&snap, &SideTab::Stats);
    }
}
