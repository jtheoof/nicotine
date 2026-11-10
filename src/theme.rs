//! App-wide colour themes: the config panel and the Linux (Wayland)
//! preview chrome.
//!
//! Configured in config.toml:
//!
//! ```toml
//! theme = "nord"                  # nicotine (default) | nord | nord-dark | nord-light
//!
//! [preview_colors]                # optional preview-chrome overrides
//! active = "#88C0D0"
//! ```
//!
//! `preview_theme` is accepted as an older name for `theme`. Nord colours
//! are from the official palette (<https://www.nordtheme.com>): Polar Night
//! nord0–3, Snow Storm nord4–6, Frost nord7–10, Aurora nord11–15.

use crate::config::PreviewColors;

/// An sRGB colour.
pub type Rgb = (u8, u8, u8);

/// Colours of a preview's chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Only the Linux Wayland previews draw with this today.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct PreviewTheme {
    /// Title strip and border of the focused client's preview.
    pub active: Rgb,
    /// Title text on `active`.
    pub active_text: Rgb,
    /// Title strip and border of other previews.
    pub inactive: Rgb,
    /// Title text on `inactive`.
    pub text: Rgb,
    /// Thumbnail area before the first frame arrives.
    pub background: Rgb,
}

/// Colours of the config panel, by role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelPalette {
    /// Window, sidebar and footer background.
    pub background: Rgb,
    /// Raised cards (a cycle-list row being dragged).
    pub surface: Rgb,
    /// Body text.
    pub text: Rgb,
    /// Brand/primary colour: header, active tab, links, section titles,
    /// primary buttons.
    pub accent: Rgb,
    /// Text drawn on `accent`.
    pub on_accent: Rgb,
    /// Dividers, outlines, hover fills.
    pub subtle: Rgb,
    /// "Latest version" badge and success states.
    pub success: Rgb,
    /// Destructive / error states.
    pub danger: Rgb,
}

/// A complete app theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    // Read only by the Linux preview path.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub preview: PreviewTheme,
    pub panel: PanelPalette,
}

mod nord {
    use super::Rgb;
    pub const NORD0: Rgb = (0x2e, 0x34, 0x40);
    pub const NORD1: Rgb = (0x3b, 0x42, 0x52);
    pub const NORD3: Rgb = (0x4c, 0x56, 0x6a);
    pub const NORD4: Rgb = (0xd8, 0xde, 0xe9);
    pub const NORD5: Rgb = (0xe5, 0xe9, 0xf0);
    pub const NORD6: Rgb = (0xec, 0xef, 0xf4);
    pub const NORD8: Rgb = (0x88, 0xc0, 0xd0);
    pub const NORD10: Rgb = (0x5e, 0x81, 0xac);
    pub const NORD11: Rgb = (0xbf, 0x61, 0x6a);
    pub const NORD14: Rgb = (0xa3, 0xbe, 0x8c);
}

const NICOTINE_RED: Rgb = (196, 30, 58);
const NICOTINE_CREAM: Rgb = (252, 250, 242);
const NICOTINE_BLACK: Rgb = (30, 30, 30);
const NICOTINE_GOLD: Rgb = (180, 155, 105);
const NICOTINE_GREEN: Rgb = (60, 140, 70);

impl Theme {
    /// The original Nicotine look.
    pub const NICOTINE: Self = Self {
        preview: PreviewTheme {
            active: NICOTINE_RED,
            active_text: NICOTINE_CREAM,
            inactive: NICOTINE_BLACK,
            text: NICOTINE_CREAM,
            background: (0, 0, 0),
        },
        panel: PanelPalette {
            background: NICOTINE_CREAM,
            surface: (255, 255, 255),
            text: NICOTINE_BLACK,
            accent: NICOTINE_RED,
            on_accent: NICOTINE_CREAM,
            subtle: NICOTINE_GOLD,
            success: NICOTINE_GREEN,
            danger: NICOTINE_RED,
        },
    };

    /// Nord dark: Polar Night surfaces, Snow Storm text, Frost accent.
    pub const NORD_DARK: Self = Self {
        preview: PreviewTheme {
            active: nord::NORD8,
            active_text: nord::NORD0,
            inactive: nord::NORD1,
            text: nord::NORD6,
            background: nord::NORD0,
        },
        panel: PanelPalette {
            background: nord::NORD0,
            surface: nord::NORD1,
            text: nord::NORD4,
            accent: nord::NORD8,
            on_accent: nord::NORD0,
            subtle: nord::NORD3,
            success: nord::NORD14,
            danger: nord::NORD11,
        },
    };

    /// Nord light: Snow Storm surfaces, Polar Night text, deep Frost accent.
    pub const NORD_LIGHT: Self = Self {
        preview: PreviewTheme {
            active: nord::NORD10,
            active_text: nord::NORD6,
            inactive: nord::NORD4,
            text: nord::NORD0,
            background: nord::NORD5,
        },
        panel: PanelPalette {
            background: nord::NORD6,
            surface: nord::NORD5,
            text: nord::NORD0,
            accent: nord::NORD10,
            on_accent: nord::NORD6,
            subtle: nord::NORD4,
            success: nord::NORD14,
            danger: nord::NORD11,
        },
    };

