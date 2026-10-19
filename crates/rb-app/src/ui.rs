//! Pieces shared by the screens: background-loaded values, relative times and section titles.

use gpui_kit::component::Theme;
use gpui_kit::prelude::*;
use gpui_kit::{Div, div};
use rb_core::user_view::{self, Span};
use rust_i18n::t;

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

/// "3 months ago" from a Unix second.
pub fn ago(secs: i64) -> String {
    t!("admin.ago", ago = since(secs)).to_string()
}

/// Small uppercase title over a group of rows.
pub fn section(title: String, theme: &Theme) -> Div {
    div().pt_3().pb_1().text_xs().text_color(theme.muted_foreground).child(title.to_uppercase())
}
