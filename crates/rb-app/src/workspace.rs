use rust_i18n::t;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::component::{Icon, InteractiveElementExt as _, Selectable as _, Sizable as _, Theme};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::{self, ConfigFile, ConnectionConfig, Language, ThemeChoice};
use rb_core::dialect::Dialect;
use rb_core::lexer::quote_always;
use rb_core::quick_open::{Hit, HitKind};
use rb_core::secrets;
use rb_core::session::Credentials;
use rb_core::ssh::{self, SshEnv};

use crate::admin_tab::{AdminTab, Target};
use crate::appearance::{self, Appearance, palette};
use crate::dbt_store::DbtStore;
use crate::frame::{context_parts, env_color};
use crate::indexer::Indexer;
use crate::lang::{self, Lang};
use crate::meta::{ConnTarget, MetaStore};
use crate::picker::Picker;
use crate::schema_tree::SchemaTree;
use crate::settings;
use crate::sql_tab::SqlTab;
use crate::table_tab::TableTab;
use crate::ui::Load;
use crate::warehouse_tab::WarehouseTab;
use crate::{CloseTab, Db, EditConnection, NewSqlTab, QuickOpen, Refresh, SelectTab, ShowHistory, conn_dialog, db_filter, history_popup, quick_open};
use rb_core::picker::{list_sql, parse_available_roles, push_recent};

/// Open connection: its own metadata session and tree; each tab keeps its own session.
pub struct Open {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
    pub tree: Entity<SchemaTree>,
    /// Quick search index (Snowflake only).
    pub indexer: Option<Entity<Indexer>>,
    /// Linked dbt project (Snowflake only; no folder = "not configured").
    pub dbt: Option<Entity<DbtStore>>,
}

#[derive(Clone)]
pub enum TabView {
    Sql(Entity<SqlTab>),
    Table(Entity<TableTab>),
    Admin(Entity<AdminTab>),
    Warehouses(Entity<WarehouseTab>),
}

impl TabView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            TabView::Sql(t) => t.read(cx).title(),
            TabView::Table(t) => t.read(cx).title(),
            TabView::Admin(t) => t.read(cx).title(),
            TabView::Warehouses(t) => t.read(cx).title(),
        }
    }

    fn conn_id(&self, cx: &App) -> String {
        match self {
            TabView::Sql(t) => t.read(cx).conn_id().to_string(),
            TabView::Table(t) => t.read(cx).conn_id().to_string(),
            TabView::Admin(t) => t.read(cx).conn_id().to_string(),
            TabView::Warehouses(t) => t.read(cx).conn_id().to_string(),
        }
    }
}

fn logo(cfg: &ConnectionConfig) -> Icon {
    kind_logo(cfg.snowflake.is_some())
}

pub fn kind_logo(snowflake: bool) -> Icon {
    if snowflake { Icon::default().path("icons/snowflake-logo.svg").text_color(rgb(0x29b5e8)) } else { Icon::default().path("icons/postgresql.svg").text_color(rgb(0x4169e1)) }
}