    pub fn preset(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "nicotine" | "default" => Some(Self::NICOTINE),
            "nord" | "nord-dark" | "nord_dark" => Some(Self::NORD_DARK),
            "nord-light" | "nord_light" => Some(Self::NORD_LIGHT),
            _ => None,
        }
    }

    /// The configured theme (default `nicotine`); unknown names are
    /// reported and fall back to the default.
    pub fn named(name: Option<&str>) -> Self {
        match name {
            None => Self::NICOTINE,
            Some(n) => Self::preset(n).unwrap_or_else(|| {
                eprintln!(
                    "Unknown theme '{n}' (expected nicotine, nord, nord-dark or nord-light); using nicotine"
                );
                Self::NICOTINE
            }),
        }
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
impl PreviewTheme {
    /// Preview colours from config: the theme's, then any
    /// `[preview_colors]` overrides. Malformed colours are reported and
    /// ignored.
    pub fn from_config(theme: Option<&str>, overrides: Option<&PreviewColors>) -> Self {
        let mut preview = Theme::named(theme).preview;
        if let Some(o) = overrides {
            for (key, value, slot) in [
                ("active", &o.active, &mut preview.active),
                ("active_text", &o.active_text, &mut preview.active_text),
                ("inactive", &o.inactive, &mut preview.inactive),
                ("text", &o.text, &mut preview.text),
                ("background", &o.background, &mut preview.background),
            ] {
                let Some(value) = value else {
                    continue;
                };
                match parse_hex(value) {
                    Some(rgb) => *slot = rgb,
                    None => eprintln!(
                        "preview_colors.{key} = '{value}' isn't a #RRGGBB colour; ignoring"
                    ),
                }
            }
        }
        preview
    }

    /// Strip/border colour and text colour for a preview.
    pub fn chrome(&self, active: bool) -> (Rgb, Rgb) {
        if active {
            (self.active, self.active_text)
        } else {
            (self.inactive, self.text)
        }
    }
}

/// Parse `#RRGGBB`, `RRGGBB` or `#RGB`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let len = match hex.len() {
        6 => 2,
        3 => 1,
        _ => return None,
    };
    let channel = |i: usize| {
        let v = u8::from_str_radix(&hex[i * len..(i + 1) * len], 16).ok()?;
        Some(if len == 1 { v * 17 } else { v })
    };
    Some((channel(0)?, channel(1)?, channel(2)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colours() {
        assert_eq!(parse_hex("#88C0D0"), Some((0x88, 0xc0, 0xd0)));
        assert_eq!(parse_hex("2e3440"), Some((0x2e, 0x34, 0x40)));
        assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex(" #ECEFF4 "), Some((0xec, 0xef, 0xf4)));
        assert_eq!(parse_hex("#12345"), None);
        assert_eq!(parse_hex("#gggggg"), None);
        assert_eq!(parse_hex("red"), None);
    }

    #[test]
    fn presets_by_name() {
        assert_eq!(Theme::preset("nicotine"), Some(Theme::NICOTINE));
        assert_eq!(Theme::preset("Nord"), Some(Theme::NORD_DARK));
        assert_eq!(Theme::preset("nord-dark"), Some(Theme::NORD_DARK));
        assert_eq!(Theme::preset("nord-light"), Some(Theme::NORD_LIGHT));
        assert_eq!(Theme::preset("solarized"), None);
    }

    #[test]
    fn nord_uses_official_palette() {
        let d = Theme::NORD_DARK;
        assert_eq!(d.preview.active, parse_hex("#88C0D0").unwrap());
        assert_eq!(d.preview.inactive, parse_hex("#3B4252").unwrap());
        assert_eq!(d.preview.text, parse_hex("#ECEFF4").unwrap());
        assert_eq!(d.panel.background, parse_hex("#2E3440").unwrap());
        assert_eq!(d.panel.text, parse_hex("#D8DEE9").unwrap());
        assert_eq!(d.panel.accent, parse_hex("#88C0D0").unwrap());
        let l = Theme::NORD_LIGHT;
        assert_eq!(l.preview.active, parse_hex("#5E81AC").unwrap());
        assert_eq!(l.preview.inactive, parse_hex("#D8DEE9").unwrap());
        assert_eq!(l.preview.text, parse_hex("#2E3440").unwrap());
        assert_eq!(l.panel.background, parse_hex("#ECEFF4").unwrap());
        assert_eq!(l.panel.accent, parse_hex("#5E81AC").unwrap());
    }

    #[test]
    fn nicotine_panel_matches_original_brand_colours() {
        let p = Theme::NICOTINE.panel;
        assert_eq!(p.accent, (196, 30, 58));
        assert_eq!(p.background, (252, 250, 242));
        assert_eq!(p.subtle, (180, 155, 105));
    }

    #[test]
    fn default_and_unknown_fall_back_to_nicotine() {
        assert_eq!(Theme::named(None), Theme::NICOTINE);
        assert_eq!(Theme::named(Some("nope")), Theme::NICOTINE);
        assert_eq!(
            PreviewTheme::from_config(None, None),
            Theme::NICOTINE.preview
        );
    }

    #[test]
    fn overrides_apply_on_top_of_preset() {
        let o = PreviewColors {
            active: Some("#BF616A".into()),
            text: Some("not a colour".into()),
            ..Default::default()
        };
        let t = PreviewTheme::from_config(Some("nord"), Some(&o));
        assert_eq!(t.active, (0xbf, 0x61, 0x6a));
        // Malformed override is ignored; preset value kept.
        assert_eq!(t.text, Theme::NORD_DARK.preview.text);
        assert_eq!(t.inactive, Theme::NORD_DARK.preview.inactive);
    }

    #[test]
    fn chrome_picks_state_colours() {
        let t = Theme::NORD_DARK.preview;
        assert_eq!(t.chrome(true), (t.active, t.active_text));
        assert_eq!(t.chrome(false), (t.inactive, t.text));
    }
}
