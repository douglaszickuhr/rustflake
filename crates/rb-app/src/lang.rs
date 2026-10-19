//! Interface language: resolves the choice (or macOS's), applies it to `rust-i18n` and notifies the UI.

use gpui_kit::*;
use rb_core::config::{self, Language};

/// Chosen language; changing the global notifies observers (`cx.observe_global::<Lang>`).
pub struct Lang(pub Language);

impl Global for Lang {}

pub fn resolve(choice: Language, os: Option<&str>) -> &'static str {
    match choice {
        Language::Pt => "pt",
        Language::En => "en",
        Language::Es => "es",
        Language::Auto => match os.map(|l| l.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase()).as_deref() {
            Some("pt") => "pt",
            Some("es") => "es",
            _ => "en",
        },
    }
}

fn apply(choice: Language, cx: &mut App) {
    rust_i18n::set_locale(resolve(choice, sys_locale::get_locale().as_deref()));
    cx.set_global(Lang(choice));
}

/// At startup, before opening the window.
pub fn init(cx: &mut App) {
    apply(config::load_settings(&config::settings_path()).language, cx);
}

/// Switches now, saves in the background and redraws everything.
pub fn set(choice: Language, cx: &mut App) {
    apply(choice, cx);
    let path = config::settings_path();
    cx.background_spawn(async move {
        if let Err(e) = config::update_settings(&path, |s| s.language = choice) {
            tracing::warn!("settings.toml: {e}");
        }
    })
    .detach();
    cx.refresh_windows();
}

/// Picker label: language name in its own language; `Auto` shows what it resolved to.
pub fn label(choice: Language) -> String {
    let name = |code: &str| match code {
        "pt" => "Português",
        "es" => "Español",
        _ => "English",
    };
    match choice {
        Language::Auto => format!("Auto ({})", name(resolve(Language::Auto, sys_locale::get_locale().as_deref()))),
        other => name(resolve(other, None)).to_string(),
    }
}

#[cfg(test)]
#[path = "../../rb-core/src/i18n_check.rs"]
mod i18n_check;

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn os_locale_maps_to_supported_language() {
        use rb_core::config::Language::*;
        assert_eq!(resolve(Auto, Some("pt-BR")), "pt");
        assert_eq!(resolve(Auto, Some("pt-PT")), "pt");
        assert_eq!(resolve(Auto, Some("es-419")), "es");
        assert_eq!(resolve(Auto, Some("en-GB")), "en");
        assert_eq!(resolve(Auto, Some("de-DE")), "en");
        assert_eq!(resolve(Auto, Some("zh-Hans-CN")), "en");
        assert_eq!(resolve(Auto, None), "en");
        assert_eq!(resolve(Es, Some("pt-BR")), "es");
        assert_eq!(resolve(Pt, None), "pt");
    }

    #[::core::prelude::v1::test]
    fn nested_toml_keys_resolve_per_locale() {
        // Explicit locale: the global is shared between parallel tests.
        assert_eq!(rust_i18n::t!("tree.loading", locale = "pt"), "carregando…");
        assert_eq!(rust_i18n::t!("tree.loading", locale = "es"), "cargando…");
        assert_eq!(rust_i18n::t!("tree.loading", locale = "de"), "loading…");
    }

    #[::core::prelude::v1::test]
    fn every_key_is_translated_and_used_keys_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        i18n_check::check(&root.join("src"), &root.join("locales/app.toml"));
    }
}
