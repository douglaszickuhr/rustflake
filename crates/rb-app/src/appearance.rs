//! Tema claro/escuro: resolve a escolha (ou o do macOS) e aplica no gpui-kit.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::*;
use rb_core::config::{self, ThemeChoice};
use rust_i18n::t;

/// Tema escolhido; `Auto` é reaplicado quando o macOS troca (`system_changed`).
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
    cx.set_global(Appearance(choice));
}

/// Na partida, antes de abrir a janela.
pub fn init(cx: &mut App) {
    apply(config::load_settings(&config::settings_path()).theme, cx.window_appearance(), cx);
}

/// Troca já (o `Theme::change` redesenha as janelas) e salva em segundo plano.
pub fn set(choice: ThemeChoice, window: &Window, cx: &mut App) {
    apply(choice, window.appearance(), cx);
    let path = config::settings_path();
    cx.background_spawn(async move {
        if let Err(e) = config::update_settings(&path, |s| s.theme = choice) {
            tracing::warn!("settings.toml: {e}");
        }
    })
    .detach();
}

/// O macOS trocou claro/escuro: só importa no `Auto`.
pub fn system_changed(window: &Window, cx: &mut App) {
    if cx.global::<Appearance>().0 == ThemeChoice::Auto {
        apply(ThemeChoice::Auto, window.appearance(), cx);
    }
}

/// Rótulo do seletor; `Auto` mostra o que está valendo.
pub fn label(choice: ThemeChoice, cx: &App) -> String {
    let name = |m: ThemeMode| if m.is_dark() { t!("theme.dark") } else { t!("theme.light") }.to_string();
    match choice {
        ThemeChoice::Auto => format!("Auto ({})", name(cx.theme().mode)),
        ThemeChoice::Light => name(ThemeMode::Light),
        ThemeChoice::Dark => name(ThemeMode::Dark),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::WindowAppearance;
    use gpui_kit::component::ThemeMode;
    use rb_core::config::ThemeChoice;

    #[::core::prelude::v1::test]
    fn auto_follows_the_system_and_explicit_choice_wins() {
        assert_eq!(resolve(ThemeChoice::Auto, WindowAppearance::Dark), ThemeMode::Dark);
        assert_eq!(resolve(ThemeChoice::Auto, WindowAppearance::VibrantLight), ThemeMode::Light);
        assert_eq!(resolve(ThemeChoice::Light, WindowAppearance::Dark), ThemeMode::Light);
        assert_eq!(resolve(ThemeChoice::Dark, WindowAppearance::Light), ThemeMode::Dark);
    }
}
