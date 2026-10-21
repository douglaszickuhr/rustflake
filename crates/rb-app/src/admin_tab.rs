//! Snowflake "Users & Roles" tab (read-only).

use rust_i18n::t;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::admin::{self, Activity, DirectGrant, EffectiveRole, Grant, Grantee, RoleGraph, RoleInfo, RoleKind, UserInfo};
use rb_core::masking::{self, Scope, TagCount};
use rb_core::org_config::OrgConfig;
use rb_core::session::Session;
use rb_core::unlocks::{self, Detail, ItemKind, UnlockItem};
use rb_core::user_view::{self, UserMeta};

use crate::Db;
use crate::lang::Lang;
use crate::meta::ConnTarget;
use crate::ui::{Load, ago, section, since, span_label};

#[derive(Clone, PartialEq)]
pub enum Target {
    User(String),
    /// All the user's roles (Users / NAME / Roles).
    UserRoles(String),
    Role(String),
}

/// SHOW returns at most 10,000 rows.
const SHOW_LIMIT: usize = 10_000;

/// One row of the users/roles list.
#[derive(Clone)]
struct ListRow {
    name: String,
    /// Bottom line (last login, counts).
    meta: String,
    /// Highlight (idle for a long time).
    warn: bool,
    dimmed: bool,
}

/// Direct roles and, once known, the inherited ones.
type UserRoles = (Vec<String>, Load<Vec<EffectiveRole>>);

/// (filter, roles?, disabled?, generation, users ready?, roles ready?).
type ListKey = (String, bool, bool, u64, bool, bool);
/// Role → inherited roles, role → databases.
type Graphs = (Arc<RoleGraph>, Arc<RoleGraph>);
/// (roles, databases graph pointer).
type UnlockKey = (Vec<String>, usize);

/// Work that render would otherwise redo every frame (thousands of users or roles).
#[derive(Default)]
struct Caches {
    list: Option<(ListKey, Rc<Vec<ListRow>>)>,
    /// Inheritance/databases rebuilt from SHOW GRANTS, by (generation, grants cached).
    graphs: Option<((u64, usize), Graphs)>,
    unlocks: Option<(UnlockKey, Rc<Vec<UnlockItem>>)>,
}

pub struct AdminTab {
    conn: ConnTarget,
    /// SHOWs (fast).
    session: Option<Arc<Session>>,
    /// ACCOUNT_USAGE (slow): the Snowflake actor is serial, so it lives in another session.
    activity_session: Option<Arc<Session>>,
    show_roles: bool,
    /// Disabled ones stay out of the list until requested.
    show_disabled: bool,
    filter: Entity<InputState>,
    users: Load<Vec<UserInfo>>,
    roles: Load<Vec<RoleInfo>>,
    stack: Vec<Target>,
    /// Direct + effective roles per user.
    user_roles: HashMap<String, Load<UserRoles>>,
    /// Role grants and who holds the role.
    role_details: HashMap<String, Load<RoleDetails>>,
    /// `SHOW GRANTS TO ROLE` per role: inheritance and the role screen use the same one.
    grants_cache: HashMap<String, Vec<Grant>>,
    /// `SHOW GRANTS TO USER`: who granted each direct role (SCIM or by hand).
    direct_grants: HashMap<String, Vec<DirectGrant>>,
    /// role → databases with USAGE (ACCOUNT_USAGE); groups the access roles.
    databases: Arc<RoleGraph>,
    roles_search: Entity<InputState>,
    unlock_filter: Option<ItemKind>,
    unused_only: bool,
    /// Role selected on the roles screen (shows the paths).
    selected: Option<String>,
    /// Open items (show their member roles), by "kind:name".
    item_open: HashSet<String>,
    /// Groups with more than 6 items showing all of them.
    group_more: HashSet<ItemKind>,
    /// Account databases: splits `<DB>_<SCHEMA>` in classification role names.
    db_names: Vec<String>,
    /// Columns with a classification tag per schema; fetched only when someone looks at the classification.
    tag_counts: Option<Load<Arc<Vec<TagCount>>>>,
    activity: HashMap<String, Load<Activity>>,
    /// Role hierarchy via ACCOUNT_USAGE; failed → inheritance falls back to SHOW per role.
    graph: Load<Arc<RoleGraph>>,
    /// Target requested from outside (Warehouses tab) before the lists load.
    pending: Option<Target>,
    /// Bumps on every ↻: results of tasks from before the reload are discarded.
    generation: u64,
    cache: RefCell<Caches>,
    _subs: Vec<Subscription>,
}

type RoleDetails = (Vec<Grant>, Vec<Grantee>);

/// Error message + hint when it's a permission issue.
pub fn explain(e: &rb_core::DbError, hint_key: &str) -> String {
    let msg = e.to_string();
    if admin::privilege_hint(&msg) { format!("{msg}\n{}", t!(hint_key)) } else { msg }
}

