//! Light/dark theme: resolves the choice (or macOS's) and applies it to gpui-kit.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::*;
use rb_core::config::ThemeChoice;
use rust_i18n::t;
use gpui_kit::{Hsla, rgb};

use crate::settings;

/// Chosen theme; `Auto` is reapplied when macOS switches (`system_changed`).
pub struct Appearance(pub ThemeChoice);

impl Global for Appearance {}

pub fn resolve(choice: ThemeChoice, system: WindowAppearance) -> ThemeMode {
    match choice {
        ThemeChoice::Light => ThemeMode::Light,
        ThemeChoice::Dark => ThemeMode::Dark,
        ThemeChoice::Auto => system.into(),
    }
}

fn apply(choice: ThemeChoice, system: WindowAppearance, cx: &mut App) {
    Theme::change(resolve(choice, system), None, cx);
    Theme::update(cx, apply_palette);
    cx.set_global(Appearance(choice));
}

/// At startup, before opening the window.
pub fn init(cx: &mut App) {
    apply(settings::get(cx).theme, cx.window_appearance(), cx);
}

/// Switches now (`Theme::change` redraws the windows) and saves in the background.
pub fn set(choice: ThemeChoice, window: &Window, cx: &mut App) {
    apply(choice, window.appearance(), cx);
    settings::update(cx, |s| s.theme = choice);
}

/// macOS switched light/dark: only matters for `Auto`.
pub fn system_changed(window: &Window, cx: &mut App) {
    if cx.global::<Appearance>().0 == ThemeChoice::Auto {
        apply(ThemeChoice::Auto, window.appearance(), cx);
    }
}

/// Picker label; `Auto` shows what is in effect.
pub fn label(choice: ThemeChoice, cx: &App) -> String {
    let name = |m: ThemeMode| if m.is_dark() { t!("theme.dark") } else { t!("theme.light") }.to_string();
    match choice {
        ThemeChoice::Auto => format!("Auto ({})", name(cx.theme().mode)),
        ThemeChoice::Light => name(ThemeMode::Light),
        ThemeChoice::Dark => name(ThemeMode::Dark),
    }
}


/// Colors of the "Rustflake UX Concepts" canvas; the light one is derived from the dark.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub surface: Hsla,
    pub secondary: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,
    pub prod: Hsla,
}

pub fn palette(dark: bool) -> Palette {
    let c = |hex: u32| -> Hsla { rgb(hex).into() };
    if dark {
        Palette {
            background: c(0x0B0B0C),
            surface: c(0x111113),
            secondary: c(0x17171A),
            border: c(0x1D1D21),
            border_strong: c(0x26262B),
            foreground: c(0xE8E8EC),
            muted: c(0x9A9AA3),
            accent: c(0x6CC4F0),
            accent_foreground: c(0x0B0B0C),
            prod: c(0xF0A35E),
        }
    } else {
        Palette {
            background: c(0xFAFAFA),
            surface: c(0xFFFFFF),
            secondary: c(0xF0F0F2),
            border: c(0xE4E4E8),
            border_strong: c(0xD4D4DA),
            foreground: c(0x141416),
            muted: c(0x5E5E66),
            accent: c(0x1A74A3),
            accent_foreground: c(0xFFFFFF),
            prod: c(0xA0560F),
        }
    }
}

/// WCAG contrast ratio (only the legibility test uses it).
#[cfg(test)]
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let lum = |h: Hsla| {
        let c: gpui_kit::Rgba = h.into();
        let ch = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `Theme::change` reloads the registered theme; reapply after every switch.
fn apply_palette(theme: &mut Theme) {
    let p = palette(theme.mode.is_dark());
    let t = &mut theme.colors;
    t.background = p.background;
    t.foreground = p.foreground;
    t.border = p.border;
    t.input = p.border_strong;
    t.ring = p.accent;
    t.caret = p.accent;
    t.popover = p.surface;
    t.popover_foreground = p.foreground;
    t.muted = p.secondary;
    t.muted_foreground = p.muted;
    t.secondary = p.secondary;
    t.secondary_hover = p.border_strong;
    t.secondary_active = p.border_strong;
    t.secondary_foreground = p.foreground;
    // Menu/list hover: secondary disappears over the popover (#111113 × #17171A).
    t.accent = p.border_strong;
    t.accent_foreground = p.foreground;
    t.primary = p.accent;
    t.primary_hover = p.accent.opacity(0.9);
    t.primary_active = p.accent.opacity(0.8);
    t.primary_foreground = p.accent_foreground;
    t.link = p.accent;
    t.title_bar = p.background;
    t.title_bar_border = p.border;
    t.status_bar = p.background;
    t.status_bar_border = p.border;
    t.sidebar = p.background;
    t.sidebar_border = p.border;
    t.sidebar_foreground = p.foreground;
    t.tab_bar = p.background;
    t.tab = p.background;
    t.tab_active = p.secondary;
    t.tab_foreground = p.muted;
    t.tab_active_foreground = p.foreground;
    t.list = p.background;
    t.list_hover = p.border_strong;
    t.list_active = p.border_strong;
    t.list_even = p.background;
    t.list_head = p.background;
    t.table = p.background;
    t.table_head = p.background;
    t.table_head_foreground = p.muted;
    t.table_hover = p.secondary;
    t.table_even = p.background;
    t.table_row_border = p.border;
    t.window_border = p.border;
    t.selection = p.accent.opacity(0.3);
    theme.font_family = "Geist".into();
    theme.mono_font_family = "Geist Mono".into();
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    #[test]
    fn palette_text_is_readable_in_both_modes() {
        for dark in [true, false] {
            let p = super::palette(dark);
            for (fg, bg, name) in [
                (p.foreground, p.background, "fg/bg"),
                (p.foreground, p.surface, "fg/surface"),
                (p.muted, p.background, "muted/bg"),
                (p.muted, p.surface, "muted/surface"),
                (p.accent, p.background, "accent/bg"),
                (p.prod, p.background, "prod/bg"),
                (p.accent_foreground, p.accent, "accent_fg/accent"),
            ] {
                let c = super::contrast(fg, bg);
                assert!(c >= 4.5, "{name} dark={dark}: {c:.2}");
            }
        }
    }

    use super::*;
    use gpui_kit::WindowAppearance;
    use gpui_kit::component::ThemeMode;
    use rb_core::config::ThemeChoice;

    #[test]
    fn auto_follows_the_system_and_explicit_choice_wins() {
        assert_eq!(resolve(ThemeChoice::Auto, WindowAppearance::Dark), ThemeMode::Dark);
        assert_eq!(resolve(ThemeChoice::Auto, WindowAppearance::VibrantLight), ThemeMode::Light);
        assert_eq!(resolve(ThemeChoice::Light, WindowAppearance::Dark), ThemeMode::Light);
        assert_eq!(resolve(ThemeChoice::Dark, WindowAppearance::Light), ThemeMode::Dark);
    }
}
