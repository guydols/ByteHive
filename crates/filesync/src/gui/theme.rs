//! ByteHive brand colours and iced style helpers.

use iced::{
    theme::Palette,
    widget::{button, container, text},
    Background, Border, Color, Theme,
};

// Brand palette
pub const AMBER: Color = Color {
    r: 1.00,
    g: 0.67,
    b: 0.055,
    a: 1.0,
};
pub const AMBER_LIGHT: Color = Color {
    r: 1.00,
    g: 0.81,
    b: 0.0,
    a: 1.0,
};
pub const AMBER_DARK: Color = Color {
    r: 1.00,
    g: 0.60,
    b: 0.082,
    a: 1.0,
};

// Surface palette
pub const BG_PRIMARY: Color = Color {
    r: 0.09,
    g: 0.09,
    b: 0.11,
    a: 1.0,
};
pub const BG_SURFACE: Color = Color {
    r: 0.12,
    g: 0.12,
    b: 0.15,
    a: 1.0,
};
pub const BG_ELEVATED: Color = Color {
    r: 0.10,
    g: 0.10,
    b: 0.13,
    a: 1.0,
};
pub const BORDER: Color = Color {
    r: 0.22,
    g: 0.22,
    b: 0.26,
    a: 1.0,
};

// Text palette
pub const TEXT_PRIMARY: Color = Color {
    r: 0.93,
    g: 0.93,
    b: 0.95,
    a: 1.0,
};
pub const TEXT_SECONDARY: Color = Color {
    r: 0.60,
    g: 0.60,
    b: 0.65,
    a: 1.0,
};
pub const TEXT_MUTED: Color = Color {
    r: 0.38,
    g: 0.38,
    b: 0.42,
    a: 1.0,
};

// Semantic colours
pub const GREEN: Color = Color {
    r: 0.27,
    g: 0.75,
    b: 0.43,
    a: 1.0,
};
pub const RED: Color = Color {
    r: 0.94,
    g: 0.32,
    b: 0.25,
    a: 1.0,
};
pub const YELLOW: Color = Color {
    r: 0.97,
    g: 0.74,
    b: 0.14,
    a: 1.0,
};

