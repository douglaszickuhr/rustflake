//! Busca rápida (⌘K): Snowflake procura no índice local; Postgres consulta o catálogo ao vivo.

use rust_i18n::t;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::quick_open::{Hit, HitKind, pg_search, search_index};

use crate::indexer::Indexer;
use crate::meta::MetaStore;
use crate::{QuickNext, QuickPrev};

type OnPick = Rc<dyn Fn(Hit, &mut Window, &mut App)>;
type OnTab = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Aba aberta oferecida no ⌘K: índice, título e (scripts) o texto, para achar pelo conteúdo.
pub struct OpenTab {
    pub ix: usize,
    pub title: String,
    pub body: Option<String>,
}

/// Abas que casam: (índice, título, detalhe). Texto vazio = todas; senão o título ou uma linha do
/// script contém o texto (sem caixa) — o detalhe é a primeira linha que casou.
fn match_tabs(tabs: &[OpenTab], text: &str) -> Vec<(usize, String, String)> {
    let q = text.to_lowercase();
    tabs.iter()
        .filter_map(|t| {
            if q.is_empty() || t.title.to_lowercase().contains(&q) {
                return Some((t.ix, t.title.clone(), String::new()));
            }
            let line = t.body.as_deref()?.lines().find(|l| l.to_lowercase().contains(&q))?;
            Some((t.ix, t.title.clone(), line.trim().to_string()))
        })
        .collect()
}

pub struct QuickOpen {
    input: Entity<InputState>,
    hits: Vec<Hit>,
    selected: usize,
    status: SharedString,
    meta: Entity<MetaStore>,
    indexer: Option<Entity<Indexer>>,
    on_pick: OnPick,
    tabs: Vec<OpenTab>,
    /// Abas que casam com o texto; vêm antes dos objetos na lista.
    tab_hits: Vec<(usize, String, String)>,
    on_tab: OnTab,
    scroll: ScrollHandle,
    /// Versão do índice da última busca.
    seen: Option<Instant>,
    _search: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl QuickOpen {
    fn new(meta: Entity<MetaStore>, indexer: Option<Entity<Indexer>>, on_pick: OnPick, tabs: Vec<OpenTab>, on_tab: OnTab, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("quick.placeholder").to_string()));
        let mut subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => this.search(cx),
            InputEvent::PressEnter { .. } => this.pick(this.selected, window, cx),
            _ => {}
        })];
        // Índice chegou ou foi atualizado: refaz a busca com o mesmo texto. Só quando muda de fato
        // (o indexador também avisa ao começar um snapshot), para não voltar a seleção ao topo à toa.
        if let Some(ix) = &indexer {
            subs.push(cx.observe(ix, |this, ix, cx| {
                let refreshed = ix.read(cx).index.as_ref().and_then(|i| i.refreshed);
                if refreshed != this.seen {
                    this.seen = refreshed;
                    this.search(cx);
                }
                // Início/fim de snapshot ou erro: o rodapé muda mesmo sem índice novo.
                cx.notify();
            }));
        }
        let seen = indexer.as_ref().and_then(|ix| ix.read(cx).index.as_ref().and_then(|i| i.refreshed));
        QuickOpen {
            input,
            hits: Vec::new(),
            selected: 0,
            status: "".into(),
            meta,
            indexer,
            on_pick,
            tab_hits: match_tabs(&tabs, ""),
            tabs,
            on_tab,
            scroll: ScrollHandle::new(),
            seen,
            _search: None,
            _subs: subs,
        }
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        self.selected = 0;
        self._search = None;
        self.tab_hits = match_tabs(&self.tabs, &text);
        if text.chars().count() < 2 {
            self.hits.clear();
            self.status = "".into();
            cx.notify();
            return;
        }
        match &self.indexer {
            Some(ix) => {
                let ix = ix.read(cx);
                match &ix.index {
                    Some(idx) => {
                        self.hits = search_index(idx, &self.meta.read(cx).cache, &text);
                        self.status = if self.hits.is_empty() && self.tab_hits.is_empty() { t!("quick.no_results").to_string().into() } else { "".into() };
                    }
                    None => {
                        self.hits.clear();
                        self.status = match &ix.error {
                            Some(e) => t!("quick.index_failed", error = e).to_string().into(),
                            None => t!("quick.indexing").to_string().into(),
                        };
                    }
                }
            }
            None => {
                self.status = t!("quick.searching").to_string().into();
                let session = self.meta.read(cx).session.clone();
                // Trocar a task cancela a busca anterior (e a espera de 250 ms).
                self._search = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(250)).await;
                    let res = pg_search(&session, &text).await;
                    this.update(cx, |q, cx| {
                        match res {
                            Ok(hits) => {
                                q.status = if hits.is_empty() && q.tab_hits.is_empty() { t!("quick.no_results").to_string().into() } else { "".into() };
                                q.hits = hits;
                            }
                            Err(e) => q.status = e.to_string().into(),
                        }
                        cx.notify();
                    })
                    .ok();
                }));
            }
        }
        cx.notify();
    }

    fn pick(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((ix, ..)) = self.tab_hits.get(i) {
            let ix = *ix;
            window.close_dialog(cx);
            (self.on_tab)(ix, window, cx);
            return;
        }
        if let Some(h) = self.hits.get(i - self.tab_hits.len()).cloned() {
            window.close_dialog(cx);
            (self.on_pick)(h, window, cx);
        }
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let n = self.tab_hits.len() + self.hits.len();
        if n == 0 {
            return;
        }
        self.selected = if down { (self.selected + 1).min(n - 1) } else { self.selected.saturating_sub(1) };
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }
}