pub struct Workspace {
    focus: FocusHandle,
    /// The open tab's area: a click there takes focus from the tree, so ⌘R refreshes the tab, not a tree node.
    main_focus: FocusHandle,
    pub connections: Vec<ConnectionConfig>,
    pub open: Vec<Open>,
    /// Connection highlighted in the sidebar: receives new tabs and shows the tree.
    current: Option<String>,
    /// Current connection's tree collapsed (click the connection again).
    /// Objects panel open (rail → Objects).
    objects_open: bool,
    /// In the Users & Roles tab the database tree doesn't help: closed unless asked.
    objects_on_admin: bool,
    /// Tab bar: width measured in the last frame, scroll, and the tab already scrolled into view.
    tab_bar_w: Rc<std::cell::Cell<f32>>,
    tab_scroll: ScrollHandle,
    revealed_tab: Option<usize>,
    /// Left button pressed on the title row: the next movement drags the window.
    drag_armed: bool,
    pub tabs: Vec<TabView>,
    pub active_tab: usize,
    /// Connection being opened (name) and its task: dropping the task cancels the attempt.
    connecting: Option<String>,
    connect_task: Option<Task<()>>,
    /// Connections to reopen at startup, one after the other (SSO opens one window at a time).
    reconnect_queue: VecDeque<String>,
    /// Last list of roles/warehouses/… per "connection:kind": the picker opens with it instantly.
    pick_cache: HashMap<String, Vec<String>>,
    /// Reopening saved table tabs: they are already in tables.toml, nothing to save.
    restoring: bool,
    _quit: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = match config::load(&config::connections_path()) {
            Ok(f) => f.connections,
            Err(e) => {
                window.push_notification(Notification::error(e.to_string()), cx);
                Vec::new()
            }
        };
        cx.observe_window_appearance(window, |_, window, cx| appearance::system_changed(window, cx)).detach();
        // Reopens the connections that were open when the app closed (with the ID token, no browser).
        let ids: Vec<String> = connections.iter().map(|c| c.id.clone()).collect();
        let reconnect_queue = config::reconnect_order(&settings::get(cx).last_open, &ids).into();
        cx.defer_in(window, |ws, window, cx| ws.reconnect_next(window, cx));
        // Any way out (⌘Q, Dock, closing the window) saves the scripts: autosave waits a second.
        let quit = cx.on_app_quit(|ws, cx| {
            ws.save_all(cx);
            settings::flush(cx);
            async {}
        });
        let ws = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            ws.update(cx, |ws, cx| ws.save_all(cx)).ok();
            true
        });
        Workspace {
            reconnect_queue,
            focus: cx.focus_handle(),
            main_focus: cx.focus_handle(),
            connections,
            open: Vec::new(),
            current: None,
            objects_open: true,
            objects_on_admin: false,
            tab_bar_w: Rc::default(),
            tab_scroll: ScrollHandle::new(),
            revealed_tab: None,
            drag_armed: false,
            tabs: Vec::new(),
            active_tab: 0,
            connecting: None,
            connect_task: None,
            pick_cache: HashMap::new(),
            restoring: false,
            _quit: quit,
        }
    }

    fn conn_ix(&self, id: &str) -> Option<usize> {
        self.connections.iter().position(|c| c.id == id)
    }

    fn edit_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.conn_ix(id) {
            conn_dialog::open(Some(self.connections[ix].clone()), window, cx);
        }
    }

    fn open_conn(&self, id: &str) -> Option<&Open> {
        self.open.iter().find(|o| o.conn.cfg.id == id)
    }

    fn current_open(&self) -> Option<&Open> {
        self.current.as_deref().and_then(|id| self.open_conn(id))
    }

    /// Creates or updates (same id). Empty password/passphrase keep the Keychain; without a tunnel, the
    /// stored passphrase is deleted. Editing an open connection reconnects. Returns `false` if refused.
    pub fn save_connection(&mut self, mut cfg: ConnectionConfig, password: String, passphrase: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let id = cfg.id.clone();
        let reconnect = self.open_conn(&id).is_some();
        if reconnect && self.has_pending_edits(&id, cx) {
            window.push_notification(Notification::error(t!("ws.pending_before_edit").to_string()), cx);
            return false;
        }
        // The dialog doesn't edit the databases filter: keeps what was already saved.
        if let (Some(new), Some(old)) = (cfg.snowflake.as_mut(), self.connections.iter().find(|c| c.id == id).and_then(|c| c.snowflake.as_ref())) {
            new.db_filter = old.db_filter.clone();
        }
        let has_ssh = cfg.ssh.is_some();
        let is_sf = cfg.snowflake.is_some();
        match self.connections.iter_mut().find(|c| c.id == id) {
            Some(c) => *c = cfg,
            None => self.connections.push(cfg),
        }
        let file = ConfigFile { connections: self.connections.clone() };
        cx.spawn_in(window, async move |this, cx| {
            let res = cx
                .background_spawn({
                    let id = id.clone();
                    async move {
                        config::save(&config::connections_path(), &file)?;
                        if !password.is_empty() {
                            secrets::set_password(&id, &password)?;
                        }
                        // The dialog's passphrase belongs to the SSH key (Postgres) or the Snowflake key.
                        let (keep, other) = if is_sf { (secrets::key_account(&id), secrets::ssh_account(&id)) } else { (secrets::ssh_account(&id), secrets::key_account(&id)) };
                        if !(is_sf || has_ssh) {
                            secrets::delete_password(&keep)?;
                        } else if !passphrase.is_empty() {
                            secrets::set_password(&keep, &passphrase)?;
                        }
                        secrets::delete_password(&other)?;
                        Ok::<_, rb_core::DbError>(())
                    }
                })
                .await;
            this.update_in(cx, |ws, window, cx| match res {
                Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                Ok(()) if reconnect => {
                    ws.disconnect(&id, cx);
                    ws.connect(&id, window, cx);
                }
                Ok(()) => {}
            })
        })
        .detach();
        cx.notify();
        true
    }

    /// Closed: connects. Open: becomes current; if it already was, collapses/expands the tree.
    fn connect(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // By id, not index: the list may change between a click being armed and running.
        let Some(ix) = self.conn_ix(id) else { return };
        let id = id.to_string();
        if self.open_conn(&id).is_some() {
            // The frame follows the active tab: switching connection goes to one of its tabs (or opens a script),
            // otherwise the context bar would show one connection and ⌘↵ would run on another.
            self.current = Some(id.clone());
            self.objects_open = true;
            match self.tabs.iter().rposition(|t| t.conn_id(cx) == id) {
                Some(ix) => self.select_tab(ix, cx),
                None => self.new_sql_tab(window, cx),
            }
            cx.notify();
            return;
        }
        // Another attempt in progress (SSO waiting, stuck network): this one replaces it.
        self.cancel_connect(cx);
        self.connecting = Some(self.connections[ix].name.clone());
        cx.notify();
        let cfg = self.connections[ix].clone();
        let engine = cx.global::<Db>().0.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let id = cfg.id.clone();
                let has_ssh = cfg.ssh.is_some();
                let is_sf = cfg.snowflake.is_some();
                let creds: Arc<Credentials> = cx
                    .background_spawn(async move {
                        Ok::<_, rb_core::DbError>(Credentials {
                            password: secrets::get_password(&id)?,
                            ssh_passphrase: if has_ssh { secrets::get_password(&secrets::ssh_account(&id))? } else { None },
                            key_passphrase: if is_sf { secrets::get_password(&secrets::key_account(&id))? } else { None },
                        })
                    })
                    .await
                    .map(Arc::new)?;
                let session = engine.connect(&cfg, creds.clone()).await?;
                // A missing or broken OrgConfig doesn't block the connection: proceeds without conventions and warns.
                let name = cfg.snowflake.as_ref().and_then(|s| s.configuration.clone());
                let (org, org_err) = match cx.background_spawn(async move { rb_core::org_config::resolve(name.as_deref()) }).await {
                    Ok(a) => (a, None),
                    Err(e) => (rb_core::org_config::OrgConfig::none(), Some(e)),
                };
                Ok::<_, rb_core::DbError>((ConnTarget { cfg, creds, org: Arc::new(org) }, session, org_err))
            }
            .await;
            this.update_in(cx, |ws, window, cx| {
                ws.connecting = None;
                ws.connect_task = None;
                match result {
                    Ok((conn, session, org_err)) => {
                        if let Some(e) = org_err {
                            window.push_notification(Notification::error(t!("ws.config_failed", error = e).to_string()), cx);
                        }
                        let id = conn.cfg.id.clone();
                        let db_filter = conn.cfg.snowflake.as_ref().map(|s| s.db_filter.clone()).unwrap_or_default();
                        let meta = cx.new(|cx| {
                            let mut m = MetaStore::new(Arc::new(session), cx);
                            m.cache.db_filter = db_filter;
                            m
                        });
                        cx.observe(&meta, |_, _, cx| cx.notify()).detach();
                        let ws_weak = cx.entity().downgrade();
                        let tree_id = id.clone();
                        let open: crate::schema_tree::OpenTable = Rc::new(move |schema, table, window, cx| {
                            ws_weak.update(cx, |ws, cx| ws.open_table(&tree_id, schema, table, window, cx)).ok();
                        });
                        let tree = cx.new(|cx| SchemaTree::new(meta.clone(), open, cx));
                        let indexer = conn.cfg.snowflake.is_some().then(|| cx.new(|cx| Indexer::new(conn.clone(), meta.clone(), cx)));
                        let dbt = conn.cfg.snowflake.as_ref().map(|sf| {
                            let cfg = sf.dbt.clone();
                            cx.new(|cx| DbtStore::new(cfg, cx))
                        });
                        ws.open.push(Open { conn, meta, tree, indexer, dbt });
                        ws.save_open(cx);
                        ws.current = Some(id.clone());
                        ws.objects_open = true;
                        ws.on_connected(&id, window, cx);
                    }
                    Err(rb_core::DbError::HostKeyUnknown { host, port, fingerprint, key }) => ws.confirm_trust(&id, host, port, fingerprint, key, window, cx),
                    Err(e) => window.push_notification(Notification::error(t!("ws.connect_failed", error = e).to_string()), cx),
                }
                ws.reconnect_next(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.connect_task = Some(task);
    }

    /// Next in the reconnect queue (each waits for the previous to finish).
    fn reconnect_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(id) = self.reconnect_queue.pop_front() {
            if self.conn_ix(&id).is_some() && self.open_conn(&id).is_none() {
                self.connect(&id, window, cx);
                return;
            }
        }
    }

    /// Writes `tables.toml` of a connection (its open table tabs, in tab order).
    fn save_tables(&self, conn_id: &str, cx: &App) {
        if self.restoring {
            return;
        }
        let tables: Vec<[String; 2]> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                TabView::Table(t) if t.read(cx).conn_id() == conn_id => Some([t.read(cx).schema.clone(), t.read(cx).table.clone()]),
                _ => None,
            })
            .collect();
        if let Err(e) = config::save_open_tables(&config::scripts_dir().join(conn_id), &tables) {
            tracing::warn!("tables.toml: {e}");
        }
    }

    /// Remembers which connections are open, for the next startup.
    fn save_open(&self, cx: &mut App) {
        let ids: Vec<String> = self.open.iter().map(|o| o.conn.cfg.id.clone()).collect();
        settings::update(cx, |s| s.last_open = ids);
    }

    /// Abandons the connection attempt: the task is dropped and the result, if it arrives, is discarded.
    /// ponytail: an SSO login already open in the browser stays there; the app just stops waiting.
    fn cancel_connect(&mut self, cx: &mut Context<Self>) {
        if self.connect_task.take().is_some() {
            // Cancelling also gives up the remaining automatic reconnects.
            self.reconnect_queue.clear();
            self.connecting = None;
            cx.notify();
        }
    }

    /// "Connecting to X… · Cancel".
    fn connecting_row(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let name = self.connecting.clone()?;
        Some(
            h_flex()
                .gap_2()
                .items_center()
                .child(t!("ws.connecting_to", name = name).to_string())
                .child(Button::new("cancel-connect").label(t!("common.cancel").to_string()).ghost().xsmall().on_click(cx.listener(|ws, _, _, cx| ws.cancel_connect(cx)))),
        )
    }

    pub fn open_table(&mut self, conn_id: &str, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.open_conn(conn_id) else { return };
        let title: SharedString = format!("{schema}.{table}").into();
        // Focus leaves the tree, so ⌘R refreshes the table.
        self.main_focus.focus(window, cx);
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Table(_)) && t.conn_id(cx) == conn_id && t.title(cx) == title) {
            self.select_tab(ix, cx);
            return;
        }
        let (conn, meta, dbt) = (o.conn.clone(), o.meta.clone(), o.dbt.clone());
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        let open_query: crate::table_tab::OpenQuery = Rc::new(move |sql, window, cx| {
            ws.update(cx, |ws, cx| ws.open_query(&id, &sql, window, cx)).ok();
        });
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        let open_table: crate::schema_tree::OpenTable = Rc::new(move |schema, table, window, cx| {
            ws.update(cx, |ws, cx| ws.open_table(&id, schema, table, window, cx)).ok();
        });
        let run_query = self.run_query_cb(conn_id, cx);
        let tab = cx.new(|cx| TableTab::new(schema, table, conn, meta, dbt, open_query, run_query, open_table, window, cx));
        self.tabs.push(TabView::Table(tab));
        self.select_tab(self.tabs.len() - 1, cx);
        self.save_tables(conn_id, cx);
    }

    /// One users & roles tab per connection: if it exists, just focus it.
    fn open_admin(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.open_conn(conn_id).map(|o| o.conn.clone()) else { return };
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Admin(_)) && t.conn_id(cx) == conn_id) {
            self.select_tab(ix, cx);
            return;
        }
        let tab = cx.new(|cx| AdminTab::new(conn, window, cx));
        self.tabs.push(TabView::Admin(tab));
        self.select_tab(self.tabs.len() - 1, cx);
    }

    /// One warehouses tab per connection; a role/user clicked there opens Users & Roles.
    fn open_warehouses(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.open_conn(conn_id).map(|o| o.conn.clone()) else { return };
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Warehouses(_)) && t.conn_id(cx) == conn_id) {
            self.select_tab(ix, cx);
            return;
        }
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        let goto: crate::warehouse_tab::Goto = Rc::new(move |target, window, cx| {
            ws.update(cx, |ws, cx| ws.admin_goto(&id, target, window, cx)).ok();
        });
        let tab = cx.new(|cx| WarehouseTab::new(conn, goto, window, cx));
        self.tabs.push(TabView::Warehouses(tab));
        self.select_tab(self.tabs.len() - 1, cx);
    }

    /// Opens (or focuses) Users & Roles already on the requested role/user.
    fn admin_goto(&mut self, conn_id: &str, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.open_admin(conn_id, window, cx);
        if let Some(TabView::Admin(t)) = self.tabs.get(self.active_tab) {
            t.update(cx, |t, cx| t.goto(target, cx));
        }
    }

    /// Quick search (⌘K) on the current connection.
    fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.current_open() else {
            window.push_notification(Notification::error(t!("ws.open_connection_to_search").to_string()), cx);
            return;
        };
        let (meta, indexer, id) = (o.meta.clone(), o.indexer.clone(), o.conn.cfg.id.clone());
        // The index refreshes slowly on its own; opening ⌘K is when freshness matters.
        if let Some(ix) = &indexer {
            ix.update(cx, |ix, cx| ix.refresh_if_stale(cx));
        }
        let tabs: Vec<quick_open::OpenTab> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, t)| {
                let conn = self.connections.iter().find(|c| c.id == t.conn_id(cx)).map_or(String::new(), |c| c.name.clone());
                let body = match t {
                    TabView::Sql(s) => Some(s.read(cx).editor.read(cx).value().to_string()),
                    _ => None,
                };
                quick_open::OpenTab { ix, title: format!("{conn} · {}", t.title(cx)), body }
            })
            .collect();
        let ws = cx.entity().downgrade();
        let ws2 = ws.clone();
        quick_open::open(
            meta,
            indexer,
            move |hit, window, cx| {
                ws.update(cx, |ws, cx| ws.reveal_hit(&id, hit, window, cx)).ok();
            },
            tabs,
            move |ix, _, cx| {
                ws2.update(cx, |ws, cx| ws.select_tab(ix, cx)).ok();
            },
            window,
            cx,
        );
    }

    /// ⌘Y: current connection's history.
    fn show_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.current_open() else {
            window.push_notification(Notification::error(t!("ws.open_connection_to_search").to_string()), cx);
            return;
        };
        let (id, name) = (o.conn.cfg.id.clone(), o.conn.cfg.name.clone());
        let path = rb_core::history::history_path(&id);
        let ws = cx.entity().downgrade();
        history_popup::open(
            name,
            path,
            move |sql, new_tab, window, cx| {
                ws.update(cx, |ws, cx| ws.use_history(&id, &sql, new_tab, window, cx)).ok();
            },
            window,
            cx,
        );
    }

    /// Inserts the query into the active SQL tab (if it's of this connection) or a new script.
    fn use_history(&mut self, conn_id: &str, sql: &str, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let same_conn = |t: &Entity<SqlTab>, cx: &App| t.read(cx).conn_id() == conn_id;
        let target = match self.tabs.get(self.active_tab) {
            Some(TabView::Sql(t)) if !new_tab && same_conn(t, cx) => Some(t.clone()),
            _ => {
                self.current = Some(conn_id.to_string());
                self.new_sql_tab(window, cx);
                match self.tabs.last() {
                    Some(TabView::Sql(t)) if same_conn(t, cx) => Some(t.clone()),
                    _ => None,
                }
            }
        };
        if let Some(t) = target {
            t.update(cx, |t, cx| t.insert_text(sql, window, cx));
        }
    }

    /// Shows the object in the tree; table/view also opens the tab.
    fn reveal_hit(&mut self, conn_id: &str, hit: Hit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = self.open_conn(conn_id).map(|o| o.tree.clone()) else { return };
        self.current = Some(conn_id.to_string());
        self.objects_open = true;
        tree.update(cx, |t, cx| t.reveal(hit.tree_path(), cx));
        if matches!(hit.kind, HitKind::Table | HitKind::View)
            && let Some(schema) = hit.schema_key()
        {
            self.open_table(conn_id, schema, hit.name, window, cx);
        }
        cx.notify();
    }

    /// New script already holding the SQL (Query button of the table tab).
    /// Callback for tabs: a new script with `sql`, run right away.
    fn run_query_cb(&self, conn_id: &str, cx: &Context<Self>) -> crate::table_tab::OpenQuery {
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        Rc::new(move |sql, window, cx| {
            ws.update(cx, |ws, cx| {
                ws.open_query(&id, &sql, window, cx);
                if let Some(TabView::Sql(t)) = ws.tabs.get(ws.active_tab) {
                    t.update(cx, |t, cx| t.run(true, window, cx));
                }
            })
            .ok();
        })
    }

    fn open_query(&mut self, conn_id: &str, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let created = config::new_script(&config::scripts_dir().join(conn_id)).and_then(|p| config::write_private(&p, sql).map(|_| p).map_err(|e| rb_core::DbError::Io(e.to_string())));
        match created {
            Ok(path) => self.open_sql_file(path, conn_id, window, cx),
            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
        }
    }

    /// New script on the current connection.
    pub fn new_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current_open().map(|o| o.conn.cfg.id.clone()) else { return };
        match config::new_script(&config::scripts_dir().join(&id)) {
            Ok(path) => self.open_sql_file(path, &id, window, cx),
            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
        }
    }

    /// Choosing a tab makes its connection the current one.
    fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active_tab = ix;
        if let Some(t) = self.tabs.get(ix) {
            self.current = Some(t.conn_id(cx));
        }
        cx.notify();
    }

    fn has_pending_edits(&self, conn_id: &str, cx: &App) -> bool {
        self.tabs.iter().any(|t| matches!(t, TabView::Table(t) if t.read(cx).conn_id() == conn_id && t.read(cx).has_pending(cx)))
    }

    /// Saves now the text of all open scripts (autosave waits 1 s).
    fn save_all(&self, cx: &App) {
        for t in &self.tabs {
            if let TabView::Sql(t) = t {
                t.read(cx).save_now(cx);
            }
        }
    }

    fn open_sql_file(&mut self, path: PathBuf, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((conn, meta)) = self.open_conn(conn_id).map(|o| (o.conn.clone(), o.meta.clone())) else { return };
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        let open_table: crate::schema_tree::OpenTable = Rc::new(move |schema, table, window, cx| {
            ws.update(cx, |ws, cx| ws.open_table(&id, schema, table, window, cx)).ok();
        });
        let run_query = self.run_query_cb(conn_id, cx);
        let tab = cx.new(|cx| SqlTab::new(path, conn, meta, open_table, run_query, window, cx));
        tab.read(cx).editor.clone().update(cx, |e, cx| e.focus(window, cx));
        self.tabs.push(TabView::Sql(tab));
        self.select_tab(self.tabs.len() - 1, cx);
    }

    /// Closing doesn't delete: the file goes to scripts/closed/.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else { return };
        if let TabView::Table(t) = tab
            && t.read(cx).has_pending(cx)
        {
            window.push_notification(Notification::error(crate::ui::keys(&t!("ws.pending_before_close"))), cx);
            return;
        }
        let conn_id = self.tabs[ix].conn_id(cx);
        match self.tabs.remove(ix) {
            TabView::Sql(t) => {
                let t = t.read(cx);
                t.save_now(cx);
                if let Err(e) = config::archive_script(&t.path) {
                    tracing::warn!("failed to archive script: {e}");
                }
            }
            TabView::Table(_) => self.save_tables(&conn_id, cx),
            _ => {}
        }
        let ix = self.active_tab.min(self.tabs.len().saturating_sub(1));
        self.select_tab(ix, cx);
    }

    /// Reopens the connection's scripts (sorted) or creates a new one.
    fn on_connected(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let files = config::conn_scripts(&config::scripts_dir(), id).unwrap_or_else(|e| {
            window.push_notification(Notification::error(e.to_string()), cx);
            Vec::new()
        });
        let tables = config::load_open_tables(&config::scripts_dir().join(id));
        if files.is_empty() && tables.is_empty() {
            self.new_sql_tab(window, cx);
        }
        for f in files {
            self.open_sql_file(f, id, window, cx);
        }
        // Already in the file: no rewrite per reopened tab.
        self.restoring = true;
        for [schema, table] in tables {
            self.open_table(id, schema, table, window, cx);
        }
        self.restoring = false;
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            let Some(o) = self.current_open() else { return self.render_connections(cx) };
            let msg = format!("{} — {}", o.conn.cfg.name, crate::ui::keys(&t!("ws.new_script_hint")));
            return v_flex().flex_1().h_full().items_center().justify_center().child(msg).into_any_element();
        }
        let content = match &self.tabs[self.active_tab] {
            TabView::Sql(t) => t.clone().into_any_element(),
            TabView::Table(t) => t.clone().into_any_element(),
            TabView::Admin(t) => t.clone().into_any_element(),
            TabView::Warehouses(t) => t.clone().into_any_element(),
        };
        // min_w_0: without it the flex item measures at least the content width (all the grid's columns)
        // and overflows the window instead of letting the grid scroll horizontally.
        div()
            .track_focus(&self.main_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, _, window, cx| {
                    // Fields inside already took focus on their own mouse down: keep it.
                    if !ws.main_focus.contains_focused(window, cx) {
                        ws.main_focus.focus(window, cx);
                    }
                }),
            )
            .flex_1()
            .min_w_0()
            .min_h_0()
            .size_full()
            .child(content)
            .into_any_element()
    }

    fn confirm_remove(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.conn_ix(id).map(|ix| self.connections[ix].clone()) else { return };
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ws, id) = (ws.clone(), c.id.clone());
            dialog.title(t!("ws.remove_title").to_string()).w(px(420.)).child(t!("ws.remove_confirm", name = c.name).to_string()).footer(
                h_flex().justify_end().gap_2().child(Button::new("rm-cancel").label(t!("common.cancel").to_string()).on_click(|_, window, cx| window.close_dialog(cx))).child(
                    Button::new("rm-ok").label(t!("ws.remove").to_string()).danger().on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        ws.update(cx, |ws, cx| ws.remove_connection(&id, window, cx)).ok();
                    }),
                ),
            )
        });
    }

    /// Removes the connection from the TOML and the password from the Keychain. If open, disconnects first.
    fn remove_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_conn(id).is_some() {
            if self.has_pending_edits(id, cx) {
                window.push_notification(Notification::error(t!("ws.pending_before_remove").to_string()), cx);
                return;
            }
            self.disconnect(id, cx);
        }
        self.connections.retain(|c| c.id != id);
        let file = ConfigFile { connections: self.connections.clone() };
        let id = id.to_string();
        cx.spawn_in(window, async move |_, cx| {
            let res = cx
                .background_spawn(async move {
                    config::save(&config::connections_path(), &file)?;
                    secrets::delete_password(&id)?;
                    secrets::delete_password(&secrets::key_account(&id))?;
                    secrets::delete_password(&secrets::ssh_account(&id))
                })
                .await;
            if let Err(e) = res {
                cx.update(|window, cx| window.push_notification(Notification::error(e.to_string()), cx))?;
            }
            anyhow::Ok(())
        })
        .detach();
        cx.notify();
    }

    /// Saves the scripts, closes the connection's tabs and drops the session (and the tunnel, if it's the last use).
    /// Scripts stay in the connection's folder and come back on reconnect.
    fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.save_all(cx);
        let keep: Vec<bool> = self.tabs.iter().map(|t| t.conn_id(cx) != id).collect();
        let before = keep.iter().take(self.active_tab).filter(|k| **k).count();
        let mut k = keep.into_iter();
        self.tabs.retain(|_| k.next().unwrap_or(true));
        self.open.retain(|o| o.conn.cfg.id != id);
        self.save_open(cx);
        self.active_tab = before.min(self.tabs.len().saturating_sub(1));
        self.current = self.tabs.get(self.active_tab).map(|t| t.conn_id(cx)).or_else(|| self.open.first().map(|o| o.conn.cfg.id.clone()));
        cx.notify();
    }

    fn request_disconnect(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_pending_edits(id, cx) {
            window.push_notification(Notification::error(t!("ws.pending_before_disconnect").to_string()), cx);
            return;
        }
        self.disconnect(id, cx);
    }

    /// SSH host not in known_hosts: shows the fingerprint and, if the user trusts it, saves and reconnects.
    #[allow(clippy::too_many_arguments)]
    fn confirm_trust(&mut self, id: &str, host: String, port: u16, fingerprint: String, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let ws = cx.entity().downgrade();
        let id = id.to_string();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ws, host, key, id) = (ws.clone(), host.clone(), key.clone(), id.clone());
            dialog.title(t!("ws.unknown_host_title").to_string()).w(px(480.)).child(t!("ws.unknown_host_body", host = host, port = port, fingerprint = fingerprint).to_string()).footer(
                h_flex().justify_end().gap_2().child(Button::new("trust-cancel").label(t!("common.cancel").to_string()).on_click(|_, window, cx| window.close_dialog(cx))).child(
                    Button::new("trust-ok").label(t!("ws.trust").to_string()).primary().on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        // ponytail: one-line append to known_hosts on the UI thread; small file.
                        match ssh::trust_host(&SshEnv::system().known_hosts, &host, port, &key) {
                            Ok(()) => {
                                ws.update(cx, |ws, cx| ws.connect(&id, window, cx)).ok();
                            }
                            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                        }
                    }),
                ),
            )
        });
    }

    /// Lists warehouses or roles (`SHOW …`) and runs `USE …` on the chosen one; the session is shared.
    /// Interface language: Auto (follows macOS) or one of the three; switches instantly.
    fn pick_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = cx.global::<Lang>().0;
        window.open_dialog(cx, move |dialog, _, _| {
            let options = [Language::Auto, Language::Pt, Language::En, Language::Es];
            let list = options.into_iter().enumerate().fold(v_flex().gap_1(), |l, (i, choice)| {
                let mark = if choice == current { "✓ " } else { "   " };
                l.child(Button::new(("lang", i)).label(format!("{mark}{}", lang::label(choice))).ghost().on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    lang::set(choice, cx);
                }))
            });
            dialog.title(t!("lang.title").to_string()).w(px(320.)).child(list)
        });
    }

    fn pick_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = cx.global::<Appearance>().0;
        window.open_dialog(cx, move |dialog, _, cx| {
            let options = [ThemeChoice::Auto, ThemeChoice::Light, ThemeChoice::Dark];
            let list = options.into_iter().enumerate().fold(v_flex().gap_1(), |l, (i, choice)| {
                let mark = if choice == current { "✓ " } else { "   " };
                l.child(Button::new(("theme", i)).label(format!("{mark}{}", appearance::label(choice, cx))).ghost().on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    appearance::set(choice, window, cx);
                }))
            });
            dialog.title(t!("theme.title").to_string()).w(px(320.)).child(list)
        });
    }

    /// Databases filter dialog (Snowflake): chips, `*` wildcard, `!` excludes.
    fn edit_db_filter(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.open_conn(conn_id).map(|o| o.meta.read(cx).cache.db_filter.clone()).unwrap_or_default();
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        db_filter::open(
            current,
            move |patterns, window, cx| {
                ws.update(cx, |ws, cx| ws.set_db_filter(&id, patterns, window, cx)).ok();
            },
            window,
            cx,
        );
    }

    /// Saves the filter to the TOML and applies it to the tree and autocomplete, without reconnecting.
    fn set_db_filter(&mut self, conn_id: &str, patterns: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sf) = self.connections.iter_mut().find(|c| c.id == conn_id).and_then(|c| c.snowflake.as_mut()) else { return };
        sf.db_filter = patterns.clone();
        if let Some(o) = self.open_conn(conn_id) {
            o.meta.update(cx, |m, cx| {
                m.cache.db_filter = patterns;
                cx.notify();
            });
        }
        let file = ConfigFile { connections: self.connections.clone() };
        cx.spawn_in(window, async move |_, cx| {
            if let Err(e) = cx.background_spawn(async move { config::save(&config::connections_path(), &file) }).await {
                cx.update(|window, cx| window.push_notification(Notification::error(e.to_string()), cx))?;
            }
            anyhow::Ok(())
        })
        .detach();
        cx.notify();
    }

    /// Role/warehouse/database/schema picker: opens instantly (recents + last known list)
    /// and refreshes the list in the background.
    fn pick_context(&mut self, conn_id: &str, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.open_conn(conn_id).map(|o| o.meta.clone()) else { return };
        let session = meta.read(cx).session.clone();
        let key = format!("{conn_id}:{kind}");
        let current = {
            let c = &meta.read(cx).cache;
            let schema = c.search_path.first().and_then(|p| p.split_once('.')).map(|(_, s)| s.to_string());
            match kind {
                "role" => c.role.clone(),
                "warehouse" => c.warehouse.clone(),
                "database" => c.current_db().map(String::from),
                _ => schema,
            }
        };
        let cached = self.pick_cache.get(&key).cloned().map_or(Load::Loading, Load::Ready);
        let recent = settings::get(cx).recent.get(&key).cloned().unwrap_or_default();
        let (k2, meta2, session2) = (key.clone(), meta.clone(), session.clone());
        let on_pick: crate::picker::OnPick = Rc::new(move |name, window, cx| {
            let (session, meta, key) = (session2.clone(), meta2.clone(), k2.clone());
            settings::update(cx, |s| push_recent(s.recent.entry(key).or_default(), &name));
            let sql = format!("use {kind} {}", quote_always(&name));
            window
                .spawn(cx, async move |cx| {
                    let res = session.execute(sql).await;
                    cx.update(|window, cx| match res {
                        Ok(_) => meta.update(cx, |m, cx| m.reload_roots(cx)),
                        Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                    })
                })
                .detach();
        });
        let picker = cx.new(|cx| Picker::new(cached, recent, current, on_pick, window, cx));
        let input = picker.read(cx).input();
        let p2 = picker.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.title(t!("ws.choose", kind = kind_label(kind)).to_string()).w(px(420.)).child(p2.clone()));
        input.update(cx, |s, cx| s.focus(window, cx));
        // List refreshed in the background; the picker is already open.
        cx.spawn(async move |ws, cx| {
            let listed = session.query_named(list_sql(kind)).await.map(|(n, rows)| match kind {
                "role" => rows.first().and_then(|r| r.first().cloned().flatten()).map(|j| parse_available_roles(&j)).unwrap_or_default(),
                _ => rb_core::snowflake::first_named(&n, &rows, "name"),
            });
            ws.update(cx, |ws, cx| {
                let v = match listed {
                    Ok(names) => {
                        ws.pick_cache.insert(key, names.clone());
                        Load::Ready(names)
                    }
                    Err(e) => Load::Failed(e.to_string()),
                };
                picker.update(cx, |p, cx| p.set_list(v, cx));
            })
            .ok();
        })
        .detach();
    }

    fn on_admin(&self) -> bool {
        matches!(self.tabs.get(self.active_tab), Some(TabView::Admin(_) | TabView::Warehouses(_)))
    }

    /// Objects panel: each context (admin tab or the rest) remembers its own.
    fn objects_visible(&self) -> bool {
        if self.on_admin() { self.objects_on_admin } else { self.objects_open }
    }

    /// Connection the frame shows: the active tab's (where the next ⌘↵ runs), otherwise the current one.
    fn display_conn(&self, cx: &App) -> Option<&Open> {
        self.tabs.get(self.active_tab).and_then(|t| self.open_conn(&t.conn_id(cx))).or_else(|| self.current_open())
    }

    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            return drag_space("drag-empty", cx).size_full().into_any_element();
        }
        let labels: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                let id = t.conn_id(cx);
                let name = self.connections.iter().find(|c| c.id == id).map_or(String::new(), |c| c.name.clone());
                format!("{name} · {}", t.title(cx))
            })
            .collect();
        // The "+" sits at the end of the bar and takes space too.
        let (width, overflow) = tab_width(self.tab_bar_w.get() - 40., self.tabs.len());
        // New active tab (click, ⌘K, just opened): scroll to it once.
        if self.revealed_tab != Some(self.active_tab) {
            self.revealed_tab = Some(self.active_tab);
            self.tab_scroll.scroll_to_item(self.active_tab);
        }
        let menu = overflow.then(|| {
            let (labels, active) = (labels.clone(), self.active_tab);
            let ws = cx.entity().downgrade();
            div().on_mouse_down(MouseButton::Left, no_drag).child(
                Button::new("tabs-more").ghost().small().icon(IconName::ChevronDown).label(labels.len().to_string()).tooltip(t!("ws.all_tabs").to_string()).dropdown_menu(move |menu, _, _| {
                    labels.iter().enumerate().fold(menu.scrollable(true), |m, (i, l)| {
                        let ws = ws.clone();
                        m.item(PopupMenuItem::new(l.clone()).checked(i == active).on_click(move |_, _, cx| {
                            ws.update(cx, |ws, cx| ws.select_tab(i, cx)).ok();
                        }))
                    })
                }),
            )
        });
        let bar = self.tabs.iter().enumerate().fold(
            TabBar::new("tabs")
                .max_width(px(width))
                .track_scroll(&self.tab_scroll)
                .selected_index(self.active_tab)
                .on_click(cx.listener(|ws, ix: &usize, _, cx| ws.select_tab(*ix, cx)))
                // + right after the last tab; the TabBar only draws that space if there is a suffix.
                // The empty space after the tabs also drags the window (the "+" doesn't).
                .last_empty_space(
                    drag_space("drag-tabs", cx).flex_1().child(
                        div()
                            .on_mouse_down(MouseButton::Left, no_drag)
                            .child(Button::new("new-tab").label("+").ghost().tooltip(crate::ui::keys(&t!("ws.new_script"))).on_click(cx.listener(|ws, _, window, cx| ws.new_sql_tab(window, cx)))),
                    ),
                )
                .suffix(div()),
            |bar, (i, t)| {
                let id = t.conn_id(cx);
                let cfg = self.connections.iter().find(|c| c.id == id);
                let label = labels[i].clone();
                // Script: double-click on the name renames the file.
                let label = match t {
                    TabView::Sql(s) => {
                        let s = s.clone();
                        div()
                            .id(("tab-label", i))
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .tooltip({
                                let tip = format!("{label}\n{}", t!("ws.rename_hint"));
                                move |w, cx| Tooltip::new(tip.clone()).build(w, cx)
                            })
                            .on_double_click(cx.listener(move |_, _, window, cx| rename_script(s.clone(), window, cx)))
                            .child(label)
                            .into_any_element()
                    }
                    _ => {
                        let tip = label.clone();
                        div().id(("tab-label", i)).min_w_0().whitespace_nowrap().text_ellipsis().tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx)).child(label).into_any_element()
                    }
                };
                bar.child(Tab::new().when_some(cfg, |tab, c| tab.prefix(div().pl_2().child(logo(c).small()))).child(label).suffix(
                    Button::new(("close", i)).label("×").ghost().tooltip(crate::ui::keys(&t!("ws.close_tab"))).on_click(cx.listener(move |ws, _, window, cx| {
                        // Without this the click bubbles up to the tab, which would do active_tab = i after the removal
                        // (nonexistent index when closing the last tab → panic in render).
                        cx.stop_propagation();
                        ws.close_tab(i, window, cx)
                    })),
                ))
            },
        );
        // Measures the bar's real width; if it changed (window resized), redraw.
        let w = self.tab_bar_w.clone();
        let measure = canvas(
            move |b, window, _| {
                let now = f32::from(b.size.width);
                if (w.get() - now).abs() > 0.5 {
                    w.set(now);
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        h_flex().size_full().child(div().relative().flex_1().min_w_0().h_full().child(measure).child(bar)).children(menu).into_any_element()
    }

    /// Title row: tabs, context bar (opens the menu) and ⌘K.
    fn render_title(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The native bar is transparent and the app owns the dragging (TitleBar::window_options):
        // same scheme as gpui-component's TitleBar.
        h_flex()
            .id("title-row")
            .window_control_area(WindowControlArea::Drag)
            .on_double_click(|_, window, _| window.titlebar_double_click())
            .on_mouse_down_out(cx.listener(|ws, _, _, _| ws.drag_armed = false))
            .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, _| ws.drag_armed = true))
            .on_mouse_up(MouseButton::Left, cx.listener(|ws, _, _, _| ws.drag_armed = false))
            .on_mouse_move(cx.listener(|ws, _, window, _| {
                if ws.drag_armed {
                    ws.drag_armed = false;
                    window.start_window_move();
                }
            }))
            .h(px(44.))
            .flex_shrink_0()
            // The window buttons are on the left; in fullscreen they disappear.
            .map(|d| if window.is_fullscreen() { d.pl_3() } else { d.pl(px(84.)) })
            .pr_3()
            .gap_4()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(div().id("title-tabs").flex_1().min_w_0().overflow_hidden().on_mouse_down(MouseButton::Left, no_drag).child(self.render_tab_bar(cx)))
            .child(h_flex().flex_shrink_0().justify_end().child(div().on_mouse_down(MouseButton::Left, no_drag).child(
                Button::new("goto").outline().small().icon(IconName::Search).label(crate::ui::keys(&format!("{}  ⌘K", t!("frame.go_to")))).on_click(cx.listener(|ws, _, w, cx| ws.quick_open(w, cx))),
            )))
    }

    /// Context bar menu: connections and, on Snowflake, role/warehouse/database/schema.
    fn context_menu(&mut self, pop: Entity<PopoverState>, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let close = move |w: &mut Window, cx: &mut App| pop.update(cx, |p, cx| p.dismiss(w, cx));
        let mut menu = v_flex().w(px(360.)).gap_1().child(section_label(t!("frame.connections").to_string(), &theme));
        for (ix, c) in self.connections.iter().enumerate() {
            let open = self.open_conn(&c.id).is_some();
            let current = self.current.as_deref() == Some(c.id.as_str());
            let (id1, id2, id3, id4) = (c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone());
            let (c1, c2, c3, c4) = (close.clone(), close.clone(), close.clone(), close.clone());
            menu = menu.child(
                h_flex()
                    .gap_1()
                    .child(side_item(("m-conn", ix), Some(logo(c)), c.name.clone(), current, cx).flex_1().on_click(cx.listener(move |ws, _, w, cx| {
                        c1(w, cx);
                        ws.connect(&id1, w, cx)
                    })))
                    .when_some(c.environment, |d, e| d.child(div().text_xs().text_color(theme.muted_foreground).child(e.label())))
                    .when(open, |d| {
                        d.child(Button::new(("m-disc", ix)).label("⏏").ghost().xsmall().tooltip(t!("ws.disconnect").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                            c2(w, cx);
                            ws.request_disconnect(&id2, w, cx)
                        })))
                    })
                    .child(Button::new(("m-edit", ix)).label("✎").ghost().xsmall().tooltip(t!("conn.edit_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                        c3(w, cx);
                        ws.edit_connection(&id3, w, cx)
                    })))
                    .child(Button::new(("m-del", ix)).label("×").ghost().xsmall().tooltip(t!("ws.remove_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                        c4(w, cx);
                        ws.confirm_remove(&id4, w, cx)
                    }))),
            );
        }
        let cn = close.clone();
        menu = menu.child(side_item("m-new", None, t!("frame.new_connection").to_string(), false, cx).on_click(cx.listener(move |_, _, w, cx| {
            cn(w, cx);
            conn_dialog::open(None, w, cx)
        })));
        if let Some(o) = self.display_conn(cx).filter(|o| o.meta.read(cx).cache.dialect == Dialect::Snowflake) {
            let id = o.conn.cfg.id.clone();
            menu = menu.child(section_label("Snowflake".into(), &theme));
            // With no current database, `show schemas` lists those of all and `use schema` would land in the wrong place.
            let has_db = o.meta.read(cx).cache.current_db().is_some();
            for kind in ["role", "warehouse", "database", "schema"].into_iter().filter(|k| *k != "schema" || has_db) {
                let (id, cl) = (id.clone(), close.clone());
                menu = menu.child(side_item(ElementId::Name(format!("m-ctx-{kind}").into()), None, t!("ws.choose", kind = kind_label(kind)).to_string(), false, cx).on_click(cx.listener(
                    move |ws, _, w, cx| {
                        cl(w, cx);
                        ws.pick_context(&id, kind, w, cx)
                    },
                )));
            }
        }
        menu.into_any_element()
    }

    /// Icon rail on the left.
    fn render_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let sf = self.display_conn(cx).filter(|o| o.conn.cfg.snowflake.is_some()).map(|o| o.conn.cfg.id.clone());
        let btn = |id: &'static str, icon: IconName, tip: String, active: bool| Button::new(id).icon(icon).ghost().accessibility_label(tip.clone()).tooltip(tip).selected(active);
        v_flex()
            .w(px(52.))
            .flex_shrink_0()
            .h_full()
            .items_center()
            .gap_1()
            .py_2()
            .border_r_1()
            .border_color(theme.border)
            .child(btn("rail-objects", IconName::Database, t!("frame.objects").to_string(), self.objects_visible()).on_click(cx.listener(|ws, _, _, cx| {
                if ws.on_admin() {
                    ws.objects_on_admin = !ws.objects_on_admin;
                } else {
                    ws.objects_open = !ws.objects_open;
                }
                cx.notify();
            })))
            .when_some(sf.clone(), |d, id| d.child(btn("rail-access", IconName::Users, t!("frame.access").to_string(), false).on_click(cx.listener(move |ws, _, w, cx| ws.open_admin(&id, w, cx)))))
            .when_some(sf, |d, id| d.child(btn("rail-wh", IconName::Warehouse, t!("frame.warehouses").to_string(), false).on_click(cx.listener(move |ws, _, w, cx| ws.open_warehouses(&id, w, cx)))))
            .child(btn("rail-hist", IconName::RotateCcwClock, crate::ui::keys(&t!("frame.history")), false).on_click(cx.listener(|ws, _, w, cx| ws.show_history(w, cx))))
            .child(div().flex_1())
            .child(btn("rail-lang", IconName::Globe, lang::label(cx.global::<Lang>().0), false).on_click(cx.listener(|ws, _, w, cx| ws.pick_language(w, cx))))
            .child(btn("rail-theme", IconName::SunMoon, appearance::label(cx.global::<Appearance>().0, cx), false).on_click(cx.listener(|ws, _, w, cx| ws.pick_theme(w, cx))))
    }

    /// Objects panel: header with the filter (Snowflake) and the current connection's tree.
    fn render_objects(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(o) = self.current_open() else {
            return div().p_3().text_sm().text_color(theme.muted_foreground).child(t!("frame.connect_hint").to_string()).into_any_element();
        };
        let cache = &o.meta.read(cx).cache;
        // Badge only with an active filter; the patterns go in the tooltip.
        let filter = (cache.dialect == Dialect::Snowflake).then(|| {
            let badge = (!cache.db_filter.is_empty()).then(|| format!("{}/{}", cache.visible_databases().count(), cache.databases.len()));
            let tip = if cache.db_filter.is_empty() { t!("filter.title").to_string() } else { cache.db_filter.join(", ") };
            (badge, tip)
        });
        let (filter_id, tree) = (o.conn.cfg.id.clone(), o.tree.clone());
        v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .when_some(filter, |d, (badge, tip)| {
                let active = badge.is_some();
                let funnel = h_flex()
                    .id("db-filter")
                    .gap_1()
                    .px_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.secondary_hover))
                    .text_color(if active { theme.primary } else { theme.muted_foreground })
                    .child(Icon::new(IconName::Funnel).small())
                    .when_some(badge, |d, b| d.child(div().text_xs().child(b)))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |ws, _, w, cx| ws.edit_db_filter(&filter_id, w, cx)));
                d.child(h_flex().justify_between().pt_1().pr_1().text_xs().text_color(theme.muted_foreground).child(t!("tree.databases").to_string().to_uppercase()).child(funnel))
            })
            .child(div().flex_1().min_h_0().child(tree))
            .into_any_element()
    }

    /// No open connection: the connection list fills the main area (there is no sidebar anymore).
    fn render_connections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().w_full().max_w(px(480.)).gap_1().child(section_label(t!("frame.connections").to_string(), &theme));
        for (ix, c) in self.connections.iter().enumerate() {
            let (id1, id2, id3) = (c.id.clone(), c.id.clone(), c.id.clone());
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(side_item(("e-conn", ix), Some(logo(c)), c.name.clone(), false, cx).flex_1().on_click(cx.listener(move |ws, _, w, cx| ws.connect(&id1, w, cx))))
                    .when_some(c.environment, |d, e| d.child(div().text_xs().text_color(theme.muted_foreground).child(e.label())))
                    .child(
                        Button::new(("e-edit", ix)).label("✎").ghost().xsmall().tooltip(t!("conn.edit_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| ws.edit_connection(&id2, w, cx))),
                    )
                    .child(Button::new(("e-del", ix)).label("×").ghost().xsmall().tooltip(t!("ws.remove_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| ws.confirm_remove(&id3, w, cx)))),
            );
        }
        list = list
            .child(side_item("e-new", None, t!("frame.new_connection").to_string(), false, cx).on_click(cx.listener(|_, _, w, cx| conn_dialog::open(None, w, cx))))
            .children(self.connecting_row(cx).map(|r| div().pt_2().text_sm().text_color(theme.muted_foreground).child(r)));
        v_flex().size_full().items_center().justify_center().p_4().child(list).into_any_element()
    }

    /// Context bar (ENV / ROLE / WAREHOUSE / DB.SCHEMA): in the status bar, so it doesn't compete for
    /// space with the tabs on the title row.
    fn context_pill(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let p = palette(theme.mode.is_dark());
        let ctx = self.display_conn(cx).map(|o| {
            let c = &o.meta.read(cx).cache;
            let env = o.conn.cfg.environment;
            let parts = context_parts(c.dialect, &o.conn.cfg.user, c.role.as_deref(), c.warehouse.as_deref(), &c.search_path);
            (o.conn.cfg.id.clone(), o.conn.cfg.name.clone(), c.dialect, parts, env, env_color(env, theme.mode.is_dark()))
        });
        // Environment and connection name inside the button: clicking them opens the menu.
        let badge = ctx.as_ref().map(|(_, name, _, _, env, color)| {
            h_flex()
                .gap_1p5()
                .when_some(*env, |d, e| {
                    d.child(
                        h_flex()
                            .gap_1p5()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color.unwrap_or(theme.muted_foreground))
                            .when_some(*color, |d, c| d.child(div().size(px(7.)).rounded_full().bg(c)))
                            .child(e.label()),
                    )
                })
                .child(div().max_w(px(200.)).overflow_hidden().text_ellipsis().whitespace_nowrap().text_color(theme.foreground).child(name.clone()))
        });
        let ws = cx.entity().downgrade();
        let menu = Popover::new("ctx-menu")
            // In the status bar: the menu opens upward.
            .anchor(Anchor::BottomLeft)
            .trigger(
                Button::new("ctx-pill")
                    .ghost()
                    .xsmall()
                    .tooltip(t!("frame.context_menu").to_string())
                    .child(h_flex().gap_1().when(ctx.is_none(), |d| d.child(t!("frame.connect").to_string())).children(badge)),
            )
            .content(move |_, window, cx| {
                // Built on every open: reflects current connections and context.
                let pop = cx.entity();
                ws.upgrade().map(|ws| ws.update(cx, |ws, cx| ws.context_menu(pop, window, cx))).unwrap_or_else(|| div().into_any_element())
            });
        let Some((conn_id, _, dialect, parts, _, _)) = ctx else {
            return h_flex().child(menu);
        };
        let mut row = h_flex().gap_1().min_w_0().max_w(px(840.)).text_xs().child(div().flex_shrink_0().child(menu));
        // Snowflake: each part opens its own picker (role, warehouse, database). Postgres: text only.
        let kinds: &[&'static str] = if dialect == Dialect::Snowflake { &["role", "warehouse", "database"] } else { &[] };
        for (i, part) in parts.into_iter().enumerate() {
            let kind = kinds.get(i).copied();
            let empty = part == "—";
            // No database chosen: nothing (the menu has "Choose database").
            if empty && kind == Some("database") {
                continue;
            }
            if i > 0 {
                row = row.child(div().text_color(p.border_strong).child("/"));
            }
            let label = part;
            let seg = div()
                .id(ElementId::Name(format!("ctx-seg-{i}").into()))
                .px_1()
                .rounded(theme.radius)
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font_family(theme.mono_font_family.clone())
                .when(empty, |d| d.text_color(theme.muted_foreground))
                .child(label);
            row = row.child(match kind {
                Some(kind) => {
                    let id = conn_id.clone();
                    seg.cursor_pointer()
                        .hover(|d| d.bg(theme.secondary_hover))
                        .tooltip(move |w, cx| Tooltip::new(t!("ws.choose", kind = kind_label(kind)).to_string()).build(w, cx))
                        .on_click(cx.listener(move |ws, _, w, cx| ws.pick_context(&id, kind, w, cx)))
                        .into_any_element()
                }
                None => seg.into_any_element(),
            });
        }
        row
    }

    /// Status bar: "connecting…" and the shortcuts. Execution status lives in the SQL tab's footer.
    fn render_status(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let hint = |s: String| div().font_family(theme.mono_font_family.clone()).child(crate::ui::keys(&s));
        h_flex()
            .h(px(28.))
            .flex_shrink_0()
            .px_3()
            .gap_4()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(self.context_pill(cx))
            .children(self.connecting_row(cx))
            .child(div().flex_1())
            .child(hint(format!("⌘↵ {}", t!("frame.hint_statement"))))
            .child(hint(format!("⌘⇧↵ {}", t!("frame.hint_all"))))
            .child(hint(format!("⌘E {}", t!("frame.hint_explain"))))
            .child(hint("⌘K".into()))
            .child(
                Popover::new("all-keys")
                    .anchor(Anchor::BottomRight)
                    .trigger(Button::new("all-keys-btn").label("+").ghost().xsmall().tooltip(t!("keys.all").to_string()))
                    .content(|_, _, cx| shortcuts_list(cx)),
            )
    }
}

/// Every shortcut, by where it works; the status bar only has room for the main four.
fn shortcuts_list(cx: &App) -> AnyElement {
    let theme = cx.theme();
    let groups: [(&str, &[(&str, &str)]); 4] = [
        (
            "keys.editor",
            &[
                ("⌘↵", "frame.hint_statement"),
                ("⌘⇧↵", "frame.hint_all"),
                ("⌘E", "frame.hint_explain"),
                ("⌘⇧E", "keys.explain_analyze"),
                ("⌘⇧F", "keys.format"),
                ("⌘/", "keys.comment"),
                ("⌘F", "keys.find"),
                ("⌥⌘F", "keys.replace"),
                ("⌃Space", "keys.complete"),
                ("⌘.", "keys.cancel"),
            ],
        ),
        (
            "keys.app",
            &[
                ("⌘K", "keys.goto"),
                ("⌘T", "keys.new_script"),
                ("⌘W", "keys.close_tab"),
                ("⌘Y", "keys.history"),
                ("⌘,", "keys.edit_connection"),
                ("⌘R", "keys.refresh"),
                ("⌘1…⌘9", "keys.switch_tab"),
                ("⌘Q", "keys.quit"),
            ],
        ),
        ("keys.results", &[("⌘C", "keys.copy"), ("⌘-click header", "keys.group_mark"), ("⌘S", "keys.save_edits"), ("F5", "keys.refresh_tree")]),
        ("keys.diagrams", &[("⌘+ ⌘−", "keys.zoom"), ("⌘0", "keys.fit"), ("⌘ scroll", "keys.zoom")]),
    ];
    v_flex()
        .gap_3()
        .p_1()
        .text_xs()
        .children(groups.iter().map(|(title, keys)| {
            v_flex().gap_1().child(div().text_color(theme.muted_foreground).child(t!(*title).to_string().to_uppercase())).children(
                keys.iter()
                    .map(|(k, label)| h_flex().gap_3().child(div().w(px(120.)).flex_shrink_0().font_family(theme.mono_font_family.clone()).child(crate::ui::keys(k))).child(t!(*label).to_string())),
            )
        }))
        .into_any_element()
}

/// A click on a tab/button of the title row must not become a window drag.
/// Dialog with the script's current name; Enter or "Rename" applies.
fn rename_script(tab: Entity<SqlTab>, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| InputState::new(window, cx).default_value(tab.read(cx).title().to_string()));
    let apply = {
        let (tab, input) = (tab.clone(), input.clone());
        Rc::new(move |window: &mut Window, cx: &mut App| {
            let name = input.read(cx).value().to_string();
            match tab.update(cx, |t, cx| t.rename(&name, cx)) {
                Ok(()) => window.close_dialog(cx),
                Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
            }
        })
    };
    let on_enter = apply.clone();
    window
        .subscribe(&input, cx, move |_, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                on_enter(window, cx);
            }
        })
        .detach();
    let field = input.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let ok = apply.clone();
        dialog.title(t!("ws.rename_title").to_string()).w(px(420.)).child(Input::new(&field)).footer(
            h_flex()
                .justify_end()
                .gap_2()
                .child(Button::new("rename-cancel").label(t!("common.cancel").to_string()).on_click(|_, w, cx| w.close_dialog(cx)))
                .child(Button::new("rename-ok").label(t!("ws.rename").to_string()).primary().on_click(move |_, w, cx| ok(w, cx))),
        )
    });
    input.update(cx, |s, cx| {
        s.focus(window, cx);
        s.select_all(window, cx);
    });
}

