//! Snowflake "Warehouses" tab (read-only): configuration, who uses it, credits and usage.

use rust_i18n::t;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::admin::{self, RoleGraph};
use rb_core::explain::short_count;
use rb_core::session::Session;
use rb_core::warehouse::{self, Suspend, WarehouseInfo, WhGrant, WhUsage};

use crate::Db;
use crate::admin_tab::{Load, Target, explain, human_ms, kv};
use crate::lang::Lang;
use crate::meta::ConnTarget;
use crate::plan_view::PlanView;
use crate::sql_tab::query_id_button;

/// Opens (or focuses) Users & Roles on the clicked role/user.
pub type Goto = Rc<dyn Fn(Target, &mut Window, &mut App)>;

type Reach = (Arc<RoleGraph>, Arc<HashMap<String, Vec<String>>>);

pub struct WarehouseTab {
    conn: ConnTarget,
    /// SHOWs (fast); ACCOUNT_USAGE opens its own sessions, because the actor is serial.
    session: Option<Arc<Session>>,
    filter: Entity<InputState>,
    list: Load<Vec<WarehouseInfo>>,
    selected: Option<String>,
    grants: HashMap<String, Load<Vec<WhGrant>>>,
    credits: HashMap<String, Load<Vec<(String, f64)>>>,
    usage: HashMap<String, Load<WhUsage>>,
    /// Hierarchy + roles of all users: once per tab, for "N users reach it".
    reach: Option<Load<Reach>>,
    users_open: bool,
    goto: Goto,
    /// Bumped on each ↻: results from a previous task are discarded.
    generation: u64,
    _subs: Vec<Subscription>,
}

