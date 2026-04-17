//! Top header bar: ByteHive logo, application title, and connection badge.

use iced::{
    widget::{container, row, svg, text, Space},
    Alignment, Element, Length,
};

use crate::gui::app::Message;
use crate::gui::state::ConnectionStatus;
use crate::gui::theme;

pub fn view(status: &ConnectionStatus) -> Element<'_, Message> {
    let logo = svg(svg::Handle::from_memory(
        include_bytes!("../../../../core/assets/bytehive-logo-full.svg").to_vec(),
    ))
    .width(Length::Fixed(160.0))
    .height(Length::Fixed(32.0));

    let app_label = text("FileSync").size(18).style(theme::secondary);

    let divider = container(Space::new().width(1))
        .width(Length::Fixed(1.0))
        .height(Length::Fixed(24.0))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(iced::Background::Color(theme::BORDER)),
                ..Default::default()
            }
        });

    let badge = connection_badge(status);

    let left = row![
        logo,
        Space::new().width(12),
        divider,
        Space::new().width(12),
        app_label
    ]
    .align_y(Alignment::Center)
    .spacing(0);

    let header_row = row![left, Space::new().width(Length::Fill), badge]
        .align_y(Alignment::Center)
        .padding(iced::Padding::from([0, 20]));

    container(header_row)
        .width(Length::Fill)
        .height(Length::Fixed(56.0))
        .center_y(Length::Fixed(56.0))
        .style(|_| {
            use iced::widget::container;
            container::Style {
                background: Some(iced::Background::Color(theme::BG_ELEVATED)),
                ..Default::default()
            }
        })
        .into()
}

/// Small status dot + text label indicating connection status.
fn connection_badge(status: &ConnectionStatus) -> Element<'_, Message> {
    let (dot_color, label_style): (iced::Color, fn(&iced::Theme) -> iced::widget::text::Style) =
        match status {
            ConnectionStatus::Idle => (theme::GREEN, theme::green_text),
            ConnectionStatus::InitialSync => (theme::AMBER, theme::amber_text),
            ConnectionStatus::Connecting => (theme::YELLOW, theme::yellow_text),
            ConnectionStatus::AwaitingApproval => (theme::YELLOW, theme::yellow_text),
            ConnectionStatus::Paused => (theme::YELLOW, theme::yellow_text),
            ConnectionStatus::Disconnected => (theme::RED, theme::red_text),
            ConnectionStatus::Error(_) => (theme::RED, theme::red_text),
        };

    let dot = container(Space::new().width(0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_| {
            use iced::widget::container;
            container::Style {
                background: Some(iced::Background::Color(dot_color)),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        });

    let label = text(status.label()).size(13).style(label_style);

    row![dot, label]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
}