fn icon(h: &Hit) -> IconName {
    match (h.kind, h.matview) {
        (HitKind::Database, _) => IconName::Database,
        (HitKind::Schema, _) => IconName::Layers,
        (HitKind::Table, _) => IconName::Table,
        (HitKind::View, true) => IconName::Sheet,
        (HitKind::View, false) => IconName::Eye,
    }
}

fn kind_label(h: &Hit) -> String {
    match (h.kind, h.matview) {
        (HitKind::Database, _) => t!("quick.kind_database"),
        (HitKind::Schema, _) => t!("quick.kind_schema"),
        (HitKind::Table, _) => t!("quick.kind_table"),
        (HitKind::View, true) => t!("quick.kind_matview"),
        (HitKind::View, false) => t!("quick.kind_view"),
    }
    .to_string()
}

/// Onde o objeto está: `DB.SCHEMA` (o que existir).
fn location(h: &Hit) -> String {
    match h.kind {
        HitKind::Database => String::new(),
        HitKind::Schema => h.database.clone().unwrap_or_default(),
        _ => h.database.iter().chain(&h.schema).cloned().collect::<Vec<_>>().join("."),
    }
}

impl Render for QuickOpen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let skip = self.tab_hits.len();
        let tab_rows = self.tab_hits.iter().enumerate().map(|(i, (_, title, detail))| {
            h_flex()
                .id(("qo-tab", i))
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .when(i == self.selected, |d| d.bg(theme.secondary))
                .hover(|d| d.bg(theme.secondary_hover))
                .child(Icon::new(IconName::File).small().text_color(theme.muted_foreground))
                .child(div().flex_shrink_0().child(title.clone()))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(detail.clone()))
                .child(div().flex_shrink_0().text_sm().text_color(theme.muted_foreground).child(t!("quick.kind_tab").to_string()))
                .on_click(cx.listener(move |q, _, window, cx| q.pick(i, window, cx)))
                .into_any_element()
        });
        let rows = self.hits.iter().enumerate().map(|(i, h)| {
            let i = i + skip;
            let selected = i == self.selected;
            h_flex()
                .id(("qo", i))
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .when(selected, |d| d.bg(theme.secondary))
                .hover(|d| d.bg(theme.secondary_hover))
                .child(Icon::new(icon(h)).small().text_color(theme.muted_foreground))
                .child(div().flex_shrink_0().child(h.name.clone()))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.muted_foreground).child(location(h)))
                .child(div().flex_shrink_0().text_sm().text_color(theme.muted_foreground).child(kind_label(h)))
                .on_click(cx.listener(move |q, _, window, cx| q.pick(i, window, cx)))
                .into_any_element()
        });
        let rows: Vec<AnyElement> = tab_rows.chain(rows).collect();
        let index_info = self.indexer.as_ref().map(|ix| {
            let ix = ix.read(cx);
            let n = ix.index.as_ref().map_or(0, |i| i.objects.len());
            let state = match (ix.running, &ix.error) {
                (true, _) => format!(" · {}", t!("quick.updating")),
                (false, Some(e)) => format!(" · {}", t!("quick.last_update_failed", error = e)),
                (false, None) => String::new(),
            };
            (format!("{}{state}", t!("quick.index_size", n = n)), ix.running)
        });
        v_flex()
            .key_context("QuickOpen")
            .gap_2()
            .on_action(cx.listener(|q, _: &QuickPrev, _, cx| q.step(false, cx)))
            .on_action(cx.listener(|q, _: &QuickNext, _, cx| q.step(true, cx)))
            .child(Input::new(&self.input))
            // As linhas são filhas diretas do contêiner rolável: `scroll_to_item` conta por elas.
            .child(div().id("qo-list").flex().flex_col().max_h(px(420.)).overflow_y_scroll().track_scroll(&self.scroll).children(rows))
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(self.status.clone()))
                    .when_some(index_info, |d, (info, running)| {
                        d.child(info).child(Button::new("qo-refresh").label("↻").ghost().disabled(running).tooltip(t!("quick.refresh_index").to_string()).on_click(cx.listener(
                            |q, _, _, cx| {
                                if let Some(ix) = &q.indexer {
                                    ix.update(cx, |ix, cx| ix.refresh(cx));
                                }
                            },
                        )))
                    }),
            )
    }
}

