//! Full value of a grid cell, on hover: stays open when the pointer moves in, copies the whole
//! value, and shows JSON coloured with indentation guides, foldable blocks and a search.

use std::collections::HashSet;
use std::ops::Range;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::decode::{JsonOutline, JsonTok, detail_text, json_line_spans, json_outline, json_visible};
use rust_i18n::t;

/// Text shown in the card; the copy button always takes the whole value.
const MAX_SHOWN_CHARS: usize = 6_000;
/// One indentation level of `detail_text`'s pretty JSON (2 spaces of the mono font).
const INDENT: f32 = 14.4;
/// Column for the ▾/▸ of foldable lines.
const FOLD_GUTTER: f32 = 12.;
const MAX_HEIGHT: f32 = 480.;

/// Card height for `lines` lines (header, padding, ~16px a line), fixed from the start: folding or
/// filtering must not shrink the card out from under the pointer, which would close it.
fn card_height(lines: usize) -> f32 {
    (40. + 16. + lines as f32 * 16.).min(MAX_HEIGHT)
}

pub struct CellDetail {
    full: String,
    shown: SharedString,
    json: bool,
    outline: JsonOutline,
    /// Lines (that open a block) shown closed.
    folded: HashSet<usize>,
    search: Entity<InputState>,
    /// Which match the search is on, and whether the body should scroll to it on the next frame.
    current: usize,
    jump: bool,
    scroll: ScrollHandle,
    height: f32,
    copied: bool,
    _sub: Subscription,
}

impl CellDetail {
    pub fn new(full: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let shown = detail_text(&full, MAX_SHOWN_CHARS);
        let json = shown.starts_with('{') || shown.starts_with('[');
        let outline = if json { json_outline(&shown.lines().collect::<Vec<_>>()) } else { JsonOutline::default() };
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("grid.search_value").to_string()));
        // Typing goes to the first match; ↵ / ⇧↵ to the next / previous one.
        let sub = cx.subscribe(&search, |this: &mut Self, _, ev: &InputEvent, cx| {
            match ev {
                InputEvent::Change => this.current = 0,
                InputEvent::PressEnter { shift, .. } => {
                    let n = this.hits(cx).len().max(1);
                    this.current = if *shift { (this.current + n - 1) % n } else { (this.current + 1) % n };
                }
                _ => return,
            }
            this.jump = true;
            cx.notify();
        });
        let height = card_height(shown.lines().count());
        CellDetail { full, shown: shown.into(), json, outline, folded: HashSet::new(), search, current: 0, jump: false, scroll: ScrollHandle::new(), height, copied: false, _sub: sub }
    }

    /// Text of a line without its indentation (JSON) and how deep it is.
    fn split<'a>(&self, line: &'a str) -> (usize, &'a str) {
        if !self.json {
            return (0, line);
        }
        let body = line.trim_start_matches(' ');
        ((line.len() - body.len()) / 2, body)
    }

    /// Every match of the search, in reading order: (line, range in the line's unindented text).
    fn hits(&self, cx: &App) -> Vec<(usize, Range<usize>)> {
        let q = self.search.read(cx).value().trim().to_string();
        self.shown.lines().enumerate().flat_map(|(i, l)| matches(self.split(l).1, &q).into_iter().map(move |r| (i, r))).collect()
    }
}

/// Where `q` occurs in `line`, ignoring case (ASCII folding keeps byte offsets valid).
fn matches(line: &str, q: &str) -> Vec<Range<usize>> {
    if q.is_empty() {
        return Vec::new();
    }
    let (hay, q) = (line.to_ascii_lowercase(), q.to_ascii_lowercase());
    hay.match_indices(&q).map(|(i, m)| i..i + m.len()).collect()
}