impl WarehouseTab {
    pub fn new(conn: ConnTarget, goto: Goto, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("wh.filter").to_string()));
        let subs = vec![
            cx.observe_global_in::<Lang>(window, |this, window, cx| {
                this.filter.update(cx, |s, cx| s.set_placeholder(t!("wh.filter").to_string(), window, cx));
            }),
            cx.subscribe_in(&filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut this = WarehouseTab {
            conn,
            session: None,
            filter,
            list: Load::Loading,
            selected: None,
            grants: HashMap::new(),
            credits: HashMap::new(),
            usage: HashMap::new(),
            reach: None,
            users_open: false,
            goto,
            generation: 0,
            _subs: subs,
        };
        this.reload(cx);
        this
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    pub fn title(&self) -> SharedString {
        t!("wh.title").to_string().into()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.list = Load::Loading;
        self.grants.clear();
        self.credits.clear();
        self.usage.clear();
        self.reach = None;
        cx.notify();
        let (conn, existing, engine) = (self.conn.clone(), self.session.clone(), cx.global::<Db>().0.clone());
        cx.spawn(async move |this, cx| {
            let res = async {
                let s = match existing {
                    Some(s) => s,
                    None => Arc::new(engine.connect(&conn.cfg, conn.creds.clone()).await?),
                };
                let list = warehouse::warehouses(&s).await?;
                Ok::<_, rb_core::DbError>((s, list))
            }
            .await;
            this.update(cx, |t, cx| {
                if t.generation != generation {
                    return;
                }
                match res {
                    Ok((s, list)) => {
                        t.session = Some(s);
                        t.list = Load::Ready(list);
                        t.ensure_loaded(cx);
                    }
                    Err(e) => t.list = Load::Failed(explain(&e, "admin.hint_grants")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, name: String, cx: &mut Context<Self>) {
        self.selected = Some(name);
        self.users_open = false;
        self.ensure_loaded(cx);
        cx.notify();
    }

    /// Fetches what's missing for the selected warehouse; each part is cached in the tab.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.selected.clone() else { return };
        let (conn, engine, generation) = (self.conn.clone(), cx.global::<Db>().0.clone(), self.generation);
        if let Some(s) = self.session.clone()
            && !self.grants.contains_key(&name)
        {
            self.grants.insert(name.clone(), Load::Loading);
            let n = name.clone();
            cx.spawn(async move |this, cx| {
                let res = warehouse::wh_grants(&s, &n).await;
                this.update(cx, |t, cx| {
                    if t.generation == generation {
                        t.grants.insert(n, res.map_or_else(|e| Load::Failed(explain(&e, "admin.hint_grants")), Load::Ready));
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        if !self.credits.contains_key(&name) {
            self.credits.insert(name.clone(), Load::Loading);
            self.usage.insert(name.clone(), Load::Loading);
            let (n, conn, engine) = (name.clone(), conn.clone(), engine.clone());
            cx.spawn(async move |this, cx| {
                let s = engine.connect(&conn.cfg, conn.creds.clone()).await.map(Arc::new);
                let credits = match &s {
                    Ok(s) => warehouse::credits(s, &n).await,
                    Err(e) => Err(e.clone()),
                };
                this.update(cx, |t, cx| {
                    if t.generation == generation {
                        t.credits.insert(n.clone(), credits.map_or_else(|e| Load::Failed(usage_error(&e)), Load::Ready));
                        cx.notify();
                    }
                })
                .ok();
                let usage = match &s {
                    Ok(s) => warehouse::usage(s, &n).await,
                    Err(e) => Err(e.clone()),
                };
                this.update(cx, |t, cx| {
                    if t.generation == generation {
                        t.usage.insert(n, usage.map_or_else(|e| Load::Failed(usage_error(&e)), Load::Ready));
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        if self.reach.is_none() {
            self.reach = Some(Load::Loading);
            cx.spawn(async move |this, cx| {
                let res = async {
                    let s = engine.connect(&conn.cfg, conn.creds.clone()).await?;
                    let g = admin::role_graph(&s).await?;
                    let u = warehouse::all_user_grants(&s).await?;
                    Ok::<_, rb_core::DbError>((Arc::new(g), Arc::new(u)))
                }
                .await;
                this.update(cx, |t, cx| {
                    if t.generation == generation {
                        t.reach = Some(res.map_or_else(|e| Load::Failed(usage_error(&e)), Load::Ready));
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// Real plan of a slow query, in a dialog (same view as SQL).
    fn open_plan(&mut self, q: warehouse::SlowQuery, window: &mut Window, cx: &mut Context<Self>) {
        let holder = cx.new(|_| PlanHolder { plan: Load::Loading });
        let (conn, engine) = (self.conn.clone(), cx.global::<Db>().0.clone());
        let h = holder.clone();
        cx.spawn(async move |_, cx| {
            let res = async {
                let s = engine.connect(&conn.cfg, conn.creds.clone()).await?;
                s.actual_plan(&q.query_id, Some(q.elapsed_ms as f64)).await
            }
            .await;
            h.update(cx, |h, cx| {
                h.plan = match res {
                    Ok(p) => Load::Ready(cx.new(|cx| PlanView::new(p, cx))),
                    Err(e) => Load::Failed(format!("{e}\n{}", t!("wh.hint_monitor"))),
                };
                cx.notify();
            });
        })
        .detach();
        window.open_dialog(cx, move |d, _, _| d.title(t!("wh.plan_title").to_string()).w(px(1100.)).child(div().h(px(640.)).child(holder.clone())));
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let q = self.filter.read(cx).value().trim().to_lowercase();
        let mut col = v_flex()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .child(div().flex_1().text_sm().child(t!("wh.title").to_string()))
                    .child(Button::new("wh-reload").label("↻").ghost().small().tooltip(t!("admin.reload").to_string()).on_click(cx.listener(|t, _, _, cx| t.reload(cx)))),
            )
            .child(Input::new(&self.filter));
        let items: Vec<WarehouseInfo> = match &self.list {
            Load::Ready(l) => l.iter().filter(|w| q.is_empty() || w.name.to_lowercase().contains(&q)).cloned().collect(),
            Load::Loading => return col.child(div().text_sm().text_color(theme.muted_foreground).child(t!("tree.loading").to_string())),
            Load::Failed(m) => return col.child(div().text_sm().text_color(theme.danger).child(m.clone())),
        };
        let total = if let Load::Ready(l) = &self.list { l.len() } else { 0 };
        col = col.child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.of", n = items.len(), total = total).to_string()));
        let selected = self.selected.clone();
        col.child(
            uniform_list("wh-list", items.len(), cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                let theme = cx.theme();
                range
                    .map(|i| {
                        let w = items[i].clone();
                        let name = w.name.clone();
                        let active = selected.as_deref() == Some(w.name.as_str());
                        v_flex()
                            .id(("wh-item", i))
                            .px_2()
                            .py_1()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .when(active, |d| d.bg(theme.secondary))
                            .hover(|d| d.bg(theme.secondary_hover))
                            .child(div().text_sm().font_family(theme.mono_font_family.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(w.name.clone()))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(list_meta(&w)))
                            .on_click(cx.listener(move |t, _, _, cx| t.select(name.clone(), cx)))
                    })
                    .collect()
            }))
            .flex_1(),
        )
    }

    fn render_details(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(name) = self.selected.clone() else {
            return div().p_4().text_color(theme.muted_foreground).child(t!("wh.pick").to_string()).into_any_element();
        };
        let Some(w) = (match &self.list {
            Load::Ready(l) => l.iter().find(|w| w.name == name).cloned(),
            _ => None,
        }) else {
            return div().into_any_element();
        };
        let status = match w.state.as_str() {
            "STARTED" => Tag::success(),
            "RESIZING" | "SUSPENDING" => Tag::warning(),
            _ => Tag::secondary(),
        }
        .small()
        .child(w.state.to_lowercase());
        let header = h_flex()
            .gap_3()
            .items_center()
            .flex_wrap()
            .child(div().text_xl().font_family(theme.mono_font_family.clone()).child(w.name.clone()))
            .child(status)
            .child(div().text_sm().text_color(theme.muted_foreground).child(t!("wh.running_queued", running = w.running, queued = w.queued).to_string()));

        let suspend = match warehouse::suspend_of(&w) {
            Suspend::Never => t!("wh.never").to_string(),
            Suspend::Secs(s) if s < 60 => format!("{s} s"),
            Suspend::Secs(s) if s < 3600 => format!("{} min", s / 60),
            Suspend::Secs(s) => format!("{} h", s / 3600),
        };
        let yes_no = |b: bool| if b { t!("wh.yes") } else { t!("wh.no") }.to_string();
        let config = v_flex()
            .child(section(t!("wh.config").to_string(), &theme))
            .child(kv(t!("wh.size").to_string(), Some(w.size.clone()), &theme))
            .child(kv(t!("wh.type").to_string(), w.kind.clone(), &theme))
            .child(kv(t!("wh.auto_suspend").to_string(), Some(suspend), &theme))
            .child(kv(t!("wh.auto_resume").to_string(), Some(yes_no(w.auto_resume)), &theme))
            .when_some(warehouse::clusters_label(&w), |d, c| d.child(kv(t!("wh.clusters").to_string(), Some(c), &theme)))
            .child(kv(t!("admin.owner").to_string(), w.owner.clone(), &theme))
            .when_some(w.comment.clone(), |d, c| d.child(kv(t!("admin.comment").to_string(), Some(c), &theme)))
            .when_some(w.resource_monitor.clone(), |d, m| d.child(kv(t!("wh.monitor").to_string(), Some(m), &theme)))
            .child(kv(t!("admin.created").to_string(), w.created_on.clone(), &theme))
            .child(kv(t!("wh.resumed").to_string(), w.resumed_on.clone(), &theme));

        let left = v_flex().flex_grow(1.).flex_basis(px(420.)).min_w_0().gap_6().child(config).child(self.render_access(&name, cx));
        let right = v_flex().flex_grow(1.).flex_basis(px(420.)).min_w_0().gap_6().child(self.render_credits(&name, cx)).child(self.render_usage(&name, window, cx));
        v_flex()
            .id("wh-details")
            .size_full()
            .p_4()
            .gap_6()
            .overflow_y_scroll()
            .child(header)
            .child(h_flex().items_start().flex_wrap().gap_10().child(left).child(right))
            .into_any_element()
    }

    /// Roles with a privilege on the warehouse and how many users reach it via inheritance.
    fn render_access(&mut self, name: &str, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let mut out = v_flex().gap_2().child(section(t!("wh.who").to_string(), &theme));
        let grants = match self.grants.get(name) {
            Some(Load::Ready(g)) => g.clone(),
            Some(Load::Failed(m)) => return out.child(div().text_sm().text_color(theme.danger).child(m.clone())),
            _ => return out.child(t!("tree.loading").to_string()),
        };
        for (gi, (privilege, roles)) in warehouse::grants_by_privilege(&grants).into_iter().enumerate() {
            let mut row = h_flex().flex_wrap().gap_1().child(div().w(px(90.)).text_xs().text_color(theme.muted_foreground).child(privilege));
            for (ri, r) in roles.into_iter().enumerate() {
                let goto = self.goto.clone();
                let target = Target::Role(r.clone());
                row = row.child(div().id(("wh-role", gi * 10_000 + ri)).cursor_pointer().child(Tag::secondary().small().child(r)).on_click(move |_, w, cx| goto(target.clone(), w, cx)));
            }
            out = out.child(row);
        }
        // Who uses = USAGE or OWNERSHIP (OPERATE/MONITOR alone can't run queries).
        let usable: Vec<String> = grants.iter().filter(|g| g.privilege == "USAGE" || g.privilege == "OWNERSHIP").map(|g| g.role.clone()).collect();
        match &self.reach {
            Some(Load::Ready((graph, users))) => {
                let who = warehouse::users_reaching(&usable, graph, users);
                let open = self.users_open;
                out = out.child(
                    Button::new("wh-users")
                        .label(format!("{} {}", if open { "▾" } else { "▸" }, t!("wh.users_reach", n = who.len())))
                        .ghost()
                        .small()
                        .on_click(cx.listener(|t, _, _, cx| {
                            t.users_open = !t.users_open;
                            cx.notify();
                        })),
                );
                if open {
                    let mut list = h_flex().flex_wrap().gap_1();
                    for (i, u) in who.into_iter().enumerate() {
                        let goto = self.goto.clone();
                        let target = Target::User(u.clone());
                        list = list.child(div().id(("wh-user", i)).cursor_pointer().child(Tag::info().small().child(u)).on_click(move |_, w, cx| goto(target.clone(), w, cx)));
                    }
                    out = out.child(list);
                }
            }
            Some(Load::Failed(m)) => out = out.child(div().text_xs().text_color(theme.danger).child(m.clone())),
            _ => out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("wh.counting_users").to_string())),
        }
        out
    }

    fn render_credits(&mut self, name: &str, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let out = v_flex().gap_2().child(section(t!("wh.credits").to_string(), &theme));
        let days = match self.credits.get(name) {
            Some(Load::Ready(d)) => d.clone(),
            Some(Load::Failed(m)) => return out.child(div().text_sm().text_color(theme.danger).child(m.clone())),
            _ => return out.child(t!("tree.loading").to_string()),
        };
        let max = days.iter().map(|(_, c)| *c).fold(0.0, f64::max);
        let total: f64 = days.iter().map(|(_, c)| c).sum();
        let mut strip = h_flex().gap(px(3.));
        for (i, (day, c)) in days.iter().enumerate() {
            let tip = t!("wh.credits_day", day = day, n = format!("{c:.2}")).to_string();
            let color = if *c <= 0.0 { theme.secondary } else { theme.primary.opacity((0.25 + 0.75 * c / max.max(1e-9)) as f32) };
            strip = strip.child(div().id(("wh-day", i)).flex_1().h(px(28.)).rounded(px(3.)).bg(color).tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx)));
        }
        out.child(strip).child(div().text_sm().text_color(theme.muted_foreground).child(t!("wh.credits_total", n = format!("{total:.2}")).to_string()))
    }

    fn render_usage(&mut self, name: &str, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let mut out = v_flex()
            .gap_2()
            .child(h_flex().items_baseline().child(section(t!("wh.usage").to_string(), &theme)).child(div().flex_1()).child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.activity_lag").to_string())));
        let u = match self.usage.get(name) {
            Some(Load::Ready(u)) => u.clone(),
            Some(Load::Failed(m)) => return out.child(div().text_sm().text_color(theme.danger).child(m.clone())),
            _ => return out.child(t!("tree.loading").to_string()),
        };
        let row = |k: String, v: String| h_flex().justify_between().text_sm().child(div().text_color(theme.muted_foreground).child(k)).child(div().font_family(theme.mono_font_family.clone()).child(v));

        out = out.child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("wh.top_users").to_string()));
        for (i, x) in u.top_users.iter().enumerate() {
            let goto = self.goto.clone();
            let target = Target::User(x.name.clone());
            out = out.child(
                h_flex()
                    .id(("wh-top", i))
                    .justify_between()
                    .px_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_sm()
                    .hover(|d| d.bg(theme.secondary_hover))
                    .child(x.name.clone())
                    .child(div().text_color(theme.muted_foreground).child(t!("wh.user_line", n = x.queries, time = human_ms(x.elapsed_ms)).to_string()))
                    .on_click(move |_, w, cx| goto(target.clone(), w, cx)),
            );
        }

        out = out.child(div().pt_2().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("wh.queue_spill").to_string()));
        let pct = if u.total > 0 { u.queued as f64 * 100.0 / u.total as f64 } else { 0.0 };
        out = out
            .child(row(t!("wh.queued_queries").to_string(), format!("{} ({pct:.1}%)", u.queued)))
            .child(row(t!("wh.queue_avg_max").to_string(), format!("{} / {}", human_ms(u.avg_queue_ms as u64), human_ms(u.max_queue_ms as u64))))
            .child(row(t!("plan.spilled_local").to_string(), format!("{}B", short_count(u.spilled_local as f64))))
            .child(row(t!("plan.spilled_remote").to_string(), format!("{}B", short_count(u.spilled_remote as f64))))
            .when(warehouse::undersized(&u), |d| d.child(div().text_sm().text_color(theme.warning).child(t!("wh.undersized").to_string())));

        out = out.child(div().pt_2().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("wh.slowest").to_string()));
        for (i, q) in u.slow.iter().enumerate() {
            let sq = q.clone();
            out = out.child(
                v_flex()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().text_xs().font_family(theme.mono_font_family.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(q.text.split_whitespace().collect::<Vec<_>>().join(" ")))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(q.user.clone())
                            .child(human_ms(q.elapsed_ms))
                            .child(q.start.clone())
                            .child(div().flex_1())
                            .child(query_id_button(("wh-qid", i), q.query_id.clone()))
                            .child(Button::new(("wh-plan", i)).label(t!("plan.tab").to_string()).ghost().xsmall().on_click(cx.listener(move |t, _, w, cx| t.open_plan(sq.clone(), w, cx)))),
                    ),
            );
        }
        out
    }
}

/// `XS · running · 3 queries` / `XS · suspended`.
fn list_meta(w: &WarehouseInfo) -> String {
    match w.state.as_str() {
        "STARTED" => t!("wh.meta_running", size = w.size, n = w.running).to_string(),
        s => format!("{} · {}", w.size, s.to_lowercase()),
    }
}

fn section(title: String, theme: &gpui_kit::component::Theme) -> Div {
    div().pt_1().pb_1().text_xs().text_color(theme.muted_foreground).child(title.to_uppercase())
}

/// Without an active warehouse ACCOUNT_USAGE doesn't run; without IMPORTED PRIVILEGES it doesn't even exist.
fn usage_error(e: &rb_core::DbError) -> String {
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("no active warehouse") {
        format!("{msg}\n{}", t!("admin.hint_warehouse"))
    } else {
        explain(e, "admin.hint_activity")
    }
}

/// Plan dialog content: loads in the background.
struct PlanHolder {
    plan: Load<Entity<PlanView>>,
}

impl Render for PlanHolder {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.plan {
            Load::Ready(p) => div().size_full().child(p.clone()).into_any_element(),
            Load::Loading => div().p_4().text_color(cx.theme().muted_foreground).child(t!("plan.loading_actual").to_string()).into_any_element(),
            Load::Failed(m) => div().p_4().text_color(cx.theme().danger).child(m.clone()).into_any_element(),
        }
    }
}

impl Render for WarehouseTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().size_full().child(self.render_list(cx)).child(div().flex_1().min_w_0().h_full().child(self.render_details(window, cx)))
    }
}