/// Abre o popup; `on_pick` recebe o objeto escolhido (o popup já fechou).
/// `tabs`: as abas abertas; escolher uma chama `on_tab` com o índice dela.
pub fn open(
    meta: Entity<MetaStore>,
    indexer: Option<Entity<Indexer>>,
    on_pick: impl Fn(Hit, &mut Window, &mut App) + 'static,
    tabs: Vec<OpenTab>,
    on_tab: impl Fn(usize, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| QuickOpen::new(meta, indexer, Rc::new(on_pick), tabs, Rc::new(on_tab), window, cx));
    let input = view.read(cx).input.clone();
    window.open_dialog(cx, move |dialog, _, _| dialog.title(t!("quick.title").to_string()).w(px(640.)).child(view.clone()));
    // Depois de abrir: o diálogo foca um handle próprio e guarda o foco anterior para devolver ao fechar.
    input.update(cx, |s, cx| s.focus(window, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn open_tabs_match_by_title_or_script_text() {
        let tab = |ix, title: &str, body: Option<&str>| OpenTab { ix, title: title.into(), body: body.map(Into::into) };
        let tabs = vec![tab(0, "Prod · daily checks", Some("select 1;\n  update ORDERS set x = 1;")), tab(1, "Prod · USERS", None)];
        assert_eq!(match_tabs(&tabs, "").len(), 2, "vazio: todas");
        assert_eq!(match_tabs(&tabs, "DAILY"), vec![(0, "Prod · daily checks".into(), String::new())]);
        assert_eq!(match_tabs(&tabs, "orders"), vec![(0, "Prod · daily checks".into(), "update ORDERS set x = 1;".into())]);
        assert_eq!(match_tabs(&tabs, "users").len(), 1);
        assert!(match_tabs(&tabs, "nothing").is_empty());
    }
}