impl AdminTab {
    pub fn new(conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("admin.filter").to_string()));
        let roles_search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("admin.search_roles").to_string()));
        let subs = vec![
            cx.observe_global_in::<Lang>(window, |this, window, cx| {
                this.filter.update(cx, |s, cx| s.set_placeholder(t!("admin.filter").to_string(), window, cx));
                this.roles_search.update(cx, |s, cx| s.set_placeholder(t!("admin.search_roles").to_string(), window, cx));
            }),
            cx.subscribe_in(&filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&roles_search, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut this = AdminTab {
            conn,
            session: None,
            activity_session: None,
            show_roles: false,
            show_disabled: false,
            filter,
            users: Load::Loading,
            roles: Load::Loading,
            stack: Vec::new(),
            user_roles: HashMap::new(),
            role_details: HashMap::new(),
            grants_cache: HashMap::new(),
            direct_grants: HashMap::new(),
            databases: Arc::default(),
            roles_search,
            unlock_filter: None,
            unused_only: false,
            selected: None,
            item_open: HashSet::new(),
            group_more: HashSet::new(),
            db_names: Vec::new(),
            tag_counts: None,
            activity: HashMap::new(),
            graph: Load::Loading,
            pending: None,
            generation: 0,
            cache: RefCell::default(),
            _subs: subs,
        };
        this.reload(cx);
        this
    }

    pub fn conn_id(&self) -> &str {
        &self.conn.cfg.id
    }

    pub fn title(&self) -> SharedString {
        t!("admin.title").to_string().into()
    }

    /// Connects (same login: the engine reuses the Snowflake session) and loads the lists.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let (conn, existing, engine) = (self.conn.clone(), self.session.clone(), cx.global::<Db>().0.clone());
        self.users = Load::Loading;
        self.roles = Load::Loading;
        // ↻ brings everything back, not just the lists.
        self.user_roles.clear();
        self.role_details.clear();
        self.grants_cache.clear();
        self.activity.clear();
        self.graph = Load::Loading;
        self.direct_grants.clear();
        self.databases = Arc::default();
        self.tag_counts = None;
        self.generation += 1;
        let generation = self.generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let t0 = std::time::Instant::now();
            let s = match existing {
                Some(s) => Ok(s),
                None => engine.connect(&conn.cfg, conn.creds.clone()).await.map(Arc::new),
            };
            tracing::info!("admin: session in {:?} (ok={})", t0.elapsed(), s.is_ok());
            let s = match s {
                Ok(s) => s,
                Err(e) => {
                    let msg = explain(&e, "admin.hint_grants");
                    this.update(cx, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        (tab.users, tab.roles) = (Load::Failed(msg.clone()), Load::Failed(msg));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            // All three at once, each list shown as soon as it arrives (users, roles, role graph).
            let (this1, this2, this3) = (this.clone(), this.clone(), this.clone());
            let (mut cx1, mut cx2, mut cx3) = (cx.clone(), cx.clone(), cx.clone());
            let users_part = async {
                let users = admin::users(&s).await;
                tracing::info!("admin: SHOW USERS in {:?} ({:?} rows)", t0.elapsed(), users.as_ref().map(Vec::len));
                this1
                    .update(&mut cx1, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        tab.session = Some(s.clone());
                        tab.users = match users {
                            Ok(u) => Load::Ready(u),
                            Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                        };
                        // What was open before the ↻ loads again.
                        tab.ensure_loaded(cx);
                        if let Some(t) = tab.pending.take() {
                            tab.stack.clear();
                            tab.open(t, cx);
                        }
                        cx.notify();
                    })
                    .ok();
            };
            let roles_part = async {
                let (rs, ds) = (s.sibling(), s.sibling());
                let (roles, db_names) = futures::join!(admin::roles(rs.as_ref().unwrap_or(&s)), rb_core::metadata::databases(ds.as_ref().unwrap_or(&s)));
                let db_names = db_names.unwrap_or_default();
                tracing::info!("admin: SHOW ROLES in {:?} ({:?} rows)", t0.elapsed(), roles.as_ref().map(Vec::len));
                this2
                    .update(&mut cx2, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        tab.roles = match roles {
                            Ok(r) => Load::Ready(r),
                            Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                        };
                        tab.db_names = db_names;
                        cx.notify();
                    })
                    .ok();
            };
            let graph_part = async {
                // Own session (same login): the actor is serial and clicks shouldn't wait for ACCOUNT_USAGE.
                let graph = match engine.connect(&conn.cfg, conn.creds.clone()).await {
                    Ok(gs) => match admin::role_graph(&gs).await {
                        // Databases are a complement: if it fails, access roles stay under "unknown database".
                        Ok(g) => Ok((g, admin::role_databases(&gs).await.unwrap_or_default())),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                };
                tracing::info!("admin: role graph in {:?} ({:?} roles)", t0.elapsed(), graph.as_ref().map(|(g, _)| g.len()));
                this3
                    .update(&mut cx3, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        match graph {
                            Ok((g, dbs)) => {
                                let g = Arc::new(g);
                                tab.databases = Arc::new(dbs);
                                // Whoever was waiting for the graph resolves now, locally.
                                for v in tab.user_roles.values_mut() {
                                    if let Load::Ready((d, eff @ Load::Loading)) = v {
                                        *eff = Load::Ready(admin::effective_from_graph(d, &g));
                                    }
                                }
                                tab.graph = Load::Ready(g);
                            }
                            Err(e) => {
                                tracing::info!("admin: no role graph ({e}); inheritance via SHOW per role");
                                tab.graph = Load::Failed(e.to_string());
                                // Whoever was waiting for the graph restarts via the slow path.
                                tab.user_roles.retain(|_, v| !matches!(v, Load::Ready((_, Load::Loading))));
                                tab.ensure_loaded(cx);
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            };
            futures::join!(users_part, roles_part, graph_part);
        })
        .detach();
    }

    /// Navigation from another tab: with the session ready it opens now; otherwise, when the lists arrive.
    pub fn goto(&mut self, t: Target, cx: &mut Context<Self>) {
        if self.session.is_some() {
            self.stack.clear();
            self.open(t, cx);
        } else {
            self.pending = Some(t);
        }
    }

    pub fn open(&mut self, t: Target, cx: &mut Context<Self>) {
        if self.stack.last() != Some(&t) {
            self.stack.push(t);
        }
        self.ensure_loaded(cx);
        cx.notify();
    }

    /// Fetches what's missing for the target at the top of the stack; the result is cached in the tab.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if let Some(Target::User(u) | Target::UserRoles(u)) = self.stack.last().cloned()
            && !self.activity.contains_key(&u)
            && let Load::Ready(us) = &self.users
            && let Some(info) = us.iter().find(|x| x.name == u).cloned()
        {
            self.activity.insert(u.clone(), Load::Loading);
            let (conn, existing, engine) = (self.conn.clone(), self.activity_session.clone(), cx.global::<Db>().0.clone());
            let generation = self.generation;
            cx.spawn(async move |this, cx| {
                let res = async {
                    let s = match existing {
                        Some(s) => s,
                        None => Arc::new(engine.connect(&conn.cfg, conn.creds.clone()).await?),
                    };
                    let a = admin::activity(&s, &info).await;
                    Ok::<_, rb_core::DbError>((s, a?))
                }
                .await;
                this.update(cx, |tab, cx| {
                    if tab.generation != generation {
                        return;
                    }
                    let v = match res {
                        Ok((s, a)) => {
                            tab.activity_session = Some(s);
                            Load::Ready(a)
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            // No active warehouse, ACCOUNT_USAGE can't run: dedicated hint.
                            let hint = if msg.to_ascii_lowercase().contains("warehouse") { "admin.hint_warehouse" } else { "admin.hint_activity" };
                            Load::Failed(if admin::privilege_hint(&msg) || hint == "admin.hint_warehouse" { format!("{msg}\n{}", t!(hint)) } else { msg })
                        }
                    };
                    tab.activity.insert(u, v);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        let Some(s) = self.session.clone() else { return };
        match self.stack.last().cloned() {
            Some(Target::User(u) | Target::UserRoles(u)) if !self.user_roles.contains_key(&u) => {
                self.user_roles.insert(u.clone(), Load::Loading);
                // Only the already-known inheritance (names), not whole grants: SYSADMIN has thousands.
                let mut known: HashMap<String, Vec<String>> = self.grants_cache.iter().map(|(k, g)| (k.clone(), admin::inherited_roles(g))).collect();
                let generation = self.generation;
                cx.spawn(async move |this, cx| {
                    let (direct, grants) = match admin::user_grants(&s, &u).await {
                        Ok(g) => (g.iter().map(|g| g.role.clone()).collect::<Vec<_>>(), g),
                        Err(e) => {
                            let v = Load::Failed(explain(&e, "admin.hint_grants"));
                            this.update(cx, |tab, cx| {
                                if tab.generation == generation {
                                    tab.user_roles.insert(u, v);
                                    cx.notify();
                                }
                            })
                            .ok();
                            return;
                        }
                    };
                    // Direct roles show up right away. With the graph, inheritance appears instantly; while loading, it appears
                    // when it arrives; without the graph, one query per role (slow) just below.
                    let d = direct.clone();
                    let uu = u.clone();
                    let walk = this
                        .update(cx, |tab, cx| {
                            if tab.generation != generation {
                                return false;
                            }
                            let (eff, walk) = match &tab.graph {
                                Load::Ready(g) => (Load::Ready(admin::effective_from_graph(&d, g)), false),
                                Load::Loading => (Load::Loading, false),
                                Load::Failed(_) => (Load::Loading, true),
                            };
                            tab.direct_grants.insert(uu.clone(), grants);
                            tab.user_roles.insert(uu, Load::Ready((d, eff)));
                            cx.notify();
                            walk
                        })
                        .unwrap_or(false);
                    if !walk {
                        return;
                    }
                    let mut fetched: HashMap<String, Vec<Grant>> = HashMap::new();
                    let eff = admin::effective_roles(&direct, async |r: &str| {
                        if let Some(i) = known.get(r) {
                            return Ok(i.clone());
                        }
                        let g = admin::role_grants(&s, r).await?;
                        let i = admin::inherited_roles(&g);
                        known.insert(r.to_string(), i.clone());
                        fetched.insert(r.to_string(), g);
                        Ok::<_, rb_core::DbError>(i)
                    })
                    .await;
                    let eff = match eff {
                        Ok(e) => Load::Ready(e),
                        Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                    };
                    this.update(cx, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        tab.grants_cache.extend(fetched);
                        tab.user_roles.insert(u, Load::Ready((direct, eff)));
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            Some(Target::Role(r)) if !self.role_details.contains_key(&r) => {
                self.role_details.insert(r.clone(), Load::Loading);
                let cached = self.grants_cache.get(&r).cloned();
                let generation = self.generation;
                cx.spawn(async move |this, cx| {
                    let res = async {
                        let g = match cached {
                            Some(g) => g,
                            None => admin::role_grants(&s, &r).await?,
                        };
                        Ok::<_, rb_core::DbError>((g, admin::grantees(&s, &r).await?))
                    }
                    .await;
                    this.update(cx, |tab, cx| {
                        if tab.generation != generation {
                            return;
                        }
                        let v = match res {
                            Ok((g, who)) => {
                                tab.grants_cache.insert(r.clone(), g.clone());
                                Load::Ready((g, who))
                            }
                            Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                        };
                        tab.role_details.insert(r, v);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            _ => {}
        }
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.stack.pop();
        // The cache may have been cleared by a ↻ after this target opened.
        self.ensure_loaded(cx);
        cx.notify();
    }

    /// Rows of the current list that match the filter (contains, case-insensitive), and the list total.
    /// Cached until the filter, the toggles or the lists change. ponytail: "active 3 min ago" doesn't tick meanwhile.
    fn visible(&self, cx: &App) -> Result<(Rc<Vec<ListRow>>, usize), String> {
        let q = self.filter.read(cx).value().trim().to_lowercase();
        let total = match (self.show_roles, &self.users, &self.roles) {
            (false, Load::Ready(u), _) => u.len(),
            (true, _, Load::Ready(r)) => r.len(),
            (false, Load::Failed(e), _) | (true, _, Load::Failed(e)) => return Err(e.clone()),
            _ => return Err(t!("tree.loading").to_string()),
        };
        let ready = (matches!(self.users, Load::Ready(_)), matches!(self.roles, Load::Ready(_)));
        let key = (q, self.show_roles, self.show_disabled, self.generation, ready.0, ready.1);
        if let Some((k, rows)) = &self.cache.borrow().list
            && *k == key
        {
            return Ok((rows.clone(), total));
        }
        let q = &key.0;
        let keep = |s: &str| q.is_empty() || s.to_lowercase().contains(q.as_str());
        let now = user_view::now();
        let rows: Vec<ListRow> = match (self.show_roles, &self.users, &self.roles) {
            (false, Load::Ready(u), _) => u
                .iter()
                .filter(|u| self.show_disabled || !u.disabled)
                .filter(|u| keep(&u.name) || u.display_name.as_deref().is_some_and(keep) || u.email.as_deref().is_some_and(keep))
                .map(|u| {
                    let (meta, warn) = meta_text(user_view::user_meta(u, now));
                    ListRow { name: u.name.clone(), meta, warn, dimmed: u.disabled }
                })
                .collect(),
            (true, _, Load::Ready(r)) => r
                .iter()
                .filter(|r| keep(&r.name))
                .map(|r| ListRow { name: r.name.clone(), meta: t!("admin.role_meta", users = r.assigned_to_users, roles = r.granted_to_roles).to_string(), warn: false, dimmed: false })
                .collect(),
            _ => Vec::new(),
        };
        let rows = Rc::new(rows);
        self.cache.borrow_mut().list = Some((key, rows.clone()));
        Ok((rows, total))
    }

    /// What `roles` unlock; cached for the last set (the user page redraws often).
    fn unlocks_of(&self, roles: Vec<String>, dbs: &Arc<RoleGraph>) -> Rc<Vec<UnlockItem>> {
        let key = (roles, Arc::as_ptr(dbs) as usize);
        if let Some((k, items)) = &self.cache.borrow().unlocks
            && *k == key
        {
            return items.clone();
        }
        let items = Rc::new(unlocks::unlock_items(&self.conn.org, &key.0, dbs));
        self.cache.borrow_mut().unlocks = Some((key, items.clone()));
        items
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let items = self.visible(cx);
        let truncated = match (self.show_roles, &self.users, &self.roles) {
            (false, Load::Ready(u), _) => u.len() >= SHOW_LIMIT,
            (true, _, Load::Ready(r)) => r.len() >= SHOW_LIMIT,
            _ => false,
        };
        let show_roles = self.show_roles;
        v_flex()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1()
                    .child(Button::new("adm-users").label(t!("admin.users").to_string()).small().selected(!show_roles).on_click(cx.listener(|t, _, _, cx| {
                        t.show_roles = false;
                        cx.notify();
                    })))
                    .child(Button::new("adm-roles").label(t!("admin.roles").to_string()).small().selected(show_roles).on_click(cx.listener(|t, _, _, cx| {
                        t.show_roles = true;
                        cx.notify();
                    })))
                    .child(div().flex_1())
                    .child(Button::new("adm-reload").label("↻").ghost().small().tooltip(t!("admin.reload").to_string()).on_click(cx.listener(|t, _, _, cx| t.reload(cx)))),
            )
            .child(Input::new(&self.filter).small().cleanable(true))
            .child(
                h_flex()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match &items {
                        Ok((v, total)) => t!("admin.of", n = v.len(), total = total).to_string(),
                        Err(_) => String::new(),
                    })
                    .when(!show_roles, |d| {
                        d.child(Checkbox::new("adm-disabled").label(t!("admin.show_disabled").to_string()).small().checked(self.show_disabled).on_click(cx.listener(|t, checked: &bool, _, cx| {
                            t.show_disabled = *checked;
                            cx.notify();
                        })))
                    }),
            )
            .when(truncated, |d| d.child(div().text_xs().text_color(theme.warning).child(t!("admin.truncated", n = SHOW_LIMIT).to_string())))
            .child(match items {
                Err(msg) => div().text_sm().text_color(theme.muted_foreground).child(msg).into_any_element(),
                Ok((items, _)) => {
                    let n = items.len();
                    let selected = self.stack.last().cloned();
                    uniform_list(
                        "adm-list",
                        n,
                        cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                            let theme = cx.theme();
                            range
                                .map(|i| {
                                    let ListRow { name, meta, warn, dimmed: disabled } = items[i].clone();
                                    let target = if show_roles { Target::Role(name.clone()) } else { Target::User(name.clone()) };
                                    let active = selected.as_ref() == Some(&target);
                                    h_flex()
                                        .id(("adm-item", i))
                                        .gap_2()
                                        .px_2()
                                        .py_1()
                                        .rounded(theme.radius)
                                        .text_sm()
                                        .cursor_pointer()
                                        .when(active, |d| d.bg(theme.secondary))
                                        .hover(|d| d.bg(theme.secondary_hover))
                                        .when(disabled, |d| d.text_color(theme.muted_foreground))
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().font_family(theme.mono_font_family.clone()).child(name))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .overflow_hidden()
                                                        .text_ellipsis()
                                                        .whitespace_nowrap()
                                                        .text_color(if warn { theme.warning } else { theme.muted_foreground })
                                                        .child(meta),
                                                ),
                                        )
                                        .on_click(cx.listener(move |t, _, _, cx| {
                                            // Clicking the list restarts navigation.
                                            t.stack.clear();
                                            t.open(target.clone(), cx);
                                        }))
                                })
                                .collect()
                        }),
                    )
                    .flex_1()
                    .into_any_element()
                }
            })
    }

    fn render_details(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(top) = self.stack.last().cloned() else {
            return div().p_4().text_color(cx.theme().muted_foreground).child(t!("admin.pick").to_string()).into_any_element();
        };
        let header = h_flex()
            .gap_2()
            .when(self.stack.len() > 1, |d| {
                d.child(Button::new("adm-back").icon(IconName::ArrowLeft).ghost().small().tooltip(t!("admin.back").to_string()).on_click(cx.listener(|t, _, _, cx| t.back(cx))))
            })
            .when_some(
                match &top {
                    Target::Role(r) => Some(r.clone()),
                    Target::UserRoles(u) => Some(format!("{u} / {}", t!("admin.roles"))),
                    Target::User(_) => None,
                },
                |d, r| d.child(div().text_lg().child(r)),
            );
        let body = match top {
            Target::User(u) => self.render_user(&u, cx),
            Target::UserRoles(u) => self.render_user_roles(&u, cx),
            Target::Role(r) => self.render_role(&r, cx),
        };
        v_flex().id("adm-details").size_full().p_4().overflow_y_scroll().child(header).child(body).into_any_element()
    }

    fn render_user(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(u) = (match &self.users {
            Load::Ready(us) => us.iter().find(|u| u.name == name).cloned(),
            _ => None,
        }) else {
            return div().child(t!("tree.loading").to_string()).into_any_element();
        };
        let act = match self.activity.get(name) {
            Some(Load::Ready(a)) => Some(a.clone()),
            _ => None,
        };
        let recent = act.as_ref().map(|a| a.logins.recent.clone()).unwrap_or_default();

        // Header: who, at a glance.
        let shown = u.display_name.clone().unwrap_or_else(|| u.name.clone());
        let initials: String = shown.split([' ', '_', '.']).filter_map(|w| w.chars().next()).take(2).collect::<String>().to_uppercase();
        let status = if u.disabled {
            crate::ui::badge(crate::ui::Tone::Danger, cx).child(t!("admin.disabled").to_string())
        } else if u.locked {
            crate::ui::badge(crate::ui::Tone::Warning, cx).child(t!("admin.locked").to_string())
        } else {
            crate::ui::badge(crate::ui::Tone::Success, cx).child(t!("admin.active").to_string())
        };
        let mut kind_auth: Vec<String> = u.user_type.iter().map(|t| title_case(t)).collect();
        kind_auth.extend(user_view::auth_summary(&u, &recent).into_iter().map(|a| if a == "password" { t!("admin.password").to_string() } else { a.to_string() }));
        let scim = user_view::scim_managed(u.owner.as_deref()).then(|| u.owner.clone().unwrap_or_default());
        let sub = [Some(u.login_name.clone().unwrap_or_else(|| u.name.clone())), u.email.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let header = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_4()
                    .flex_wrap()
                    .child(
                        div()
                            .size(px(48.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(theme.secondary)
                            .text_color(theme.primary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(initials),
                    )
                    .child(v_flex().gap_1().min_w_0().child(div().text_xl().child(shown)).child(div().text_sm().text_color(theme.muted_foreground).child(sub)))
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(status)
                            .when(!kind_auth.is_empty(), |d| d.child(crate::ui::badge(crate::ui::Tone::Neutral, cx).child(kind_auth.join(" · "))))
                            .child(if u.has_mfa {
                                crate::ui::badge(crate::ui::Tone::Success, cx).child("MFA")
                            } else {
                                crate::ui::badge(crate::ui::Tone::Warning, cx).child(t!("admin.no_mfa").to_string())
                            })
                            .when(scim.is_some(), |d| d.child(crate::ui::badge(crate::ui::Tone::Info, cx).child(t!("admin.scim").to_string()))),
                    ),
            )
            .when_some(scim, |d, owner| d.child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.scim_note", owner = owner).to_string())));

        // Left: defaults, activity, logins.
        let created = u.created_on.as_deref().map(|c| match user_view::parse_ts(c) {
            Some(t) => format!("{} · {}", &c[..10.min(c.len())], ago(t)),
            None => c.to_string(),
        });
        let defaults = v_flex()
            .child(section(t!("admin.defaults").to_string(), &theme))
            .child(kv(t!("admin.role").to_string(), u.default_role.clone(), &theme))
            .child(kv(t!("admin.warehouse").to_string(), u.default_warehouse.clone(), &theme))
            .child(kv(t!("admin.namespace").to_string(), u.default_namespace.clone(), &theme))
            .child(kv(t!("admin.created").to_string(), created, &theme));
        let left = v_flex().flex_grow(1.).flex_basis(px(460.)).min_w_0().gap_6().child(defaults).child(self.render_activity(name, cx));

        // Right: unified access and what else it uses.
        let right = v_flex().flex_grow(1.).flex_basis(px(320.)).min_w_0().gap_6().child(self.render_access_summary(name, cx));

        v_flex().gap_6().pt_2().child(header).child(h_flex().items_start().flex_wrap().gap_10().child(left).child(right)).into_any_element()
    }

    /// 30-day activity: 4 numbers, day strip (more readable than mostly empty bars) and logins.
    fn render_activity(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let head = h_flex()
            .items_baseline()
            .child(section(t!("admin.activity_30").to_string(), &theme))
            .child(div().flex_1())
            .child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.activity_lag").to_string()));
        let out = v_flex().gap_3().child(head);
        let a = match self.activity.get(name) {
            None | Some(Load::Loading) => return out.child(t!("tree.loading").to_string()).into_any_element(),
            Some(Load::Failed(m)) => return out.child(div().text_color(theme.danger).child(m.clone())).into_any_element(),
            Some(Load::Ready(a)) => a.clone(),
        };
        let q = &a.queries;
        let since = q.last.as_deref().and_then(user_view::parse_ts).map_or("—".to_string(), since);
        let tile = |big: String, small: Option<String>, label: String| {
            v_flex()
                .flex_grow(1.)
                .flex_basis(px(110.))
                .px_3()
                .py_2()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .child(h_flex().items_baseline().gap_1().child(div().text_xl().child(big)).when_some(small, |d, s| d.child(div().text_xs().text_color(theme.muted_foreground).child(s))))
                .child(div().text_xs().text_color(theme.muted_foreground).child(label))
        };
        let tiles = h_flex()
            .flex_wrap()
            .gap_2()
            .child(tile(q.total.to_string(), None, t!("admin.tile_queries").to_string()))
            .child(tile(human_ms(q.elapsed_ms), None, t!("admin.tile_runtime").to_string()))
            .child(tile(a.logins.ok.to_string(), Some(t!("admin.tile_failed", n = a.logins.failed).to_string()), t!("admin.tile_logins").to_string()))
            .child(tile(since, None, t!("admin.tile_since").to_string()));
        let color = |lvl: u8| match lvl {
            0 => theme.secondary,
            1 => theme.primary.opacity(0.3),
            2 => theme.primary.opacity(0.6),
            _ => theme.primary,
        };
        let mut strip = h_flex().gap(px(3.));
        for (i, (day, n)) in q.per_day.iter().enumerate() {
            let tip = format!("{day} · {n}");
            strip = strip.child(div().id(("day", i)).flex_1().h(px(28.)).rounded(px(3.)).bg(color(user_view::intensity(*n))).tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx)));
        }
        let first = q.per_day.first().map(|(d, _)| short_day(d)).unwrap_or_default();
        let peak = q.per_day.iter().max_by_key(|(_, n)| *n).filter(|(_, n)| *n > 0).map(|(d, n)| t!("admin.peak", day = short_day(d), n = n).to_string()).unwrap_or_default();
        let labels = h_flex().justify_between().text_xs().text_color(theme.muted_foreground).child(first).child(peak).child(t!("admin.today").to_string());

        let mut signins = v_flex().child(section(t!("admin.sign_ins").to_string(), &theme));
        for (e, n) in user_view::group_sign_ins(&a.logins.recent) {
            let how = [e.factor.clone(), e.client.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            signins = signins.child(
                v_flex()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .text_sm()
                            .when(!e.success, |d| d.text_color(theme.danger))
                            .child(div().w(px(130.)).flex_shrink_0().child(e.at.get(..16).unwrap_or(&e.at).to_string()))
                            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.muted_foreground).child(how))
                            .child(div().font_family(theme.mono_font_family.clone()).text_xs().text_color(theme.muted_foreground).child(e.ip.clone().unwrap_or_default()))
                            .when(n > 1, |d| d.child(crate::ui::badge(crate::ui::Tone::Neutral, cx).child(format!("×{n}")))),
                    )
                    .when_some(e.error.clone(), |d, err| d.child(div().text_xs().text_color(theme.danger).child(err))),
            );
        }
        out.child(tiles).child(v_flex().gap_1().child(strip).child(labels)).child(signins).into_any_element()
    }

    /// Aggregated TAG_REFERENCES (a few seconds on the warehouse): only the first time classification is opened.
    fn ensure_tag_counts(&mut self, cx: &mut Context<Self>) {
        // With no tag in the configuration there's nothing to count (nor a session to open).
        if self.tag_counts.is_some() || !self.conn.org.has_classification() {
            return;
        }
        self.tag_counts = Some(Load::Loading);
        let (conn, engine, generation) = (self.conn.clone(), cx.global::<Db>().0.clone(), self.generation);
        cx.spawn(async move |this, cx| {
            let res = match engine.connect(&conn.cfg, conn.creds.clone()).await {
                Ok(s) => masking::tag_counts(&s, &conn.org).await,
                Err(e) => Err(e),
            };
            this.update(cx, |t, cx| {
                if t.generation == generation {
                    t.tag_counts = Some(match res {
                        Ok(c) => Load::Ready(Arc::new(c)),
                        Err(e) => Load::Failed(explain(&e, "admin.hint_activity")),
                    });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Summary of where a classification item unmasks: "everywhere" or "N schemas · M databases".
    fn class_scope_summary(&self, it: &UnlockItem) -> String {
        let Detail::Level(l) = it.detail else { return detail_text(it) };
        if !self.conn.org.has_masking() {
            return detail_text(it);
        }
        let scopes: Vec<Scope> = it.roles.iter().map(|r| self.conn.org.scope(r, l, &self.db_names)).collect();
        if scopes.contains(&Scope::Global) {
            return t!("admin.scope_everywhere").to_string();
        }
        let schemas = scopes.iter().filter(|s| matches!(s, Scope::Schema(..))).count();
        let dbs = scopes.iter().filter(|s| matches!(s, Scope::Database(_) | Scope::Group)).count();
        match (schemas, dbs) {
            (0, d) => t!("admin.scope_dbs", n = d).to_string(),
            (s, 0) => t!("admin.scope_schemas", n = s).to_string(),
            (s, d) => format!("{} · {}", t!("admin.scope_schemas", n = s), t!("admin.scope_dbs", n = d)),
        }
    }

    /// "Unmasks": where each role of the level applies and how many columns that covers (read by it or not).
    fn render_unmasks(&mut self, it: &UnlockItem, level: u8, items: &[UnlockItem], held: &[String], cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        // Read = having USAGE on the database through some access role.
        let readable: HashSet<&str> = items.iter().filter(|i| i.kind == ItemKind::Data && !i.name.is_empty()).map(|i| i.name.as_str()).collect();
        let mut out = v_flex().gap_1().child(div().pt_2().text_xs().text_color(theme.muted_foreground).child(t!("admin.unmasks", level = level_text(&self.conn.org, level)).to_string()));
        let org = self.conn.org.clone();
        let forced = org.forces_masking(held);
        if let Some(top) = org.max_level().filter(|_| !forced.is_empty()) {
            let top = level_text(&org, top);
            out = out.child(div().text_xs().text_color(theme.warning).child(t!("admin.forced_masking", roles = forced.join(", "), level = top).to_string()));
        }
        let counts = match &self.tag_counts {
            _ if !org.has_classification() => None,
            Some(Load::Ready(c)) => Some(c.clone()),
            Some(Load::Failed(m)) => {
                out = out.child(div().text_xs().text_color(theme.danger).child(m.clone()));
                None
            }
            _ => {
                out = out.child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.counting_columns").to_string()));
                None
            }
        };
        let mut scopes: Vec<Scope> = it.roles.iter().map(|r| org.scope(r, level, &self.db_names)).collect();
        scopes.sort_by_key(|s| scope_label(&org, s));
        scopes.dedup();
        for sc in scopes {
            let line = match &counts {
                Some(c) => {
                    let (inside, outside): (Vec<TagCount>, Vec<TagCount>) = c.iter().cloned().partition(|x| readable.contains(x.database.as_str()));
                    let (r, n) = (org.columns(&inside, level, &sc), org.columns(&outside, level, &sc));
                    t!("admin.scope_columns", read = r, unread = n).to_string()
                }
                None => String::new(),
            };
            out = out.child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().font_family(theme.mono_font_family.clone()).child(scope_label(&org, &sc)))
                    .child(div().text_color(theme.muted_foreground).child(line)),
            );
        }
        out.child(div().pt_1().text_xs().text_color(theme.muted_foreground).child(t!("admin.policy_note", config = org.name()).to_string()))
    }

    /// User's roles (direct and inherited) and the inheritance/databases graph for the paths.
    fn access_data(&self, name: &str) -> Option<(Vec<String>, Vec<EffectiveRole>, bool)> {
        match self.user_roles.get(name) {
            Some(Load::Ready((d, e))) => {
                let loading = matches!(e, Load::Loading);
                let eff = if let Load::Ready(e) = e { e.clone() } else { Vec::new() };
                Some((d.clone(), eff, loading))
            }
            _ => None,
        }
    }

    /// Inheritance and databases per role: from ACCOUNT_USAGE when it came, otherwise from the SHOW GRANTS already done.
    /// `Arc`: redrawing must not clone the whole account hierarchy every frame.
    fn inherits_and_databases(&self) -> Graphs {
        if let Load::Ready(g) = &self.graph {
            return (g.clone(), self.databases.clone());
        }
        let key = (self.generation, self.grants_cache.len());
        if let Some((k, graphs)) = &self.cache.borrow().graphs
            && *k == key
        {
            return graphs.clone();
        }
        let inh = self.grants_cache.iter().map(|(k, g)| (k.clone(), admin::inherited_roles(g))).collect();
        let dbs = self.grants_cache.iter().map(|(k, g)| (k.clone(), g.iter().filter(|x| x.privilege == "USAGE" && x.granted_on == "DATABASE").map(|x| x.name.clone()).collect())).collect();
        let graphs = (Arc::new(inh), Arc::new(dbs));
        self.cache.borrow_mut().graphs = Some((key, graphs.clone()));
        graphs
    }

    /// Access summary on the user's page; the full list opens at "All roles →".
    fn render_access_summary(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let out = v_flex().gap_3().child(section(t!("admin.access").to_string(), &theme));
        let Some((direct, eff, loading)) = self.access_data(name) else {
            return match self.user_roles.get(name) {
                Some(Load::Failed(m)) => out.child(div().text_color(theme.danger).child(m.clone())),
                _ => out.child(t!("tree.loading").to_string()),
            }
            .into_any_element();
        };
        let target = Target::UserRoles(name.to_string());
        out.child(self.render_access_facts(name, &direct, &eff, loading, cx))
            .child(Button::new("all-roles").label(t!("admin.all_roles").to_string()).outline().small().on_click(cx.listener(move |t, _, _, cx| t.open(target.clone(), cx))))
            .into_any_element()
    }

    /// "N direct → M inherited" and the facts that answer "what can this person reach".
    fn render_access_facts(&mut self, name: &str, direct: &[String], eff: &[EffectiveRole], loading: bool, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let (_, dbs) = self.inherits_and_databases();
        let all: Vec<String> = direct.iter().cloned().chain(eff.iter().map(|e| e.name.clone())).collect();
        let items = self.unlocks_of(all, &dbs);
        let count = |k: ItemKind| items.iter().filter(|i| i.kind == k).count();
        let top = items.iter().find_map(|i| if let Detail::Level(l) = i.detail { Some(l) } else { None });
        let (data_roles, data_dbs) = unlocks::data_spread(&items);
        let by_hand = self.direct_grants.get(name).map_or(0, |g| g.iter().filter(|g| !g.from_scim()).count());
        let mut facts: Vec<String> = Vec::new();
        if let Some(l) = top {
            facts.push(t!("admin.fact_level", level = level_text(&self.conn.org, l)).to_string());
        }
        // Only what the configuration can recognize; without a rule, "0 warehouses" would be false.
        let org = &self.conn.org;
        if org.has_kind(RoleKind::Warehouse) {
            facts.push(t!("admin.fact_warehouses", n = count(ItemKind::Warehouse)).to_string());
        }
        if org.has_kind(RoleKind::Base) {
            facts.push(t!("admin.fact_departments", n = count(ItemKind::Department)).to_string());
        }
        if org.has_kind(RoleKind::Access) {
            facts.push(t!("admin.fact_data", n = data_roles, dbs = data_dbs).to_string());
        }
        if by_hand > 0 {
            facts.push(t!("admin.fact_by_hand", n = by_hand).to_string());
        }
        v_flex()
            .gap_2()
            .child(h_flex().items_center().gap_2().child(div().text_xl().child(if direct.len() == 1 { t!("admin.direct_one") } else { t!("admin.direct_n", n = direct.len()) }.to_string())).child(
                if loading {
                    // "0 inherited" while loading looked like the final answer; why it's slow goes in the hover.
                    let why = t!("admin.loading_inherited").to_string();
                    h_flex()
                        .id("inherited-loading")
                        .gap_1p5()
                        .items_center()
                        .text_base()
                        .text_color(theme.muted_foreground)
                        .tooltip(move |w, cx| gpui_kit::component::tooltip::Tooltip::new(why.clone()).build(w, cx))
                        .child(t!("admin.inherited_loading").to_string())
                        .child(Spinner::new().xsmall())
                        .into_any_element()
                } else {
                    h_flex().text_base().text_color(theme.muted_foreground).child(t!("admin.inherited_n", n = eff.len()).to_string()).into_any_element()
                },
            ))
            // Counts with only the direct ones would be wrong: they wait for the inherited.
            .when(!loading, |d| d.child(h_flex().flex_wrap().gap_x_4().gap_y_1().text_sm().text_color(theme.muted_foreground).children(facts)))
    }

    /// "Users / NAME / Roles" screen: search, groups by what the role unlocks and the selected one's path.
    fn render_user_roles(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some((direct, eff, loading)) = self.access_data(name) else {
            return match self.user_roles.get(name) {
                Some(Load::Failed(m)) => div().text_color(theme.danger).child(m.clone()),
                _ => div().child(t!("tree.loading").to_string()),
            }
            .into_any_element();
        };
        let (inherits, dbs) = self.inherits_and_databases();
        let all: Vec<String> = direct.iter().cloned().chain(eff.iter().map(|e| e.name.clone())).collect();
        let items = self.unlocks_of(all.clone(), &dbs);
        let usage = match self.activity.get(name) {
            Some(Load::Ready(a)) => Some(a.usage.clone()),
            _ => None,
        };
        let now = user_view::now();
        let default_role = match &self.users {
            Load::Ready(us) => us.iter().find(|u| u.name == name).and_then(|u| u.default_role.clone()),
            _ => None,
        };
        let grants = self.direct_grants.get(name).cloned().unwrap_or_default();
        // Measurable usage: warehouse by name, role only when it was the query's primary.
        let used_of = |it: &UnlockItem| -> Option<Option<i64>> {
            let u = usage.as_ref()?;
            let last = match it.kind {
                ItemKind::Warehouse => u.warehouses.get(&it.name),
                ItemKind::Functional | ItemKind::Other => u.roles.get(&it.name),
                _ => return None,
            };
            Some(last.and_then(|l| user_view::parse_ts(l)))
        };
        let toks = unlocks::tokens(self.roles_search.read(cx).value().as_ref());
        let searching = !toks.is_empty();

        // Header + search + filters.
        let mut chips = h_flex().flex_wrap().gap_1().child(Button::new("uf-all").label(t!("admin.all_types").to_string()).xsmall().outline().selected(self.unlock_filter.is_none()).on_click(
            cx.listener(|t, _, _, cx| {
                t.unlock_filter = None;
                cx.notify();
            }),
        ));
        for (i, k) in [ItemKind::Warehouse, ItemKind::Classification, ItemKind::Department, ItemKind::Data, ItemKind::Functional, ItemKind::Other].into_iter().enumerate() {
            let n = items.iter().filter(|it| it.kind == k).count();
            if n == 0 {
                continue;
            }
            chips = chips.child(
                Button::new(("uf", i))
                    .child(h_flex().gap_1().items_center().child(div().size(px(7.)).rounded_full().bg(kind_color(k, &theme))).child(format!("{} {n}", item_kind_label(k))))
                    .xsmall()
                    .outline()
                    .selected(self.unlock_filter == Some(k))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        t.unlock_filter = if t.unlock_filter == Some(k) { None } else { Some(k) };
                        cx.notify();
                    })),
            );
        }
        chips = chips.child(div().flex_1()).child(Checkbox::new("unused-90").label(t!("admin.unused_90").to_string()).small().checked(self.unused_only).on_click(cx.listener(|t, c: &bool, _, cx| {
            t.unused_only = *c;
            cx.notify();
        })));
        let head = v_flex().gap_3().child(self.render_access_facts(name, &direct, &eff, loading, cx)).child(Input::new(&self.roles_search).small().cleanable(true)).child(chips);

        // List.
        let mut list = v_flex().gap_4().flex_grow(1.).flex_basis(px(420.)).min_w_0();
        let used_text = |u: Option<Option<i64>>| -> (String, bool) {
            match u {
                None => ("—".into(), false),
                Some(None) => (t!("admin.unused").to_string(), true),
                Some(Some(t)) if now - t < 86_400 => (t!("admin.today").to_string().to_lowercase(), false),
                Some(Some(t)) => (ago(t).to_string(), false),
            }
        };
        if self.unlock_filter.is_none() && !self.unused_only {
            let mut sec = v_flex().gap_1().child(section(t!("admin.direct").to_string(), &theme));
            for (i, g) in grants.iter().filter(|g| !searching || unlocks::matches_all(&g.role, &toks)).enumerate() {
                let reach = eff.iter().filter(|e| e.path.first() == Some(&g.role)).count();
                let source = if g.from_scim() {
                    t!("admin.source_scim").to_string()
                } else {
                    t!("admin.source_hand", by = g.granted_by.clone().unwrap_or_default(), on = g.created_on.as_deref().map(|c| c.get(..10).unwrap_or(c)).unwrap_or("")).to_string()
                };
                let (used, warn) = used_text(usage.as_ref().map(|u| u.roles.get(&g.role).and_then(|l| user_view::parse_ts(l))));
                let role = g.role.clone();
                sec = sec.child(
                    h_flex()
                        .id(("direct", i))
                        .gap_3()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(self.selected.as_deref() == Some(g.role.as_str()), |d| d.bg(theme.secondary))
                        .hover(|d| d.bg(theme.secondary_hover))
                        .child(v_flex().flex_1().min_w_0().child(one_line(highlighted(&g.role, &toks, &theme))).child(div().text_xs().text_color(theme.muted_foreground).child(source)))
                        .child(div().flex_shrink_0().whitespace_nowrap().text_xs().text_color(theme.muted_foreground).child(t!("admin.reaches", n = reach).to_string()))
                        .child(div().w(px(80.)).text_xs().text_right().text_color(if warn { theme.warning } else { theme.muted_foreground }).child(used))
                        .on_click(cx.listener(move |t, _, _, cx| {
                            t.selected = Some(role.clone());
                            cx.notify();
                        })),
                );
            }
            list = list.child(sec);
        }
        let mut shown_any = false;
        for (gi, k) in [ItemKind::Warehouse, ItemKind::Classification, ItemKind::Department, ItemKind::Data, ItemKind::Functional, ItemKind::Other].into_iter().enumerate() {
            if self.unlock_filter.is_some_and(|f| f != k) {
                continue;
            }
            let hits: Vec<(&UnlockItem, Vec<&String>)> = items
                .iter()
                .filter(|it| it.kind == k)
                .filter(|it| !self.unused_only || matches!(used_of(it), Some(None)))
                .filter_map(|it| {
                    if !searching {
                        return Some((it, Vec::new()));
                    }
                    let own = unlocks::matches_all(&format!("{} {}", it.name, detail_text(it)), &toks);
                    let members: Vec<&String> = it.roles.iter().filter(|r| unlocks::matches_all(r, &toks) && **r != it.name).collect();
                    (own || !members.is_empty()).then_some((it, members))
                })
                .collect();
            if hits.is_empty() {
                continue;
            }
            shown_any = true;
            let collapsed = !searching && hits.len() > 6 && !self.group_more.contains(&k);
            let total = hits.len();
            let mut sec = v_flex().gap_1().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(7.)).rounded_full().bg(kind_color(k, &theme)))
                    .child(section(item_kind_label(k), &theme))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(total.to_string())),
            );
            for (ii, (it, members)) in hits.into_iter().take(if collapsed { 5 } else { total }).enumerate() {
                let key = format!("{k:?}:{}", it.name);
                let open = searching || self.item_open.contains(&key);
                // Expands with one role too: under a database, that role (e.g. `…_MONOLITH_RO_AR`) is the schema.
                let multi = !it.roles.is_empty();
                let first = it.roles[0].clone();
                let (used, warn) = used_text(used_of(it));
                let label = if it.name.is_empty() { t!("admin.unknown_db").to_string() } else { item_title(&self.conn.org, it) };
                let selected = self.selected.as_ref().is_some_and(|s| it.roles.contains(s));
                let key2 = key.clone();
                sec = sec.child(
                    h_flex()
                        .id(("item", gi * 100_000 + ii))
                        .gap_3()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.secondary))
                        .hover(|d| d.bg(theme.secondary_hover))
                        .child(div().w(px(12.)).text_xs().text_color(theme.muted_foreground).child(if multi { if open { "▾" } else { "▸" } } else { "" }))
                        .child(one_line(highlighted(&label, &toks, &theme)).flex_1())
                        .child(div().flex_shrink_0().whitespace_nowrap().text_xs().text_color(theme.muted_foreground).child(if k == ItemKind::Classification {
                            self.class_scope_summary(it)
                        } else {
                            detail_text(it)
                        }))
                        .child(div().w(px(80.)).text_xs().text_right().text_color(if warn { theme.warning } else { theme.muted_foreground }).child(used))
                        .on_click(cx.listener(move |t, _, _, cx| {
                            if multi && !t.item_open.remove(&key2) {
                                t.item_open.insert(key2.clone());
                            }
                            if k == ItemKind::Classification {
                                t.ensure_tag_counts(cx);
                            }
                            t.selected = Some(first.clone());
                            cx.notify();
                        })),
                );
                if multi && open {
                    let rows: Vec<&String> = if searching { members } else { it.roles.iter().collect() };
                    for (mi, m) in rows.into_iter().enumerate() {
                        let role = m.clone();
                        let where_ = if let Detail::Level(l) = it.detail { Some(scope_label(&self.conn.org, &self.conn.org.scope(m, l, &self.db_names))) } else { None };
                        sec = sec.child(
                            h_flex()
                                .id(("member", (gi * 100_000 + ii) * 1_000 + mi))
                                .gap_2()
                                .pl(px(32.))
                                .py_0p5()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .text_sm()
                                .when(self.selected.as_deref() == Some(m.as_str()), |d| d.bg(theme.secondary))
                                .hover(|d| d.bg(theme.secondary_hover))
                                .child(div().text_color(theme.muted_foreground).child("└"))
                                .child(one_line(highlighted(m, &toks, &theme)).flex_1())
                                .when_some(where_, |d, w| d.child(one_line(w).max_w(relative(0.4)).text_xs().text_color(theme.muted_foreground)))
                                .on_click(cx.listener(move |t, _, _, cx| {
                                    if k == ItemKind::Classification {
                                        t.ensure_tag_counts(cx);
                                    }
                                    t.selected = Some(role.clone());
                                    cx.notify();
                                })),
                        );
                    }
                }
            }
            if collapsed {
                sec = sec.child(Button::new(("more", gi)).label(t!("admin.n_more", n = total - 5).to_string()).ghost().xsmall().on_click(cx.listener(move |t, _, _, cx| {
                    t.group_more.insert(k);
                    cx.notify();
                })));
            }
            list = list.child(sec);
        }
        if !shown_any && searching {
            list = list.child(div().text_color(theme.muted_foreground).child(t!("admin.no_match", q = self.roles_search.read(cx).value().to_string()).to_string()));
        }
        if loading {
            list = list.child(div().text_sm().text_color(theme.muted_foreground).child(t!("admin.loading_inherited").to_string()));
        }

        // Panel: the why of the selected one (all paths).
        let aside = self.selected.clone().filter(|s| all.contains(s)).map(|sel| {
            let paths = unlocks::all_paths(&direct, &inherits, &sel, 5);
            let item = items.iter().find(|it| it.roles.contains(&sel));
            let scim = paths.iter().any(|p| grants.iter().any(|g| Some(&g.role) == p.first() && g.from_scim()));
            let mut a = v_flex()
                .gap_3()
                .p_4()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .when_some(item, |d, it| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(div().size(px(7.)).rounded_full().bg(kind_color(it.kind, &theme)))
                            .child(item_kind_label(it.kind)),
                    )
                    .child(div().text_lg().font_family(theme.mono_font_family.clone()).child(if it.name.is_empty() { t!("admin.unknown_db").to_string() } else { item_title(&self.conn.org, it) }))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(detail_text(it)))
                })
                .child(h_flex().gap_2().child(div().flex_1().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).overflow_hidden().text_ellipsis().child(sel.clone())).child(
                    Button::new("copy-role").label(t!("admin.copy").to_string()).ghost().xsmall().on_click({
                        let sel = sel.clone();
                        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(sel.clone()))
                    }),
                ))
                .child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.paths", n = paths.len()).to_string()));
            for p in &paths {
                let mut ol = v_flex().gap_0p5().p_2().rounded(theme.radius).bg(theme.secondary).text_xs().font_family(theme.mono_font_family.clone());
                for (d, node) in p.iter().enumerate() {
                    let tag = if d == 0 {
                        grants.iter().find(|g| &g.role == node).map(|g| if g.from_scim() { t!("admin.scim_short").to_string() } else { t!("admin.by_hand").to_string() })
                    } else if default_role.as_deref() == Some(node.as_str()) {
                        Some(t!("admin.default_marker").to_string())
                    } else {
                        None
                    };
                    ol = ol.child(
                        h_flex()
                            .gap_1()
                            .pl(px(d as f32 * 12.))
                            .when(d > 0, |r| r.child(div().text_color(theme.muted_foreground).child("└")))
                            .child(div().min_w_0().overflow_hidden().text_ellipsis().child(node.clone()))
                            .when_some(tag, |r, tg| r.child(div().px_1().rounded(px(3.)).border_1().border_color(theme.border).text_color(theme.muted_foreground).child(tg))),
                    );
                }
                a = a.child(ol);
            }
            if scim {
                a = a.child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.scim_path").to_string()));
            }
            // Without masking rules in the configuration, "where it unmasks" would be made up.
            if let Some(it) = item.filter(|it| it.kind == ItemKind::Classification && self.conn.org.has_masking())
                && let Detail::Level(l) = it.detail
            {
                a = a.child(self.render_unmasks(it, l, &items, &all, cx));
            }
            a
        });

        v_flex()
            .gap_5()
            .pt_2()
            .child(head)
            .child(h_flex().items_start().flex_wrap().gap_8().child(list).when_some(aside, |d, a| d.child(div().flex_grow(1.).flex_basis(px(320.)).max_w(px(440.)).min_w_0().child(a))))
            .into_any_element()
    }

    fn render_role(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut out = v_flex().gap_1();
        if let Load::Ready(rs) = &self.roles
            && let Some(r) = rs.iter().find(|r| r.name == name)
        {
            out = out.child(DescriptionList::horizontal().columns(2).item(t!("admin.owner").to_string(), r.owner.clone().unwrap_or_else(|| "—".into()), 1).item(
                t!("admin.comment").to_string(),
                r.comment.clone().unwrap_or_else(|| "—".into()),
                1,
            ));
        }
        match self.role_details.get(name) {
            None | Some(Load::Loading) => out = out.child(t!("tree.loading").to_string()),
            Some(Load::Failed(m)) => out = out.child(div().text_color(theme.danger).child(m.clone())),
            Some(Load::Ready((grants, who))) => {
                let (grants, who) = (grants.clone(), who.clone());
                let mut granted = h_flex().flex_wrap().gap_1();
                for (i, g) in who.iter().enumerate() {
                    granted = granted.child(if g.kind == "ROLE" { role_tag(("who", i), &g.name, cx).into_any_element() } else { user_tag(("who", i), &g.name, cx).into_any_element() });
                }
                if who.is_empty() {
                    granted = granted.child(div().text_color(theme.muted_foreground).child(t!("admin.none").to_string()));
                }
                let privs: Vec<Grant> = grants.iter().filter(|g| !(g.privilege == "USAGE" && g.granted_on == "ROLE")).cloned().collect();
                let n = privs.len();
                out = out
                    .child(section(t!("admin.granted_to").to_string(), &theme))
                    .child(granted)
                    .child(section(t!("admin.inherits").to_string(), &theme))
                    .child(tags("inh", &admin::inherited_roles(&grants), cx))
                    .child(section(t!("admin.privileges", n = n).to_string(), &theme))
                    // Virtualized: SYSADMIN may have thousands of grants.
                    .child(
                        uniform_list("adm-privs", n, move |range, _, cx| {
                            let muted = cx.theme().muted_foreground;
                            range
                                .map(|i| {
                                    let g = &privs[i];
                                    h_flex()
                                        .gap_2()
                                        .text_sm()
                                        .whitespace_nowrap()
                                        .child(div().w(px(160.)).flex_shrink_0().child(g.privilege.clone()))
                                        .child(div().w(px(120.)).flex_shrink_0().text_color(muted).child(g.granted_on.clone()))
                                        .child(div().min_w_0().overflow_hidden().text_ellipsis().child(g.name.clone()))
                                        .when(g.grant_option, |d| d.child(div().text_xs().text_color(muted).child("(grant option)")))
                                })
                                .collect()
                        })
                        .h(px(360.)),
                    );
            }
        }
        out.into_any_element()
    }
}

