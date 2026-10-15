//! Aba "Usuários & Roles" do Snowflake (somente leitura).

use rust_i18n::t;
use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::assets::IconName;
use gpui_kit::component::chart::BarChart;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::admin::{self, Activity, EffectiveRole, Grant, Grantee, RoleInfo, UserInfo};
use rb_core::session::Session;

use crate::Db;
use crate::lang::Lang;
use crate::meta::ConnTarget;

pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Clone, PartialEq)]
pub enum Target {
    User(String),
    Role(String),
}

/// SHOW devolve no máximo 10.000 linhas.
const SHOW_LIMIT: usize = 10_000;

pub struct AdminTab {
    conn: ConnTarget,
    /// SHOWs (rápidos).
    session: Option<Arc<Session>>,
    /// ACCOUNT_USAGE (lento): o ator do Snowflake é serial, então fica em outra sessão.
    activity_session: Option<Arc<Session>>,
    show_roles: bool,
    /// Desabilitados ficam fora da lista até pedir.
    show_disabled: bool,
    filter: Entity<InputState>,
    users: Load<Vec<UserInfo>>,
    roles: Load<Vec<RoleInfo>>,
    stack: Vec<Target>,
    /// Roles diretas + efetivas por usuário.
    user_roles: HashMap<String, Load<(Vec<String>, Load<Vec<EffectiveRole>>)>>,
    role_details: HashMap<String, Load<(Vec<Grant>, Vec<Grantee>)>>,
    /// `SHOW GRANTS TO ROLE` por role: herança e tela de role usam o mesmo.
    grants_cache: HashMap<String, Vec<Grant>>,
    activity: HashMap<String, Load<Activity>>,
    /// Sobe a cada ↻: resultado de task anterior ao reload é descartado.
    generation: u64,
    _subs: Vec<Subscription>,
}

/// Mensagem do erro + dica quando é falta de permissão.
pub fn explain(e: &rb_core::DbError, hint_key: &str) -> String {
    let msg = e.to_string();
    if admin::privilege_hint(&msg) { format!("{msg}\n{}", t!(hint_key)) } else { msg }
}