fn no_drag(_: &MouseDownEvent, _: &mut Window, cx: &mut App) {
    cx.stop_propagation();
}

const TAB_MIN: f32 = 120.;
const TAB_MAX: f32 = 240.;

/// Width of each tab so `n` fit in `avail`, clamped to min/max;
/// true = they don't fit even at the minimum (the bar scrolls and the all-tabs menu shows).
fn tab_width(avail: f32, n: usize) -> (f32, bool) {
    if n == 0 || avail <= 0. {
        return (TAB_MAX, false);
    }
    let w = avail / n as f32;
    (w.clamp(TAB_MIN, TAB_MAX), w < TAB_MIN)
}

/// Empty area inside the tab strip that drags the window like the title bar: arms the
/// drag (`title-row` starts the movement) and double-click maximizes.
fn drag_space(id: &'static str, cx: &mut Context<Workspace>) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h_full()
        .window_control_area(WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.titlebar_double_click())
        .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, _| ws.drag_armed = true))
}

/// `kind` goes raw into the SQL (`show roles`); on screen, translated.
fn kind_label(kind: &str) -> String {
    match kind {
        "role" => t!("kind.role"),
        "warehouse" => t!("kind.warehouse"),
        "database" => t!("kind.database"),
        _ => t!("kind.schema"),
    }
    .to_string()
}

