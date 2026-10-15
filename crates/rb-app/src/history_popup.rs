//! Histórico (⌘Y): queries já executadas na conexão atual, com busca.

use rust_i18n::t;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::history::{self, Entry};

use crate::{QuickNext, QuickPrev};

/// (sql, abrir em aba nova)
type OnPick = Rc<dyn Fn(String, bool, &mut Window, &mut App)>;

const MAX_HITS: usize = 200;

/// "hoje 21:14", "ontem 18:40" ou "2026-10-05 09:12", no fuso de `tz`.
fn when_label<Tz: chrono::TimeZone>(ts: i64, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (Some(at), Some(today)) = (tz.timestamp_millis_opt(ts).single(), tz.timestamp_millis_opt(now).single()) else { return String::new() };
    let time = at.format("%H:%M").to_string();
    match today.date_naive().signed_duration_since(at.date_naive()).num_days() {
        0 => t!("history.today", time = time).to_string(),
        1 => t!("history.yesterday", time = time).to_string(),
        _ => at.format("%Y-%m-%d %H:%M").to_string(),
    }
}

pub struct HistoryPopup {
    input: Entity<InputState>,
    all: Vec<Entry>,
    hits: Vec<Entry>,
    selected: usize,
    loaded: bool,
    on_pick: OnPick,
    scroll: ScrollHandle,
    _load: Task<()>,
    _subs: Vec<Subscription>,
}

impl HistoryPopup {
    fn new(path: PathBuf, on_pick: OnPick, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.placeholder").to_string()));
        let subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => this.search(cx),
            InputEvent::PressEnter { secondary, .. } => this.pick(this.selected, *secondary, window, cx),
            _ => {}
        })];
        let load = cx.spawn(async move |this, cx| {
            let all = cx.background_spawn(async move { history::load(&path) }).await;
            this.update(cx, |h, cx| {
                h.all = all;
                h.loaded = true;
                h.search(cx);
            })
            .ok();
        });
        HistoryPopup { input, all: Vec::new(), hits: Vec::new(), selected: 0, loaded: false, on_pick, scroll: ScrollHandle::new(), _load: load, _subs: subs }
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        self.hits = history::search(&self.all, &text).into_iter().take(MAX_HITS).cloned().collect();
        self.selected = 0;
        cx.notify();
    }

    fn pick(&mut self, i: usize, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.hits.get(i) {
            let sql = e.sql.clone();
            window.close_dialog(cx);
            (self.on_pick)(sql, new_tab, window, cx);
        }
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.hits.is_empty() {
            return;
        }
        self.selected = if down { (self.selected + 1).min(self.hits.len() - 1) } else { self.selected.saturating_sub(1) };
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }
}

impl Render for HistoryPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let now = history::now_ms();
        let rows = self.hits.iter().enumerate().map(|(i, e)| {
            let head = crate::sql_tab::sql_head(&e.sql);
            let mut info = format!("{} · {} ms", when_label(e.ts, now, &chrono::Local), e.ms);
            if let Some(n) = e.rows {
                info += &format!(" · {}", t!("grid.rows", n = n));
            }
            v_flex()
                .id(("hist", i))
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .when(i == self.selected, |d| d.bg(theme.secondary))
                .hover(|d| d.bg(theme.secondary_hover))
                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family("monospace").child(head))
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(info)
                        .when(e.error.is_some(), |d| d.child(div().text_color(theme.danger).child(t!("history.error").to_string()))),
                )
                .on_click(cx.listener(move |h, ev: &ClickEvent, window, cx| h.pick(i, ev.modifiers().platform, window, cx)))
        });
        let status = if !self.loaded {
            t!("tree.loading").to_string()
        } else if self.hits.is_empty() {
            t!("history.empty").to_string()
        } else {
            t!("history.hint").to_string()
        };
        v_flex()
            .key_context("QuickOpen")
            .gap_2()
            .on_action(cx.listener(|h, _: &QuickPrev, _, cx| h.step(false, cx)))
            .on_action(cx.listener(|h, _: &QuickNext, _, cx| h.step(true, cx)))
            .child(Input::new(&self.input))
            // As linhas são filhas diretas do contêiner rolável: `scroll_to_item` conta por elas.
            .child(div().id("hist-list").flex().flex_col().max_h(px(420.)).overflow_y_scroll().track_scroll(&self.scroll).children(rows))
            .child(div().text_sm().text_color(theme.muted_foreground).child(status))
    }
}

/// Abre o popup; `on_pick(sql, aba_nova)` roda depois de fechar.
pub fn open(conn_name: String, path: PathBuf, on_pick: impl Fn(String, bool, &mut Window, &mut App) + 'static, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| HistoryPopup::new(path, Rc::new(on_pick), window, cx));
    let input = view.read(cx).input.clone();
    window.open_dialog(cx, move |dialog, _, _| dialog.title(t!("history.title", conn = conn_name).to_string()).w(px(640.)).child(view.clone()));
    // Depois de abrir: o diálogo foca um handle próprio e guarda o foco anterior para devolver ao fechar.
    input.update(cx, |s, cx| s.focus(window, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn when_label_says_today_yesterday_or_date() {
        let utc = chrono::Utc;
        let now = 1_760_560_000_000; // 2025-10-15 20:26:40 UTC
        assert_eq!(when_label(now - 60_000, now, &utc), t!("history.today", time = "20:25").to_string());
        assert_eq!(when_label(now - 86_400_000, now, &utc), t!("history.yesterday", time = "20:26").to_string());
        assert_eq!(when_label(now - 10 * 86_400_000, now, &utc), "2025-10-05 20:26");
    }
}
