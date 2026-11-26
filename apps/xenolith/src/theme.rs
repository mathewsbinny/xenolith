//! Tool Suite Design Language theme loader (`design/tokens.json`): Dark and Light palettes applied to `egui` and
//! `egui_dock`, matching the BFBC2 Map Editor's visual language.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};
use serde::Deserialize;

const TOKENS_JSON: &str = include_str!("../../../design/tokens.json");

/// Active UI theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ThemeId {
    /// Dark studio theme (default).
    #[default]
    Dark,
    /// Light studio theme.
    Light,
}

impl ThemeId {
    /// Human-readable label.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    /// Toggles between Dark and Light.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

/// Resolved color palette for one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Application background behind docked panels.
    pub ground: Color32,
    /// Panel body background.
    pub surface_1: Color32,
    /// Inputs, raised toolbars, and hovered rows.
    pub surface_2: Color32,
    /// Panel header strips, active tabs, and pressed controls.
    pub surface_3: Color32,
    /// Subtle hairline divider.
    pub line: Color32,
    /// Strong border / focus frame.
    pub line_strong: Color32,
    /// Primary text color.
    pub ink: Color32,
    /// Secondary / muted text color.
    pub ink_muted: Color32,
    /// Faint / disabled text color.
    pub ink_faint: Color32,
    /// Primary accent color.
    pub accent: Color32,
    /// Text color on accent background.
    pub on_accent: Color32,
    /// Accent-tinted link / active text color.
    pub accent_text: Color32,
    /// Subtle accent fill.
    pub accent_subtle: Color32,
    /// Selection highlight.
    pub selection: Color32,
    /// Keyboard focus ring.
    pub focus: Color32,
    /// Success status color.
    pub status_ok: Color32,
    /// Warning status color.
    pub status_warn: Color32,
    /// Error status color.
    pub status_error: Color32,
    /// Informational status color.
    pub status_info: Color32,
}

#[derive(Deserialize)]
struct TokensFile {
    color: ColorSection,
}

#[derive(Deserialize)]
struct ColorSection {
    tokens: Vec<ColorToken>,
}

#[derive(Deserialize)]
struct ColorToken {
    name: String,
    value: BTreeMap<String, String>,
}

/// Parsed design tokens (loaded once from `design/tokens.json`).
pub struct Tokens {
    /// Dark palette.
    pub dark: Palette,
    /// Light palette.
    pub light: Palette,
}

impl Tokens {
    /// Returns the palette for `theme`.
    #[must_use]
    pub const fn palette(&self, theme: ThemeId) -> Palette {
        match theme {
            ThemeId::Dark => self.dark,
            ThemeId::Light => self.light,
        }
    }
}

fn parse_hex(s: &str) -> Color32 {
    let hex = s.trim_start_matches('#');
    if hex.len() == 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        )
    {
        return Color32::from_rgb(r, g, b);
    }
    Color32::MAGENTA
}

fn build_palette(map: &BTreeMap<String, BTreeMap<String, String>>, key: &str, fallback_dark: bool) -> Palette {
    let get = |name: &str, def: Color32| -> Color32 {
        map.get(name).and_then(|v| v.get(key)).map_or(def, |hex| parse_hex(hex))
    };
    let bg = if fallback_dark {
        Color32::from_rgb(0x14, 0x14, 0x14)
    } else {
        Color32::from_rgb(0xb4, 0xb4, 0xb4)
    };
    let fg = if fallback_dark {
        Color32::from_rgb(0xe3, 0xe3, 0xe3)
    } else {
        Color32::from_rgb(0x10, 0x10, 0x10)
    };
    Palette {
        ground: get("ground", bg),
        surface_1: get("surface-1", bg),
        surface_2: get("surface-2", bg),
        surface_3: get("surface-3", bg),
        line: get("line", bg),
        line_strong: get("line-strong", fg),
        ink: get("ink", fg),
        ink_muted: get("ink-muted", fg),
        ink_faint: get("ink-faint", fg),
        accent: get("accent", Color32::from_rgb(0x34, 0x75, 0xd0)),
        on_accent: get("on-accent", Color32::WHITE),
        accent_text: get("accent-text", fg),
        accent_subtle: get("accent-subtle", bg),
        selection: get("selection", Color32::from_rgb(0xf0, 0xa0, 0x30)),
        focus: get("focus", Color32::from_rgb(0x7c, 0xb9, 0xff)),
        status_ok: get("status-ok", Color32::from_rgb(0x6c, 0xc2, 0x8d)),
        status_warn: get("status-warn", Color32::from_rgb(0xe6, 0xc3, 0x4a)),
        status_error: get("status-error", Color32::from_rgb(0xf0, 0x74, 0x66)),
        status_info: get("status-info", Color32::from_rgb(0x73, 0xae, 0xe3)),
    }
}