impl Render for CellDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let style = |c: Hsla| HighlightStyle { color: Some(c), ..Default::default() };
        let colour = |t: JsonTok| match t {
            JsonTok::Key => style(theme.info),
            JsonTok::Str => style(theme.success),
            JsonTok::Num => style(theme.warning),
            JsonTok::Lit => style(theme.danger),
        };
        let found = HighlightStyle { background_color: Some(theme.warning.opacity(0.3)), ..Default::default() };
        let here = HighlightStyle { background_color: Some(theme.warning.opacity(0.8)), color: Some(theme.background), ..Default::default() };
        let query = self.search.read(cx).value().trim().to_string();
        let all: Vec<&str> = self.shown.lines().collect();
        let hits = self.hits(cx);
        let current = hits.get(self.current.min(hits.len().saturating_sub(1))).cloned();
        // The blocks around the current match open, so it can be seen.
        let mut folded = self.folded.clone();
        let mut up = current.as_ref().and_then(|(l, _)| self.outline.parent.get(*l).copied().flatten());
        while let Some(p) = up {
            folded.remove(&p);
            up = self.outline.parent[p];
        }
        let visible = if self.json { json_visible(&all, &self.outline, &folded, "") } else { (0..all.len()).collect() };
        if self.jump
            && let Some(pos) = current.as_ref().and_then(|(l, _)| visible.iter().position(|v| v == l))
        {
            self.scroll.scroll_to_item(pos);
        }
        self.jump = false;
        let rows = visible.into_iter().map(|i| {
            let (depth, rest) = self.split(all[i]);
            let close = self.outline.close.get(i).copied().flatten();
            let folded = close.is_some() && folded.contains(&i);
            // Folded: `"action": { … },` — the close line's text follows the opener.
            let text = match close.filter(|_| folded) {
                Some(c) => format!("{rest} … {}", all[c].trim()),
                None => rest.to_string(),
            };
            let hits = matches(&text, &query);
            let mut marks: Vec<(Range<usize>, HighlightStyle)> = if self.json {
                json_line_spans(&text).into_iter().filter(|(r, _)| !hits.iter().any(|h| h.start < r.end && r.start < h.end)).map(|(r, t)| (r, colour(t))).collect()
            } else {
                Vec::new()
            };
            marks.extend(hits.into_iter().map(|r| {
                let is_here = current.as_ref().is_some_and(|(l, c)| *l == i && *c == r);
                (r, if is_here { here } else { found })
            }));
            marks.sort_by_key(|(r, _)| r.start);
            let arrow = close.map(|_| if folded { "▸" } else { "▾" });
            h_flex()
                .id(("json-line", i))
                .items_start()
                .when(close.is_some(), |d| {
                    d.cursor_pointer().hover(|s| s.bg(theme.accent)).on_click(cx.listener(move |this, _, _, cx| {
                        if !this.folded.remove(&i) {
                            this.folded.insert(i);
                        }
                        cx.notify();
                    }))
                })
                .children((0..depth).map(|_| div().w(px(INDENT)).flex_shrink_0().self_stretch().border_l_1().border_color(theme.border)))
                .when(self.json, |d| d.child(div().w(px(FOLD_GUTTER)).flex_shrink_0().text_color(theme.muted_foreground).children(arrow)))
                .child(div().flex_1().min_w_0().child(StyledText::new(text).with_highlights(marks)))
        });
        let summary = match (query.is_empty(), hits.len()) {
            (true, _) => format!("{}{}", if self.json { "JSON · " } else { "" }, t!("grid.chars", n = crate::ui::grouped(self.full.chars().count() as u64))),
            (false, 0) => t!("grid.no_match").to_string(),
            (false, n) => crate::ui::keys(&t!("grid.match_of", i = self.current.min(n - 1) + 1, n = n)),
        };
        let copy_label = if self.copied { t!("grid.copied") } else { t!("grid.copy") };
        v_flex()
            // Takes the pointer: the wheel scrolls this card, not the grid behind it (which would
            // move another row under the pointer and swap the card).
            .id("cell-detail")
            .occlude()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .w(px(720.))
            .h(px(self.height))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .text_xs()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(220.)).child(Input::new(&self.search).xsmall().cleanable(true)))
                    .child(div().flex_1().text_color(theme.muted_foreground).child(summary))
                    .child(Button::new("cell-copy").label(copy_label.to_string()).xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(this.full.clone()));
                        this.copied = true;
                        cx.notify();
                    }))),
            )
            .child(div().id("cell-detail-body").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.scroll).p_2().font_family(theme.mono_font_family.clone()).children(rows))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn card_keeps_room_for_all_lines_up_to_the_cap() {
        assert!(super::card_height(1) < super::card_height(10));
        assert_eq!(super::card_height(500), super::MAX_HEIGHT);
    }

    #[test]
    fn search_ignores_case() {
        assert_eq!(super::matches(r#""Name": "chat_NAME""#, "name"), [1..5, 14..18]);
        assert!(super::matches("abc", "").is_empty());
    }
}