fn section_label(text: String, theme: &Theme) -> Div {
    div().px_2().pt_2().pb_1().text_xs().text_color(theme.muted_foreground).child(text.to_uppercase())
}

/// Clickable sidebar item, left-aligned (Button always centers).
fn side_item(id: impl Into<ElementId>, icon: Option<Icon>, label: impl Into<SharedString>, active: bool, cx: &App) -> Stateful<Div> {
    let t = cx.theme();
    let label = label.into();
    h_flex()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .gap_2()
        .px_2()
        .py_1()
        .min_w_0()
        .rounded(t.radius)
        .text_sm()
        .whitespace_nowrap()
        .overflow_hidden()
        .map(|d| if active { d.bg(t.list_active) } else { d.hover(|d| d.bg(t.secondary_hover)) })
        .when_some(icon, |d, i| d.child(i.small().flex_shrink_0()))
        .child(div().min_w_0().overflow_hidden().text_ellipsis().child(label))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // 2px strip on top: orange when the active tab is production.
        let stripe = self.display_conn(cx).and_then(|o| env_color(o.conn.cfg.environment, theme.mode.is_dark())).unwrap_or(theme.border);
        // Closed panel: no split, so it doesn't rescale the stored sizes (it would reopen at 640px).
        let split = if self.objects_visible() {
            h_resizable("objects-split")
                .child(resizable_panel().size(px(260.)).size_range(px(180.)..px(640.)).child(self.render_objects(cx)))
                .child(resizable_panel().child(self.render_main(cx)))
                .into_any_element()
        } else {
            self.render_main(cx)
        };
        v_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(theme.font_family.clone())
            .border_t_2()
            .border_color(stripe)
            .on_action(cx.listener(|ws, _: &NewSqlTab, window, cx| ws.new_sql_tab(window, cx)))
            .on_action(cx.listener(|ws, _: &QuickOpen, window, cx| ws.quick_open(window, cx)))
            .on_action(cx.listener(|ws, _: &ShowHistory, window, cx| ws.show_history(window, cx)))
            // ⌘, (Ctrl+, elsewhere): edits the active connection; none open = new connection.
            .on_action(cx.listener(|ws, _: &EditConnection, window, cx| match ws.current.clone() {
                Some(id) => ws.edit_connection(&id, window, cx),
                None => conn_dialog::open(None, window, cx),
            }))
            .on_action(cx.listener(|ws, _: &CloseTab, window, cx| ws.close_tab(ws.active_tab, window, cx)))
            .on_action(cx.listener(|ws, a: &SelectTab, _, cx| {
                if !ws.tabs.is_empty() {
                    ws.select_tab(a.0.min(ws.tabs.len() - 1), cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &Refresh, _, cx| match ws.tabs.get(ws.active_tab) {
                Some(TabView::Table(t)) => t.update(cx, |t, cx| t.refresh(cx)),
                Some(TabView::Admin(t)) => t.update(cx, |t, cx| t.reload(cx)),
                Some(TabView::Warehouses(t)) => t.update(cx, |t, cx| t.reload(cx)),
                // A script reruns with ⌘↵, not ⌘R.
                Some(TabView::Sql(_)) | None => {}
            }))
            .child(self.render_title(window, cx))
            .child(h_flex().flex_1().min_h_0().child(self.render_rail(cx)).child(div().flex_1().min_w_0().h_full().child(split)))
            .child(self.render_status(cx))
    }
}

#[cfg(test)]
mod tests {
    // `gpui_kit::*` brings GPUI\'s own `test` attribute; this one wins over the glob.
    use core::prelude::v1::test;

    use super::*;

    #[test]
    fn tabs_shrink_to_fit_then_overflow() {
        assert_eq!(tab_width(1000., 2), (TAB_MAX, false), "few tabs: max width");
        assert_eq!(tab_width(1000., 5), (200., false), "they share the space");
        assert_eq!(tab_width(1000., 9), (TAB_MIN, true), "below the minimum: scroll and show the menu");
        assert_eq!(tab_width(0., 3), (TAB_MAX, false), "before the first measurement");
    }
}