static TOKENS: LazyLock<Tokens> = LazyLock::new(|| {
    let map: BTreeMap<String, BTreeMap<String, String>> = serde_json::from_str::<TokensFile>(TOKENS_JSON)
        .map_or_default(|f| f.color.tokens.into_iter().map(|t| (t.name, t.value)).collect());
    Tokens {
        dark: build_palette(&map, "dark", true),
        light: build_palette(&map, "light", false),
    }
});

/// Global parsed design tokens.
#[must_use]
pub fn tokens() -> &'static Tokens {
    &TOKENS
}

/// Applies the Tool Suite Design Language theme to an `egui::Context`.
pub fn apply(ctx: &egui::Context, theme: ThemeId) {
    let p = tokens().palette(theme);
    let mut visuals = match theme {
        ThemeId::Dark => Visuals::dark(),
        ThemeId::Light => Visuals::light(),
    };
    visuals.panel_fill = p.surface_1;
    visuals.window_fill = p.surface_1;
    visuals.extreme_bg_color = p.ground;
    visuals.faint_bg_color = p.surface_2;
    visuals.override_text_color = Some(p.ink);
    visuals.selection.bg_fill = p.accent_subtle;
    visuals.selection.stroke = Stroke::new(1.0, p.accent);
    visuals.hyperlink_color = p.accent_text;
    visuals.warn_fg_color = p.status_warn;
    visuals.error_fg_color = p.status_error;
    visuals.window_stroke = Stroke::new(1.0, p.line_strong);
    visuals.widgets.noninteractive.bg_fill = p.surface_1;
    visuals.widgets.noninteractive.weak_bg_fill = p.surface_1;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.ink);
    visuals.widgets.inactive.bg_fill = p.surface_2;
    visuals.widgets.inactive.weak_bg_fill = p.surface_2;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, p.ink);
    visuals.widgets.hovered.bg_fill = p.surface_3;
    visuals.widgets.hovered.weak_bg_fill = p.surface_3;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.line_strong);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, p.ink);
    visuals.widgets.active.bg_fill = p.accent;
    visuals.widgets.active.weak_bg_fill = p.accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, p.focus);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, p.on_accent);
    visuals.widgets.open.bg_fill = p.surface_3;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, p.line_strong);
    visuals.window_corner_radius = CornerRadius::same(4);
    ctx.set_visuals(visuals);
}

/// Builds an `egui_dock::Style` aligned with the Tool Suite Design Language.
#[must_use]
pub fn dock_style(egui_style: &egui::Style, theme: ThemeId) -> egui_dock::Style {
    let p = tokens().palette(theme);
    let mut style = egui_dock::Style::from_egui(egui_style);
    style.main_surface_border_stroke = Stroke::new(1.0, p.line);
    style.tab_bar.bg_fill = p.ground;
    style.tab_bar.hline_color = p.line;
    style.tab.tab_body.bg_fill = p.surface_1;
    style.tab.tab_body.stroke = Stroke::new(1.0, p.line);
    style.tab.active.bg_fill = p.surface_3;
    style.tab.active.text_color = p.ink;
    style.tab.focused.bg_fill = p.surface_3;
    style.tab.focused.text_color = p.ink;
    style.tab.inactive.bg_fill = p.surface_2;
    style.tab.inactive.text_color = p.ink_muted;
    style.tab.hovered.bg_fill = p.surface_3;
    style.tab.hovered.text_color = p.ink;
    style.separator.color_idle = p.line;
    style.separator.color_hovered = p.accent;
    style.separator.color_dragged = p.focus;
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_dark_and_light_palettes_from_tokens_json() {
        let t = tokens();
        assert_eq!(t.dark.ground, Color32::from_rgb(0x14, 0x14, 0x14));
        assert_eq!(t.light.ground, Color32::from_rgb(0xb4, 0xb4, 0xb4));
        assert_eq!(t.dark.accent, Color32::from_rgb(0x34, 0x75, 0xd0));
        assert_eq!(t.light.accent, Color32::from_rgb(0x1c, 0x5f, 0xae));
    }
}