impl AdminTab {
    pub fn new(conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t!("admin.filter").to_string()));
        let subs = vec![
            cx.observe_global_in::<Lang>(window, |this, window, cx| {
                this.filter.update(cx, |s, cx| s.set_placeholder(t!("admin.filter").to_string(), window, cx));
            }),
            cx.subscribe_in(&filter, window, |_, _, ev: &InputEvent, _, cx| {
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
            activity: HashMap::new(),
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
        t!("admin.title").to_string().into()
    }

    /// Conecta (mesmo login: o engine reaproveita a sessão Snowflake) e carrega as listas.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let (conn, existing, engine) = (self.conn.clone(), self.session.clone(), cx.global::<Db>().0.clone());
        self.users = Load::Loading;
        self.roles = Load::Loading;
        // ↻ traz tudo de novo, não só as listas.
        self.user_roles.clear();
        self.role_details.clear();
        self.grants_cache.clear();
        self.activity.clear();
        self.generation += 1;
        let generation = self.generation;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let t0 = std::time::Instant::now();
            let s = match existing {
                Some(s) => Ok(s),
                None => engine.connect(&conn.cfg, conn.creds.clone()).await.map(Arc::new),
            };
            tracing::info!("admin: sessão em {:?} (ok={})", t0.elapsed(), s.is_ok());
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
            // Cada lista aparece assim que chega: não espera a outra.
            let users = admin::users(&s).await;
            tracing::info!("admin: SHOW USERS em {:?} ({:?} linhas)", t0.elapsed(), users.as_ref().map(Vec::len));
            this.update(cx, |tab, cx| {
                if tab.generation != generation {
                    return;
                }
                tab.session = Some(s.clone());
                tab.users = match users {
                    Ok(u) => Load::Ready(u),
                    Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                };
                // O que estava aberto antes do ↻ volta a carregar.
                tab.ensure_loaded(cx);
                cx.notify();
            })
            .ok();
            let roles = admin::roles(&s).await;
            tracing::info!("admin: SHOW ROLES em {:?} ({:?} linhas)", t0.elapsed(), roles.as_ref().map(Vec::len));
            this.update(cx, |tab, cx| {
                if tab.generation != generation {
                    return;
                }
                tab.roles = match roles {
                    Ok(r) => Load::Ready(r),
                    Err(e) => Load::Failed(explain(&e, "admin.hint_grants")),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn open(&mut self, t: Target, cx: &mut Context<Self>) {
        if self.stack.last() != Some(&t) {
            self.stack.push(t);
        }
        self.ensure_loaded(cx);
        cx.notify();
    }

    /// Busca o que falta para o alvo do topo da pilha; o resultado fica em cache na aba.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if let Some(Target::User(u)) = self.stack.last().cloned()
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
                            // Sem warehouse ativo, ACCOUNT_USAGE não roda: dica própria.
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
            Some(Target::User(u)) if !self.user_roles.contains_key(&u) => {
                self.user_roles.insert(u.clone(), Load::Loading);
                // Só a herança já conhecida (nomes), não os grants inteiros: SYSADMIN tem milhares.
                let mut known: HashMap<String, Vec<String>> = self.grants_cache.iter().map(|(k, g)| (k.clone(), admin::inherited_roles(g))).collect();
                let generation = self.generation;
                cx.spawn(async move |this, cx| {
                    let direct = match admin::user_roles(&s, &u).await {
                        Ok(d) => d,
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
                    // Roles diretas aparecem já; a herança (uma consulta por role) vem depois.
                    let d = direct.clone();
                    let uu = u.clone();
                    this.update(cx, |tab, cx| {
                        if tab.generation == generation {
                            tab.user_roles.insert(uu, Load::Ready((d, Load::Loading)));
                            cx.notify();
                        }
                    })
                    .ok();
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
        // O cache pode ter sido limpo por um ↻ depois que este alvo abriu.
        self.ensure_loaded(cx);
        cx.notify();
    }

    /// Nomes da lista atual que casam com o filtro (contém, sem caixa).
    fn visible(&self, cx: &App) -> Result<Vec<(String, bool)>, String> {
        let q = self.filter.read(cx).value().trim().to_lowercase();
        let keep = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
        match (self.show_roles, &self.users, &self.roles) {
            (false, Load::Ready(u), _) => Ok(u.iter().filter(|u| self.show_disabled || !u.disabled).filter(|u| keep(&u.name) || u.display_name.as_deref().is_some_and(keep) || u.email.as_deref().is_some_and(keep)).map(|u| (u.name.clone(), u.disabled)).collect()),
            (true, _, Load::Ready(r)) => Ok(r.iter().filter(|r| keep(&r.name)).map(|r| (r.name.clone(), false)).collect()),
            (false, Load::Failed(e), _) | (true, _, Load::Failed(e)) => Err(e.clone()),
            _ => Err(t!("tree.loading").to_string()),
        }
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
                    .when(!show_roles, |d| {
                        d.child(Button::new("adm-disabled").label(t!("admin.show_disabled").to_string()).ghost().small().selected(self.show_disabled).on_click(cx.listener(|t, _, _, cx| {
                            t.show_disabled = !t.show_disabled;
                            cx.notify();
                        })))
                    })
                    .child(Button::new("adm-reload").label("↻").ghost().small().tooltip(t!("admin.reload").to_string()).on_click(cx.listener(|t, _, _, cx| t.reload(cx)))),
            )
            .child(Input::new(&self.filter))
            .when(truncated, |d| d.child(div().text_xs().text_color(theme.warning).child(t!("admin.truncated", n = SHOW_LIMIT).to_string())))
            .child(match items {
                Err(msg) => div().text_sm().text_color(theme.muted_foreground).child(msg).into_any_element(),
                Ok(items) => {
                    let n = items.len();
                    let selected = self.stack.last().cloned();
                    uniform_list("adm-list", n, cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                        let theme = cx.theme();
                        range
                            .map(|i| {
                                let (name, disabled) = items[i].clone();
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
                                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                                    .when(disabled, |d| d.child(div().text_xs().child(t!("admin.disabled").to_string())))
                                    .on_click(cx.listener(move |t, _, _, cx| {
                                        // Clicar na lista recomeça a navegação.
                                        t.stack.clear();
                                        t.open(target.clone(), cx);
                                    }))
                            })
                            .collect()
                    }))
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
            .child(div().text_lg().child(match &top {
                Target::User(u) => u.clone(),
                Target::Role(r) => r.clone(),
            }));
        let body = match top {
            Target::User(u) => self.render_user(&u, cx),
            Target::Role(r) => self.render_role(&r, cx),
        };
        v_flex().id("adm-details").size_full().p_4().overflow_y_scroll().child(header).child(body).into_any_element()
    }

    fn render_user(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
        let mut out = v_flex().gap_1();
        if let Load::Ready(us) = &self.users
            && let Some(u) = us.iter().find(|u| u.name == name)
        {
            let status = if u.disabled {
                t!("admin.disabled")
            } else if u.locked {
                t!("admin.locked")
            } else {
                t!("admin.active")
            };
            let mut auth: Vec<String> = Vec::new();
            if u.has_password {
                auth.push(t!("admin.password").to_string());
            }
            if u.has_rsa_public_key {
                auth.push("key pair".into());
            }
            if u.has_mfa {
                auth.push("MFA".into());
            }
            auth.extend(u.user_type.clone());
            out = out.child(section(t!("admin.properties").to_string(), cx)).child(
                DescriptionList::horizontal()
                    .columns(2)
                    .item(t!("admin.display_name").to_string(), dash(&u.display_name), 1)
                    .item(t!("admin.email").to_string(), dash(&u.email), 1)
                    .item(t!("admin.default_role").to_string(), dash(&u.default_role), 1)
                    .item(t!("admin.default_warehouse").to_string(), dash(&u.default_warehouse), 1)
                    .item(t!("admin.default_namespace").to_string(), dash(&u.default_namespace), 1)
                    .item(t!("admin.status").to_string(), status.to_string(), 1)
                    .item(t!("admin.created").to_string(), dash(&u.created_on), 1)
                    .item(t!("admin.last_login").to_string(), u.last_success_login.clone().unwrap_or_else(|| t!("admin.never").to_string()), 1)
                    .item(t!("admin.auth").to_string(), if auth.is_empty() { "—".to_string() } else { auth.join(", ") }, 2),
            );
        }
        out = out.child(section(t!("admin.direct_roles").to_string(), cx));
        match self.user_roles.get(name) {
            None | Some(Load::Loading) => out = out.child(t!("tree.loading").to_string()),
            Some(Load::Failed(m)) => out = out.child(div().text_color(theme.danger).child(m.clone())),
            Some(Load::Ready((direct, eff))) => {
                let (direct, eff) = (direct.clone(), clone_load(eff));
                out = out.child(tags("d", &direct, cx));
                out = out.child(section(t!("admin.effective_roles").to_string(), cx));
                match eff {
                    Load::Loading => out = out.child(t!("tree.loading").to_string()),
                    Load::Failed(m) => out = out.child(div().text_color(theme.danger).child(m)),
                    Load::Ready(e) if e.is_empty() => out = out.child(div().text_color(theme.muted_foreground).child(t!("admin.none").to_string())),
                    Load::Ready(e) => {
                        for (i, r) in e.iter().enumerate() {
                            out = out.child(
                                h_flex()
                                    .gap_2()
                                    .child(role_tag(("eff", i), &r.name, cx))
                                    .child(div().text_sm().text_color(theme.muted_foreground).child(r.path.join(" → "))),
                            );
                        }
                    }
                }
            }
        }
        out = out.child(self.render_activity(name, cx));
        out.into_any_element()
    }

    fn render_activity(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut out = v_flex()
            .gap_1()
            .child(section(t!("admin.activity").to_string(), cx))
            .child(div().text_xs().text_color(theme.muted_foreground).child(t!("admin.activity_lag").to_string()));
        let a = match self.activity.get(name) {
            None | Some(Load::Loading) => return out.child(t!("tree.loading").to_string()).into_any_element(),
            Some(Load::Failed(m)) => return out.child(div().text_color(theme.danger).child(m.clone())).into_any_element(),
            Some(Load::Ready(a)) => a.clone(),
        };
        let q = &a.queries;
        out = out
            .child(t!("admin.logins", ok = a.logins.ok, failed = a.logins.failed).to_string())
            .child(t!("admin.queries", n = q.total, last = q.last.as_deref().unwrap_or("—"), time = human_ms(q.elapsed_ms)).to_string())
            .child(div().pt_2().text_sm().child(t!("admin.queries_per_day").to_string()))
            .child(
                div().h(px(160.)).child(
                    BarChart::new(q.per_day.clone())
                        .band(|d: &(String, u64)| SharedString::from(d.0.get(5..).unwrap_or(&d.0).to_string()))
                        .value(|d: &(String, u64)| d.1 as f64)
                        .tick_margin(5)
                        .tooltip_title(|d: &(String, u64)| d.0.clone().into()),
                ),
            );
        let whs = v_flex().flex_1().gap_1().child(section(t!("admin.top_warehouses").to_string(), cx)).children(q.top_warehouses.iter().map(|(w, n)| div().text_sm().child(format!("{w} · {n}"))));
        let mut roles = v_flex().flex_1().gap_1().child(section(t!("admin.top_roles").to_string(), cx));
        for (i, (r, n)) in q.top_roles.iter().enumerate() {
            roles = roles.child(h_flex().gap_2().child(role_tag(("top", i), r, cx)).child(div().text_sm().child(n.to_string())));
        }
        out = out.child(h_flex().items_start().gap_4().child(whs).child(roles)).child(section(t!("admin.recent_logins").to_string(), cx));
        for e in &a.logins.recent {
            let line = [Some(e.at.clone()), Some(if e.success { "✓".into() } else { "✗".into() }), e.factor.clone(), e.client.clone(), e.ip.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            out = out.child(
                v_flex()
                    .child(div().text_sm().when(!e.success, |d| d.text_color(theme.danger)).child(line))
                    .when_some(e.error.clone(), |d, err| d.child(div().text_xs().text_color(theme.danger).child(err))),
            );
        }
        out.into_any_element()
    }

    fn render_role(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut out = v_flex().gap_1();
        if let Load::Ready(rs) = &self.roles
            && let Some(r) = rs.iter().find(|r| r.name == name)
        {
            out = out.child(
                DescriptionList::horizontal()
                    .columns(2)
                    .item(t!("admin.owner").to_string(), r.owner.clone().unwrap_or_else(|| "—".into()), 1)
                    .item(t!("admin.comment").to_string(), r.comment.clone().unwrap_or_else(|| "—".into()), 1),
            );
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
                    .child(section(t!("admin.granted_to").to_string(), cx))
                    .child(granted)
                    .child(section(t!("admin.inherits").to_string(), cx))
                    .child(tags("inh", &admin::inherited_roles(&grants), cx))
                    .child(section(t!("admin.privileges", n = n).to_string(), cx))
                    // Virtualizada: SYSADMIN pode ter milhares de grants.
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

fn clone_load<T: Clone>(l: &Load<T>) -> Load<T> {
    match l {
        Load::Loading => Load::Loading,
        Load::Ready(v) => Load::Ready(v.clone()),
        Load::Failed(m) => Load::Failed(m.clone()),
    }
}

fn section(title: String, cx: &App) -> Div {
    div().pt_3().pb_1().text_xs().text_color(cx.theme().muted_foreground).child(title.to_uppercase())
}

/// Tag clicável que navega para a role.
fn role_tag(id: impl Into<ElementId>, name: &str, cx: &mut Context<AdminTab>) -> Stateful<Div> {
    let target = Target::Role(name.to_string());
    div().id(id).cursor_pointer().child(Tag::secondary().small().child(name.to_string())).on_click(cx.listener(move |t, _, _, cx| t.open(target.clone(), cx)))
}

fn user_tag(id: impl Into<ElementId>, name: &str, cx: &mut Context<AdminTab>) -> Stateful<Div> {
    let target = Target::User(name.to_string());
    div()
        .id(id)
        .cursor_pointer()
        .child(Tag::info().small().child(h_flex().gap_1().child(Icon::new(IconName::CircleUser).xsmall()).child(name.to_string())))
        .on_click(cx.listener(move |t, _, _, cx| t.open(target.clone(), cx)))
}

/// Linha de tags de role; `prefix` deixa os ids únicos por seção.
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
fn human_ms(ms: u64) -> String {
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
    #[::core::prelude::v1::test]
    fn human_ms_units() {
        assert_eq!(super::human_ms(12_500), "12.5 s");
        assert_eq!(super::human_ms(90_000), "1.5 min");
        assert_eq!(super::human_ms(4_000_000), "1.1 h");
    }
}
