//! Quick picker for role/warehouse/database/schema: opens instantly with recents and filters as you type.

use rust_i18n::t;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::picker::rank;

use crate::admin_tab::Load;

pub type OnPick = Rc<dyn Fn(String, &mut Window, &mut App)>;

pub struct Picker {
    input: Entity<InputState>,
    all: Load<Vec<String>>,
    recent: Vec<String>,
    current: Option<String>,
    on_pick: OnPick,
    _subs: Vec<Subscription>,
}

impl Picker {
    pub fn new(all: Load<Vec<String>>, recent: Vec<String>, current: Option<String>, on_pick: OnPick, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("picker.search").to_string()));
        let subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => cx.notify(),
            // Enter picks the first of the visible list.
            InputEvent::PressEnter { .. } => {
                if let Some(first) = this.visible(cx).first().cloned() {
                    this.pick(first, window, cx);
                }
            }
            _ => {}
        })];
        Picker { input, all, recent, current, on_pick, _subs: subs }
    }

    pub fn input(&self) -> Entity<InputState> {
        self.input.clone()
    }

    /// The full list arrived (or was refreshed) in the background.
    pub fn set_list(&mut self, all: Load<Vec<String>>, cx: &mut Context<Self>) {
        self.all = all;
        cx.notify();
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }

    /// No search: recents first. With search: ranking over the whole list.
    fn visible(&self, cx: &App) -> Vec<String> {
        let q = self.query(cx);
        match &self.all {
            Load::Ready(all) if !q.is_empty() => rank(all, &q).into_iter().map(String::from).collect(),
            _ if q.is_empty() && !self.recent.is_empty() => self.recent.clone(),
            Load::Ready(all) => all.clone(),
            _ => Vec::new(),
        }
    }

    fn pick(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        (self.on_pick)(name, window, cx);
    }
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let q = self.query(cx);
        let row = |id: ElementId, name: String, current: bool, cx: &mut Context<Self>| {
            let n = name.clone();
            h_flex()
                .id(id)
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .font_family(theme.mono_font_family.clone())
                .hover(|d| d.bg(theme.secondary_hover))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                .when(current, |d| d.child(div().text_color(theme.primary).child("✓")))
                .on_click(cx.listener(move |p, _, w, cx| p.pick(n.clone(), w, cx)))
        };
        let label = |s: String| div().px_2().pt_2().text_xs().text_color(theme.muted_foreground).child(s.to_uppercase());
        let mut out = v_flex().gap_1().child(Input::new(&self.input));

        if q.is_empty() && !self.recent.is_empty() {
            out = out.child(label(t!("picker.recent").to_string()));
            for (i, r) in self.recent.clone().into_iter().enumerate() {
                let cur = self.current.as_deref() == Some(r.as_str());
                out = out.child(row(("recent", i).into(), r, cur, cx));
            }
        }
        match &self.all {
            Load::Loading => out = out.child(div().px_2().py_2().text_sm().text_color(theme.muted_foreground).child(t!("tree.loading").to_string())),
            Load::Failed(m) => out = out.child(div().px_2().py_2().text_sm().text_color(theme.danger).child(m.clone())),
            Load::Ready(all) => {
                let items: Vec<String> = if q.is_empty() { all.clone() } else { rank(all, &q).into_iter().map(String::from).collect() };
                out = out.child(label(if q.is_empty() { t!("picker.all", n = items.len()).to_string() } else { t!("picker.matches", n = items.len()).to_string() }));
                if items.is_empty() {
                    out = out.child(div().px_2().text_sm().text_color(theme.muted_foreground).child(t!("picker.none").to_string()));
                } else {
                    let current = self.current.clone();
                    // Virtualized: the account may have thousands of roles.
                    out = out.child(
                        uniform_list("picker-list", items.len(), cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                            let theme = cx.theme().clone();
                            range
                                .map(|i| {
                                    let name = items[i].clone();
                                    let cur = current.as_deref() == Some(name.as_str());
                                    let n = name.clone();
                                    h_flex()
                                        .id(("pick", i))
                                        .gap_2()
                                        .px_2()
                                        .py_1()
                                        .rounded(theme.radius)
                                        .cursor_pointer()
                                        .text_sm()
                                        .font_family(theme.mono_font_family.clone())
                                        .hover(|d| d.bg(theme.secondary_hover))
                                        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                                        .when(cur, |d| d.child(div().text_color(theme.primary).child("✓")))
                                        .on_click(cx.listener(move |p, _, w, cx| p.pick(n.clone(), w, cx)))
                                })
                                .collect()
                        }))
                        .h(px(320.)),
                    );
                }
            }
        }
        out
    }
}
