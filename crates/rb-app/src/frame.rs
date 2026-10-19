//! Pure frame pieces: context bar text and environment color.

use gpui_kit::Hsla;
use rb_core::config::Environment;
use rb_core::dialect::Dialect;

use crate::appearance::palette;

/// Per-part limit: the Button doesn't shrink, so a long name would push ⌘K out.
pub const MAX_PART: usize = 24;

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_PART { s.to_string() } else { format!("{}…", s.chars().take(MAX_PART - 1).collect::<String>()) }
}

/// Context bar parts; missing ones become "—"; each is cut at `MAX_PART`.
pub fn context_parts(env: Option<Environment>, dialect: Dialect, user: &str, role: Option<&str>, warehouse: Option<&str>, search_path: &[String]) -> Vec<String> {
    let dash = |v: Option<&str>| clip(v.filter(|s| !s.is_empty()).unwrap_or("—"));
    let mut out: Vec<String> = env.map(|e| e.label().to_string()).into_iter().collect();
    let first = search_path.first().map(String::as_str);
    match dialect {
        Dialect::Snowflake => out.extend([dash(role), dash(warehouse), dash(first)]),
        Dialect::Postgres => out.extend([dash(Some(user)), dash(first)]),
    }
    out
}

/// Only production gets a color (window border and label).
pub fn env_color(env: Option<Environment>, dark: bool) -> Option<Hsla> {
    (env == Some(Environment::Production)).then(|| palette(dark).prod)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[::core::prelude::v1::test]
    fn snowflake_full_and_missing_parts() {
        assert_eq!(
            context_parts(Some(Environment::Production), Dialect::Snowflake, "u", Some("DATA_ENGINEER"), Some("GENERAL_WH"), &sp(&["REPORTING_PROD.PUBLIC"])),
            sp(&["PROD", "DATA_ENGINEER", "GENERAL_WH", "REPORTING_PROD.PUBLIC"])
        );
        assert_eq!(context_parts(None, Dialect::Snowflake, "u", None, None, &[]), sp(&["—", "—", "—"]));
    }

    #[::core::prelude::v1::test]
    fn postgres_uses_user_and_search_path() {
        assert_eq!(context_parts(Some(Environment::Development), Dialect::Postgres, "app", None, None, &sp(&["public", "x"])), sp(&["DEV", "app", "public"]));
    }

    #[::core::prelude::v1::test]
    fn only_production_is_colored() {
        assert!(env_color(Some(Environment::Production), true).is_some());
        assert!(env_color(Some(Environment::Staging), true).is_none());
        assert!(env_color(None, false).is_none());
    }

    #[::core::prelude::v1::test]
    fn long_parts_are_truncated() {
        let long = "ANALYTICS_ENGINEERING_READONLY_ROLE";
        let p = context_parts(None, Dialect::Snowflake, "u", Some(long), Some("WH"), &sp(&["DB.S"]));
        assert_eq!(p[0].chars().count(), MAX_PART);
        assert!(p[0].ends_with('…') && p[0].starts_with("ANALYTICS_ENGINEERING"));
        assert_eq!(p[1], "WH");
    }
}