impl Render for AdminTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().size_full().child(self.render_list(cx)).child(div().flex_1().min_w_0().h_full().child(self.render_details(cx)))
    }
}

/// "key · value" row of the sections; empty becomes "not defined".
pub(crate) fn kv(label: String, value: Option<String>, theme: &Theme) -> Div {
    h_flex().min_h(px(40.)).gap_4().border_b_1().border_color(theme.border).child(div().w(px(130.)).flex_shrink_0().text_color(theme.muted_foreground).child(label)).child(
        match value.filter(|v| !v.is_empty()) {
            Some(v) => div().min_w_0().overflow_hidden().text_ellipsis().font_family(theme.mono_font_family.clone()).child(v),
            None => div().text_color(theme.muted_foreground).child(t!("admin.not_set").to_string()),
        },
    )
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// `2026-09-29` → `09-29`.
fn short_day(d: &str) -> String {
    d.get(5..10).unwrap_or(d).to_string()
}

/// User's bottom line in the list; `true` = highlight (idle for a long time).
fn meta_text(m: UserMeta) -> (String, bool) {
    match m {
        UserMeta::Disabled => (t!("admin.disabled").to_string(), false),
        UserMeta::Service { key_pair: true } => (t!("admin.meta_service_key").to_string(), false),
        UserMeta::Service { key_pair: false } => (t!("admin.meta_service").to_string(), false),
        UserMeta::NeverLoggedIn { age } => (t!("admin.meta_never", age = span_label(age)).to_string(), false),
        UserMeta::Idle { days } => (t!("admin.meta_idle", n = days).to_string(), true),
        UserMeta::Active { since } => (t!("admin.meta_active", ago = span_label(since)).to_string(), false),
    }
}

fn scope_label(org: &OrgConfig, s: &Scope) -> String {
    match s {
        Scope::Global => t!("admin.scope_everywhere").to_string(),
        Scope::Group => t!("admin.scope_group", glob = org.group_glob().unwrap_or("?")).to_string(),
        Scope::Database(d) => t!("admin.scope_db", db = d).to_string(),
        Scope::Schema(d, sc) => format!("{d}.{sc}"),
        Scope::Unresolved(x) => format!("{x} (?)"),
    }
}

fn item_kind_label(k: ItemKind) -> String {
    match k {
        ItemKind::Warehouse => t!("admin.kind_warehouse"),
        ItemKind::Classification => t!("admin.kind_classification"),
        ItemKind::Department => t!("admin.kind_department"),
        ItemKind::Data => t!("admin.kind_data"),
        ItemKind::Functional => t!("admin.kind_functional"),
        ItemKind::Other => t!("admin.kind_other"),
    }
    .to_string()
}

/// One color per kind, the same on the chip, the group and the panel.
fn kind_color(k: ItemKind, theme: &Theme) -> Hsla {
    match k {
        ItemKind::Warehouse => theme.primary,
        ItemKind::Classification => theme.warning,
        ItemKind::Department => theme.info,
        ItemKind::Data => theme.success,
        ItemKind::Functional => theme.foreground,
        ItemKind::Other => theme.muted_foreground,
    }
}

/// `C4 · Restricted` (labels from the configuration).
fn level_text(org: &OrgConfig, l: u8) -> String {
    match org.level_label(l) {
        Some(n) => format!("C{l} · {n}"),
        None => format!("C{l}"),
    }
}

fn item_title(org: &OrgConfig, it: &UnlockItem) -> String {
    match it.detail {
        Detail::Level(l) => level_text(org, l),
        _ => it.name.clone(),
    }
}

fn detail_text(it: &UnlockItem) -> String {
    let n = it.roles.len();
    match it.detail {
        Detail::Warehouse { operate: true, monitor: true } => t!("admin.wh_both").to_string(),
        Detail::Warehouse { operate: true, .. } => t!("admin.wh_operate").to_string(),
        Detail::Warehouse { .. } => t!("admin.wh_monitor").to_string(),
        Detail::Level(_) => n_roles(n),
        Detail::Data { write } => format!("{} · {}", if write { t!("admin.read_write") } else { t!("admin.read") }, n_roles(n)),
        Detail::None => match it.kind {
            ItemKind::Department => t!("admin.base_role").to_string(),
            ItemKind::Functional => t!("admin.functional_role").to_string(),
            _ => String::new(),
        },
    }
}

/// Text with the search matches highlighted.
fn highlighted(text: &str, toks: &[String], theme: &Theme) -> StyledText {
    let style = HighlightStyle { color: Some(theme.primary), font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() };
    StyledText::new(text.to_string()).with_highlights(unlocks::match_ranges(text, toks).into_iter().map(|r| (r, style)))
}

/// A single line, with ellipsis: long role names don't invade the neighboring column (the panel
/// shows the full name).
fn one_line(el: impl IntoElement) -> Div {
    div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(el)
}

/// Clickable tag that navigates to the role.
fn role_tag(id: impl Into<ElementId>, name: &str, cx: &mut Context<AdminTab>) -> Stateful<Div> {
    let target = Target::Role(name.to_string());
    div().id(id).cursor_pointer().child(crate::ui::badge(crate::ui::Tone::Neutral, cx).child(name.to_string())).on_click(cx.listener(move |t, _, _, cx| t.open(target.clone(), cx)))
}

fn user_tag(id: impl Into<ElementId>, name: &str, cx: &mut Context<AdminTab>) -> Stateful<Div> {
    let target = Target::User(name.to_string());
    div()
        .id(id)
        .cursor_pointer()
        .child(crate::ui::badge(crate::ui::Tone::Info, cx).child(h_flex().gap_1().child(Icon::new(IconName::CircleUser).xsmall()).child(name.to_string())))
        .on_click(cx.listener(move |t, _, _, cx| t.open(target.clone(), cx)))
}

/// Role tag row; `prefix` makes ids unique per section.
fn tags(prefix: &'static str, roles: &[String], cx: &mut Context<AdminTab>) -> AnyElement {
    if roles.is_empty() {
        return div().text_color(cx.theme().muted_foreground).child(t!("admin.none").to_string()).into_any_element();
    }
    let mut row = h_flex().flex_wrap().gap_1();
    for (i, r) in roles.iter().enumerate() {
        row = row.child(role_tag((prefix, i), r, cx));
    }
    row.into_any_element()
}

/// 12500 → "12.5 s"; 4_000_000 → "1.1 h".
pub(crate) fn human_ms(ms: u64) -> String {
    let s = ms as f64 / 1000.0;
    if s < 60.0 {
        format!("{s:.1} s")
    } else if s < 3600.0 {
        format!("{:.1} min", s / 60.0)
    } else {
        format!("{:.1} h", s / 3600.0)
    }
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    #[test]
    fn human_ms_units() {
        assert_eq!(super::human_ms(12_500), "12.5 s");
        assert_eq!(super::human_ms(90_000), "1.5 min");
        assert_eq!(super::human_ms(4_000_000), "1.1 h");
    }
}

fn n_roles(n: usize) -> String {
    if n == 1 { t!("admin.one_role").to_string() } else { t!("admin.n_roles", n = n).to_string() }
}
