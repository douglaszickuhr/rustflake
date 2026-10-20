//! Pieces shared by the screens: background-loaded values, relative times and section titles.

use gpui_kit::component::{ActiveTheme as _, Theme, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::{App, Div, div, px};
use rb_core::user_view::{self, Span};
use rust_i18n::t;

/// What a badge says: neutral text, or a status shown by a small colored dot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warning,
    Danger,
}

/// The app's one badge: outlined pill, normal text color, colored dot for a status. The filled
/// component tags (bright background, dim text) were hard to read; add the text with `.child`.
pub fn badge(tone: Tone, cx: &App) -> Div {
    let theme = cx.theme();
    let dot = match tone {
        Tone::Neutral => None,
        Tone::Info => Some(theme.info),
        Tone::Success => Some(theme.success),
        Tone::Warning => Some(theme.warning),
        Tone::Danger => Some(theme.danger),
    };
    h_flex()
        .flex_shrink_0()
        .h(px(20.))
        .px_2()
        .gap_1p5()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.foreground)
        .whitespace_nowrap()
        .when_some(dot, |d, c| d.child(div().size(px(6.)).flex_shrink_0().rounded_full().bg(c)))
}

/// A value fetched in the background.
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// "3 months", in the interface language.
pub fn span_label(s: Span) -> String {
    match s {
        Span::Minutes(n) => t!("span.minutes", n = n),
        Span::Hours(n) => t!("span.hours", n = n),
        Span::Days(n) => t!("span.days", n = n),
        Span::Months(n) => t!("span.months", n = n),
        Span::Years(n) => t!("span.years", n = n),
    }
    .to_string()
}

/// Time since a Unix second, as a span.
pub fn since(secs: i64) -> String {
    span_label(user_view::span(user_view::now() - secs))
}

/// Local date and time of a Unix second, for hovers ("2026-10-19 08:12:45 +01:00").
pub fn local_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S %:z").to_string()).unwrap_or_default()
}

/// Shortcut text for this platform: written with Mac symbols, shown as Ctrl/Alt/Shift elsewhere
/// (GPUI's `secondary-` bindings are ⌘ on macOS and Ctrl on Windows/Linux).
pub fn keys(s: &str) -> String {
    if cfg!(target_os = "macos") { s.to_string() } else { keys_for_pc(s) }
}

fn keys_for_pc(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let m = match c {
            '⌘' | '⌃' => "Ctrl+",
            '⌥' => "Alt+",
            '⇧' => "Shift+",
            '↵' => {
                out.push_str("Enter");
                continue;
            }
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push_str(m);
        // "⌘ scroll" → "Ctrl+scroll"
        if chars.peek() == Some(&' ') {
            chars.next();
        }
    }
    out
}

/// Exact count with thousands separators (807312 → "807,312").
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// "3 months ago" from a Unix second.
pub fn ago(secs: i64) -> String {
    t!("admin.ago", ago = since(secs)).to_string()
}

/// Small uppercase title over a group of rows.
pub fn section(title: String, theme: &Theme) -> Div {
    div().pt_3().pb_1().text_xs().text_color(theme.muted_foreground).child(title.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{grouped, keys_for_pc};

    #[test]
    fn mac_symbols_become_pc_keys() {
        assert_eq!(keys_for_pc("⌘⇧↵ run all"), "Ctrl+Shift+Enter run all");
        assert_eq!(keys_for_pc("⌥⌘F"), "Alt+Ctrl+F");
        assert_eq!(keys_for_pc("⌃Space"), "Ctrl+Space");
        assert_eq!(keys_for_pc("⌘ scroll"), "Ctrl+scroll");
        assert_eq!(keys_for_pc("Close (⌘W)"), "Close (Ctrl+W)", "inside a sentence");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!((grouped(0), grouped(999), grouped(1000), grouped(807312), grouped(1234567890)), ("0".into(), "999".into(), "1,000".into(), "807,312".into(), "1,234,567,890".into()));
    }
}
