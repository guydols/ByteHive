//! ByteHive brand colours and iced style helpers.

use iced::{
    Background, Border, Color, Theme,
    theme::Palette,
    widget::{button, container, text},
};

// Brand palette
pub const AMBER: Color = Color { r: 1.00, g: 0.67, b: 0.055, a: 1.0 };
pub const AMBER_LIGHT: Color = Color { r: 1.00, g: 0.81, b: 0.0, a: 1.0 };
pub const AMBER_DARK: Color = Color { r: 1.00, g: 0.60, b: 0.082, a: 1.0 };

// Surface palette
pub const BG_PRIMARY: Color   = Color { r: 0.09, g: 0.09, b: 0.11, a: 1.0 };
pub const BG_SURFACE: Color   = Color { r: 0.12, g: 0.12, b: 0.15, a: 1.0 };
pub const BG_ELEVATED: Color  = Color { r: 0.10, g: 0.10, b: 0.13, a: 1.0 };
pub const BORDER: Color        = Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 };

// Text palette
pub const TEXT_PRIMARY:   Color = Color { r: 0.93, g: 0.93, b: 0.95, a: 1.0 };
pub const TEXT_SECONDARY: Color = Color { r: 0.60, g: 0.60, b: 0.65, a: 1.0 };
pub const TEXT_MUTED:     Color = Color { r: 0.38, g: 0.38, b: 0.42, a: 1.0 };

// Semantic colours
pub const GREEN:  Color = Color { r: 0.27, g: 0.75, b: 0.43, a: 1.0 };
pub const RED:    Color = Color { r: 0.94, g: 0.32, b: 0.25, a: 1.0 };
pub const YELLOW: Color = Color { r: 0.97, g: 0.74, b: 0.14, a: 1.0 };

pub fn bytehive_theme() -> Theme {
    Theme::custom(
        "ByteHive".to_string(),
        Palette {
            background: BG_PRIMARY,
            text:       TEXT_PRIMARY,
            primary:    AMBER,
            success:    GREEN,
            danger:     RED,
            warning:    YELLOW,
        },
    )
}

// Container styles

pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn elevated_surface(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_ELEVATED)),
        border: Border {
            color: BORDER,
            width: 0.0,
            radius: 0.0.into(),
        },
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn amber_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { r: 1.0, g: 0.67, b: 0.055, a: 0.12 })),
        border: Border {
            color: Color { r: 1.0, g: 0.67, b: 0.055, a: 0.35 },
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn conflict_item_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { r: 0.94, g: 0.32, b: 0.25, a: 0.08 })),
        border: Border {
            color: Color { r: 0.94, g: 0.32, b: 0.25, a: 0.25 },
            width: 1.0,
            radius: 6.0.into(),
        },
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn transparent(_theme: &Theme) -> container::Style {
    container::Style {
        background: None,
        border: Border::default(),
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn log_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { r: 0.06, g: 0.06, b: 0.08, a: 1.0 })),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(TEXT_SECONDARY),
        ..Default::default()
    }
}

// Button styles

pub fn btn_primary(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered  => AMBER_LIGHT,
        button::Status::Pressed  => AMBER_DARK,
        button::Status::Disabled => Color { r: 0.5, g: 0.4, b: 0.1, a: 0.5 },
        _                        => AMBER,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color { r: 0.08, g: 0.06, b: 0.02, a: 1.0 },
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn btn_ghost(_theme: &Theme, status: button::Status) -> button::Style {
    let (bg, border_color) = match status {
        button::Status::Hovered | button::Status::Pressed =>
            (Color { r: 0.22, g: 0.22, b: 0.26, a: 0.5 }, BORDER),
        _ =>
            (Color::TRANSPARENT, BORDER),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: TEXT_SECONDARY,
        border: Border { color: border_color, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    }
}

pub fn btn_danger(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed =>
            Color { r: 0.75, g: 0.22, b: 0.17, a: 1.0 },
        _ => RED,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::WHITE,
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub fn btn_flat(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered  => Color { r: 1.0, g: 1.0, b: 1.0, a: 0.04 },
        button::Status::Pressed  => Color { r: 1.0, g: 1.0, b: 1.0, a: 0.08 },
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: TEXT_PRIMARY,
        border: Border { radius: 4.0.into(), ..Default::default() },
        ..Default::default()
    }
}

// Text styles

pub fn muted(_theme: &Theme) -> text::Style {
    text::Style { color: Some(TEXT_MUTED) }
}

pub fn secondary(_theme: &Theme) -> text::Style {
    text::Style { color: Some(TEXT_SECONDARY) }
}

pub fn amber_text(_theme: &Theme) -> text::Style {
    text::Style { color: Some(AMBER) }
}

pub fn green_text(_theme: &Theme) -> text::Style {
    text::Style { color: Some(GREEN) }
}

pub fn red_text(_theme: &Theme) -> text::Style {
    text::Style { color: Some(RED) }
}

pub fn yellow_text(_theme: &Theme) -> text::Style {
    text::Style { color: Some(YELLOW) }
}