pub fn bytehive_theme() -> Theme {
    Theme::custom(
        "ByteHive".to_string(),
        Palette {
            background: BG_PRIMARY,
            text: TEXT_PRIMARY,
            primary: AMBER,
            success: GREEN,
            danger: RED,
            warning: YELLOW,
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
                a: 0.35,
            },
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

pub fn conflict_item_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color {
            r: 0.94,
            g: 0.32,
            b: 0.25,
            a: 0.08,
        })),
        border: Border {
            color: Color {
                r: 0.94,
                g: 0.32,
                b: 0.25,
                a: 0.25,
            },
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
        background: Some(Background::Color(Color {
            r: 0.06,
            g: 0.06,
            b: 0.08,
            a: 1.0,
        })),
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
        button::Status::Hovered => AMBER_LIGHT,
        button::Status::Pressed => AMBER_DARK,
        button::Status::Disabled => Color {
            r: 0.5,
            g: 0.4,
            b: 0.1,
            a: 0.5,
        },
        _ => AMBER,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color {
            r: 0.08,
            g: 0.06,
            b: 0.02,
            a: 1.0,
        },
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn btn_ghost(_theme: &Theme, status: button::Status) -> button::Style {
    let (bg, border_color) = match status {
        button::Status::Hovered | button::Status::Pressed => (
            Color {
                r: 0.22,
                g: 0.22,
                b: 0.26,
                a: 0.5,
            },
            BORDER,
        ),
        _ => (Color::TRANSPARENT, BORDER),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: TEXT_SECONDARY,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

pub fn btn_danger(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => Color {
            r: 0.75,
            g: 0.22,
            b: 0.17,
            a: 1.0,
        },
        _ => RED,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::WHITE,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn btn_flat(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.04,
        },
        button::Status::Pressed => Color {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 0.08,
        },
        _ => Color::TRANSPARENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: TEXT_PRIMARY,
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

// Text styles

pub fn muted(_theme: &Theme) -> text::Style {
    text::Style {
        color: Some(TEXT_MUTED),
    }
}

pub fn secondary(_theme: &Theme) -> text::Style {
    text::Style {
        color: Some(TEXT_SECONDARY),
    }
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
    text::Style {
        color: Some(YELLOW),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::button;

    // ─── Color constant properties ────────────────────────────────────────────

    #[test]
    fn all_surface_colors_are_fully_opaque() {
        let colors = [
            BG_PRIMARY,
            BG_SURFACE,
            BG_ELEVATED,
            BORDER,
            TEXT_PRIMARY,
            TEXT_SECONDARY,
            TEXT_MUTED,
        ];
        for c in &colors {
            assert_eq!(c.a, 1.0, "surface/text color must have alpha=1.0");
        }
    }

    #[test]
    fn all_brand_colors_are_fully_opaque() {
        let colors = [AMBER, AMBER_LIGHT, AMBER_DARK, GREEN, RED, YELLOW];
        for c in &colors {
            assert_eq!(c.a, 1.0, "brand color must have alpha=1.0");
        }
    }

    #[test]
    fn amber_has_high_red_component() {
        assert!(AMBER.r > 0.9, "AMBER should be predominantly red/orange");
    }

    #[test]
    fn green_has_high_green_component() {
        assert!(GREEN.g > 0.5, "GREEN should have a high green component");
        assert!(GREEN.g > GREEN.r, "GREEN's green channel should exceed red");
    }

    #[test]
    fn red_has_high_red_component() {
        assert!(RED.r > 0.8, "RED should have a high red component");
        assert!(RED.r > RED.g, "RED's red channel should exceed green");
    }

    #[test]
    fn bg_primary_is_dark() {
        // Dark theme: background should be close to black.
        assert!(BG_PRIMARY.r < 0.2, "BG_PRIMARY should be a dark colour");
        assert!(BG_PRIMARY.g < 0.2);
        assert!(BG_PRIMARY.b < 0.2);
    }

    #[test]
    fn text_primary_is_light() {
        // Dark theme: primary text should be close to white.
        assert!(
            TEXT_PRIMARY.r > 0.8,
            "TEXT_PRIMARY should be a light colour"
        );
    }

    #[test]
    fn amber_variants_are_all_distinct() {
        assert_ne!(AMBER, AMBER_LIGHT);
        assert_ne!(AMBER, AMBER_DARK);
        assert_ne!(AMBER_LIGHT, AMBER_DARK);
    }

    // ─── bytehive_theme ───────────────────────────────────────────────────────

    #[test]
    fn bytehive_theme_does_not_panic() {
        let _ = bytehive_theme();
    }

    // ─── Container styles ─────────────────────────────────────────────────────

    #[test]
    fn panel_style_has_background() {
        let style = panel(&bytehive_theme());
        assert!(style.background.is_some(), "panel must have a background");
    }

    #[test]
    fn panel_style_has_border() {
        let style = panel(&bytehive_theme());
        assert!(style.border.width > 0.0, "panel must have a visible border");
    }

    #[test]
    fn elevated_surface_style_has_background() {
        let style = elevated_surface(&bytehive_theme());
        assert!(style.background.is_some());
    }

    #[test]
    fn amber_panel_style_has_background() {
        let style = amber_panel(&bytehive_theme());
        assert!(style.background.is_some());
    }

    #[test]
    fn conflict_item_panel_style_has_background() {
        let style = conflict_item_panel(&bytehive_theme());
        assert!(style.background.is_some());
    }

    #[test]
    fn log_panel_style_has_background() {
        let style = log_panel(&bytehive_theme());
        assert!(style.background.is_some());
    }

    #[test]
    fn transparent_style_has_no_background() {
        let style = transparent(&bytehive_theme());
        assert!(
            style.background.is_none(),
            "transparent style must have no background"
        );
    }

    // ─── Text styles ──────────────────────────────────────────────────────────

    #[test]
    fn muted_text_returns_muted_color() {
        let style = muted(&bytehive_theme());
        assert_eq!(style.color, Some(TEXT_MUTED));
    }

    #[test]
    fn secondary_text_returns_secondary_color() {
        let style = secondary(&bytehive_theme());
        assert_eq!(style.color, Some(TEXT_SECONDARY));
    }

    #[test]
    fn amber_text_returns_amber_color() {
        let style = amber_text(&bytehive_theme());
        assert_eq!(style.color, Some(AMBER));
    }

    #[test]
    fn green_text_returns_green_color() {
        let style = green_text(&bytehive_theme());
        assert_eq!(style.color, Some(GREEN));
    }

    #[test]
    fn red_text_returns_red_color() {
        let style = red_text(&bytehive_theme());
        assert_eq!(style.color, Some(RED));
    }

    #[test]
    fn yellow_text_returns_yellow_color() {
        let style = yellow_text(&bytehive_theme());
        assert_eq!(style.color, Some(YELLOW));
    }

    #[test]
    fn all_text_style_colors_are_distinct() {
        let colors = [
            muted(&bytehive_theme()).color,
            secondary(&bytehive_theme()).color,
            amber_text(&bytehive_theme()).color,
            green_text(&bytehive_theme()).color,
            red_text(&bytehive_theme()).color,
            yellow_text(&bytehive_theme()).color,
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "text style at index {i} and {j} must return distinct colors"
                );
            }
        }
    }

    // ─── Button styles ────────────────────────────────────────────────────────

    #[test]
    fn btn_primary_active_background_is_amber() {
        let theme = bytehive_theme();
        let style = btn_primary(&theme, button::Status::Active);
        assert_eq!(
            style.background,
            Some(Background::Color(AMBER)),
            "btn_primary active should use AMBER"
        );
    }

    #[test]
    fn btn_primary_hovered_background_is_amber_light() {
        let theme = bytehive_theme();
        let style = btn_primary(&theme, button::Status::Hovered);
        assert_eq!(style.background, Some(Background::Color(AMBER_LIGHT)));
    }

    #[test]
    fn btn_primary_pressed_background_is_amber_dark() {
        let theme = bytehive_theme();
        let style = btn_primary(&theme, button::Status::Pressed);
        assert_eq!(style.background, Some(Background::Color(AMBER_DARK)));
    }

    #[test]
    fn btn_primary_disabled_background_differs_from_active() {
        let theme = bytehive_theme();
        let active = btn_primary(&theme, button::Status::Active);
        let disabled = btn_primary(&theme, button::Status::Disabled);
        assert_ne!(
            active.background, disabled.background,
            "disabled btn_primary must differ from active"
        );
    }

    #[test]
    fn btn_primary_text_color_is_dark() {
        let theme = bytehive_theme();
        let style = btn_primary(&theme, button::Status::Active);
        // Dark text on the amber background for contrast.
        assert!(
            style.text_color.r < 0.2,
            "btn_primary text should be dark to contrast with amber"
        );
    }

    #[test]
    fn btn_ghost_active_background_is_transparent() {
        let theme = bytehive_theme();
        let style = btn_ghost(&theme, button::Status::Active);
        assert_eq!(
            style.background,
            Some(Background::Color(Color::TRANSPARENT))
        );
    }

    #[test]
    fn btn_ghost_hovered_background_differs_from_active() {
        let theme = bytehive_theme();
        let active = btn_ghost(&theme, button::Status::Active);
        let hovered = btn_ghost(&theme, button::Status::Hovered);
        assert_ne!(active.background, hovered.background);
    }

    #[test]
    fn btn_flat_active_background_is_transparent() {
        let theme = bytehive_theme();
        let style = btn_flat(&theme, button::Status::Active);
        assert_eq!(
            style.background,
            Some(Background::Color(Color::TRANSPARENT))
        );
    }

    #[test]
    fn btn_flat_hovered_background_is_slightly_visible() {
        let theme = bytehive_theme();
        let style = btn_flat(&theme, button::Status::Hovered);
        // Hovered state must have a non-transparent background.
        assert_ne!(
            style.background,
            Some(Background::Color(Color::TRANSPARENT)),
            "btn_flat hovered should show a subtle background"
        );
    }

    #[test]
    fn btn_danger_active_background_is_red() {
        let theme = bytehive_theme();
        let style = btn_danger(&theme, button::Status::Active);
        assert_eq!(style.background, Some(Background::Color(RED)));
    }

    #[test]
    fn btn_danger_hovered_background_differs_from_active() {
        let theme = bytehive_theme();
        let active = btn_danger(&theme, button::Status::Active);
        let hovered = btn_danger(&theme, button::Status::Hovered);
        assert_ne!(active.background, hovered.background);
    }

    #[test]
    fn btn_danger_text_color_is_white() {
        let theme = bytehive_theme();
        let style = btn_danger(&theme, button::Status::Active);
        assert_eq!(style.text_color, Color::WHITE);
    }
}
