use std::collections::HashMap;
use std::path::PathBuf;
use rust_i18n::t;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Icon, InteractiveElementExt as _, Selectable as _, Sizable as _, Theme};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex};
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
use crate::admin_tab::Load;
use crate::dbt_store::DbtStore;
use crate::picker::Picker;
use crate::warehouse_tab::WarehouseTab;
use rb_core::picker::{list_sql, parse_available_roles, push_recent};
use crate::indexer::Indexer;
use crate::appearance::{self, Appearance, palette};
use crate::frame::{context_parts, env_color};
use crate::lang::{self, Lang};
use crate::meta::{ConnTarget, MetaStore};
use crate::schema_tree::SchemaTree;
use crate::sql_tab::SqlTab;
use crate::table_tab::TableTab;
use crate::{CloseTab, Db, NewSqlTab, Quit, QuickOpen, ShowHistory, conn_dialog, db_filter, history_popup, quick_open};

/// Conexão aberta: sessão de metadados e árvore próprias; cada aba guarda a sua sessão.
pub struct Open {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
    pub tree: Entity<SchemaTree>,
    /// Índice da busca rápida (só Snowflake).
    pub indexer: Option<Entity<Indexer>>,
    /// Projeto dbt ligado (só Snowflake; sem pasta = "não configurado").
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
    if snowflake {
        Icon::default().path("icons/snowflake-logo.svg").text_color(rgb(0x29b5e8))
    } else {
        Icon::default().path("icons/postgresql.svg").text_color(rgb(0x4169e1))
    }
}

pub struct Workspace {
    focus: FocusHandle,
    pub connections: Vec<ConnectionConfig>,
    pub open: Vec<Open>,
    /// Conexão destacada na barra lateral: recebe as abas novas e mostra a árvore.
    current: Option<String>,
    /// Árvore da conexão atual recolhida (clique de novo na conexão).
    /// Painel de objetos aberto (trilho → Objetos).
    objects_open: bool,
    /// Na aba Usuários & Roles a árvore de databases não ajuda: fechada, a não ser que peçam.
    objects_on_admin: bool,
    /// Botão esquerdo apertado na linha do título: o próximo movimento arrasta a janela.
    drag_armed: bool,
    pub tabs: Vec<TabView>,
    pub active_tab: usize,
    /// Conexão sendo aberta (nome) e a task: soltar a task cancela a tentativa.
    connecting: Option<String>,
    connect_task: Option<Task<()>>,
    /// Conexões a reabrir na partida, uma depois da outra (o SSO abre uma janela por vez).
    reconnect_queue: Vec<String>,
    /// Escolhas recentes do seletor ("conexão:tipo"), espelho do settings.toml.
    recent: std::collections::BTreeMap<String, Vec<String>>,
    /// Última lista de roles/warehouses/… por "conexão:tipo": o seletor abre com ela na hora.
    pick_cache: HashMap<String, Vec<String>>,
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
        // Reabre as conexões que estavam abertas quando o app fechou (com o ID token, sem navegador).
        let ids: Vec<String> = connections.iter().map(|c| c.id.clone()).collect();
        let reconnect_queue = config::reconnect_order(&config::load_settings(&config::settings_path()).last_open, &ids);
        cx.defer_in(window, |ws, window, cx| ws.reconnect_next(window, cx));
        Workspace {
            reconnect_queue, focus: cx.focus_handle(), connections, open: Vec::new(), current: None, objects_open: true, objects_on_admin: false, drag_armed: false, tabs: Vec::new(), active_tab: 0, connecting: None, connect_task: None, recent: config::load_settings(&config::settings_path()).recent, pick_cache: HashMap::new() }
    }

    fn open_conn(&self, id: &str) -> Option<&Open> {
        self.open.iter().find(|o| o.conn.cfg.id == id)
    }

    fn current_open(&self) -> Option<&Open> {
        self.current.as_deref().and_then(|id| self.open_conn(id))
    }

    /// Cria ou atualiza (mesmo id). Senha/passphrase vazias mantêm o Keychain; sem túnel, a
    /// passphrase guardada é apagada. Editar uma conexão aberta reconecta. Devolve `false` se recusou.
    pub fn save_connection(&mut self, mut cfg: ConnectionConfig, password: String, passphrase: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let id = cfg.id.clone();
        let reconnect = self.open_conn(&id).is_some();
        if reconnect && self.has_pending_edits(&id, cx) {
            window.push_notification(Notification::error(t!("ws.pending_before_edit").to_string()), cx);
            return false;
        }
        // O diálogo não edita o filtro de databases: mantém o que já estava salvo.
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
                        // A passphrase do diálogo é da chave SSH (Postgres) ou da chave Snowflake.
                        let (keep, other) = if is_sf {
                            (secrets::key_account(&id), secrets::ssh_account(&id))
                        } else {
                            (secrets::ssh_account(&id), secrets::key_account(&id))
                        };
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
                    if let Some(ix) = ws.connections.iter().position(|c| c.id == id) {
                        ws.connect(ix, window, cx);
                    }
                }
                Ok(()) => {}
            })
        })
        .detach();
        cx.notify();
        true
    }

    /// Fechada: conecta. Aberta: vira a atual; se já era, recolhe/expande a árvore.
    fn connect(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.connections[ix].id.clone();
        if self.open_conn(&id).is_some() {
            // A moldura segue a aba ativa: trocar de conexão leva a uma aba dela (ou abre um script),
            // senão a barra de contexto mostraria uma conexão e o ⌘↵ rodaria em outra.
            self.current = Some(id.clone());
            self.objects_open = true;
            match self.tabs.iter().rposition(|t| t.conn_id(cx) == id) {
                Some(ix) => self.select_tab(ix, cx),
                None => self.new_sql_tab(window, cx),
            }
            cx.notify();
            return;
        }
        // Outra tentativa em curso (SSO esperando, rede travada): esta substitui aquela.
        self.cancel_connect(cx);
        self.connecting = Some(self.connections[ix].name.clone());
        cx.notify();
        let cfg = self.connections[ix].clone();
        let engine = cx.global::<Db>().0.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let id = cfg.id.clone();
            let result = async {
                let has_ssh = cfg.ssh.is_some();
                let is_sf = cfg.snowflake.is_some();
                let creds = cx
                    .background_spawn(async move {
                        Ok::<_, rb_core::DbError>(Credentials {
                            password: secrets::get_password(&id)?,
                            ssh_passphrase: if has_ssh { secrets::get_password(&secrets::ssh_account(&id))? } else { None },
                            key_passphrase: if is_sf { secrets::get_password(&secrets::key_account(&id))? } else { None },
                        })
                    })
                    .await?;
                let session = engine.connect(&cfg, creds.clone()).await?;
                // OrgConfig que sumiu ou quebrou não impede a conexão: segue sem convenções e avisa.
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
                        let open: Rc<dyn Fn(String, String, &mut Window, &mut App)> = Rc::new(move |schema, table, window, cx| {
                            ws_weak.update(cx, |ws, cx| ws.open_table(&tree_id, schema, table, window, cx)).ok();
                        });
                        let tree = cx.new(|cx| SchemaTree::new(meta.clone(), open, cx));
                        let indexer = conn.cfg.snowflake.is_some().then(|| cx.new(|cx| Indexer::new(conn.clone(), meta.clone(), cx)));
                        let dbt = conn.cfg.snowflake.as_ref().map(|sf| {
                            let cfg = sf.dbt.clone();
                            cx.new(|cx| DbtStore::new(cfg, cx))
                        });
                        ws.open.push(Open { conn, meta, tree, indexer, dbt });
                        ws.save_open();
                        ws.current = Some(id.clone());
                        ws.objects_open = true;
                        ws.on_connected(&id, window, cx);
                    }
                    Err(rb_core::DbError::HostKeyUnknown { host, port, fingerprint, key }) => {
                        ws.confirm_trust(ix, host, port, fingerprint, key, window, cx)
                    }
                    Err(e) => window.push_notification(Notification::error(t!("ws.connect_failed", error = e).to_string()), cx),
                }
                ws.reconnect_next(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.connect_task = Some(task);
    }

    /// Próxima da fila de reconexão (cada uma espera a anterior terminar).
    fn reconnect_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while !self.reconnect_queue.is_empty() {
            let id = self.reconnect_queue.remove(0);
            if let Some(ix) = self.connections.iter().position(|c| c.id == id)
                && self.open_conn(&id).is_none()
            {
                self.connect(ix, window, cx);
                return;
            }
        }
    }

    /// Lembra quais conexões estão abertas, para a próxima partida.
    fn save_open(&self) {
        let ids: Vec<String> = self.open.iter().map(|o| o.conn.cfg.id.clone()).collect();
        let path = config::settings_path();
        std::thread::spawn(move || {
            if let Err(e) = config::update_settings(&path, |s| s.last_open = ids) {
                tracing::warn!("settings.toml: {e}");
            }
        });
    }

    /// Abandona a tentativa de conexão: a task é solta e o resultado, se chegar, é descartado.
    /// ponytail: um login SSO já aberto no navegador continua lá; só o app para de esperar.
    fn cancel_connect(&mut self, cx: &mut Context<Self>) {
        if self.connect_task.take().is_some() {
            // Cancelar também desiste das reconexões automáticas que faltavam.
            self.reconnect_queue.clear();
            self.connecting = None;
            cx.notify();
        }
    }

    /// "Conectando a X… · Cancelar".
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
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Table(_)) && t.conn_id(cx) == conn_id && t.title(cx) == title) {
            self.select_tab(ix, cx);
            return;
        }
        let (conn, meta, dbt) = (o.conn.clone(), o.meta.clone(), o.dbt.clone());
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        let open_query: crate::table_tab::OpenQuery = Rc::new(move |sql, window, cx| {
            ws.update(cx, |ws, cx| ws.open_query(&id, &sql, window, cx)).ok();
        });
        let tab = cx.new(|cx| TableTab::new(schema, table, conn, meta, dbt, open_query, window, cx));
        self.tabs.push(TabView::Table(tab));
        self.select_tab(self.tabs.len() - 1, cx);
    }

    /// Uma aba de usuários & roles por conexão: se já existe, só foca.
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

    /// Uma aba de warehouses por conexão; role/usuário clicado lá abre Usuários & Roles.
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

    /// Abre (ou foca) Usuários & Roles já na role/usuário pedido.
    fn admin_goto(&mut self, conn_id: &str, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.open_admin(conn_id, window, cx);
        if let Some(TabView::Admin(t)) = self.tabs.get(self.active_tab) {
            t.update(cx, |t, cx| t.goto(target, cx));
        }
    }

    /// Busca rápida (⌘K) na conexão atual.
    fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.current_open() else {
            window.push_notification(Notification::error(t!("ws.open_connection_to_search").to_string()), cx);
            return;
        };
        let (meta, indexer, id) = (o.meta.clone(), o.indexer.clone(), o.conn.cfg.id.clone());
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

    /// ⌘Y: histórico da conexão atual.
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

    /// Insere a query na aba SQL ativa (se for desta conexão) ou num script novo.
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

    /// Mostra o objeto na árvore; tabela/view também abre a aba.
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

    /// Script novo já com o SQL (botão Query da aba de tabela).
    fn open_query(&mut self, conn_id: &str, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let created = config::new_script(&config::scripts_dir().join(conn_id)).and_then(|p| std::fs::write(&p, sql).map(|_| p).map_err(|e| rb_core::DbError::Io(e.to_string())));
        match created {
            Ok(path) => self.open_sql_file(path, conn_id, window, cx),
            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
        }
    }

    /// Script novo na conexão atual.
    pub fn new_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current_open().map(|o| o.conn.cfg.id.clone()) else { return };
        match config::new_script(&config::scripts_dir().join(&id)) {
            Ok(path) => self.open_sql_file(path, &id, window, cx),
            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
        }
    }

    /// Escolher uma aba torna a conexão dela a atual.
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

    /// Grava já o texto de todos os scripts abertos (o autosave espera 1 s).
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
        let tab = cx.new(|cx| SqlTab::new(path, conn, meta, open_table, window, cx));
        tab.read(cx).editor.clone().update(cx, |e, cx| e.focus(window, cx));
        self.tabs.push(TabView::Sql(tab));
        self.select_tab(self.tabs.len() - 1, cx);
    }

    /// Fechar não apaga: o arquivo vai para scripts/closed/.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else { return };
        if let TabView::Table(t) = tab
            && t.read(cx).has_pending(cx)
        {
            window.push_notification(Notification::error(t!("ws.pending_before_close").to_string()), cx);
            return;
        }
        if let TabView::Sql(t) = self.tabs.remove(ix) {
            let t = t.read(cx);
            t.save_now(cx);
            if let Err(e) = config::archive_script(&t.path) {
                tracing::warn!("failed to archive script: {e}");
            }
        }
        let ix = self.active_tab.min(self.tabs.len().saturating_sub(1));
        self.select_tab(ix, cx);
    }

    /// Reabre os scripts da conexão (ordenados) ou cria um novo.
    fn on_connected(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let files = config::conn_scripts(&config::scripts_dir(), id).unwrap_or_else(|e| {
            window.push_notification(Notification::error(e.to_string()), cx);
            Vec::new()
        });
        if files.is_empty() {
            self.new_sql_tab(window, cx);
        }
        for f in files {
            self.open_sql_file(f, id, window, cx);
        }
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            let Some(o) = self.current_open() else { return self.render_connections(cx) };
            let msg = format!("{} — {}", o.conn.cfg.name, t!("ws.new_script_hint"));
            return v_flex().flex_1().h_full().items_center().justify_center().child(msg).into_any_element();
        }
        let content = match &self.tabs[self.active_tab] {
            TabView::Sql(t) => t.clone().into_any_element(),
            TabView::Table(t) => t.clone().into_any_element(),
            TabView::Admin(t) => t.clone().into_any_element(),
            TabView::Warehouses(t) => t.clone().into_any_element(),
        };
        // min_w_0: sem ele o item flex mede no mínimo a largura do conteúdo (todas as colunas da
        // grade) e vaza da janela em vez de deixar a grade rolar na horizontal.
        div().flex_1().min_w_0().min_h_0().size_full().child(content).into_any_element()
    }

    fn confirm_remove(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.connections.get(ix).cloned() else { return };
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ws, id) = (ws.clone(), c.id.clone());
            dialog
                .title(t!("ws.remove_title").to_string())
                .w(px(420.))
                .child(t!("ws.remove_confirm", name = c.name).to_string())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("rm-cancel").label(t!("common.cancel").to_string()).on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("rm-ok").label(t!("ws.remove").to_string()).danger().on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            ws.update(cx, |ws, cx| ws.remove_connection(&id, window, cx)).ok();
                        })),
                )
        });
    }

    /// Tira a conexão do TOML e a senha do Keychain. Se estiver aberta, desconecta antes.
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

    /// Salva os scripts, fecha as abas da conexão e solta a sessão (e o túnel, se for o último uso).
    /// Os scripts ficam na pasta da conexão e voltam ao reconectar.
    fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.save_all(cx);
        let keep: Vec<bool> = self.tabs.iter().map(|t| t.conn_id(cx) != id).collect();
        let before = keep.iter().take(self.active_tab).filter(|k| **k).count();
        let mut k = keep.into_iter();
        self.tabs.retain(|_| k.next().unwrap_or(true));
        self.open.retain(|o| o.conn.cfg.id != id);
        self.save_open();
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

    /// Host SSH fora do known_hosts: mostra o fingerprint e, se o usuário confiar, grava e reconecta.
    #[allow(clippy::too_many_arguments)]
    fn confirm_trust(&mut self, ix: usize, host: String, port: u16, fingerprint: String, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ws, host, key) = (ws.clone(), host.clone(), key.clone());
            dialog
                .title(t!("ws.unknown_host_title").to_string())
                .w(px(480.))
                .child(t!("ws.unknown_host_body", host = host, port = port, fingerprint = fingerprint).to_string())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("trust-cancel").label(t!("common.cancel").to_string()).on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("trust-ok").label(t!("ws.trust").to_string()).primary().on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            // ponytail: append de uma linha no known_hosts na thread de UI; arquivo pequeno.
                            match ssh::trust_host(&SshEnv::system().known_hosts, &host, port, &key) {
                                Ok(()) => {
                                    ws.update(cx, |ws, cx| ws.connect(ix, window, cx)).ok();
                                }
                                Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                            }
                        })),
                )
        });
    }

    /// Lista warehouses ou roles (`SHOW …`) e roda `USE …` na escolhida; a sessão é compartilhada.
    /// Idioma da interface: Auto (segue o macOS) ou um dos três; troca na hora.
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

    /// Diálogo do filtro de databases (Snowflake): chips, `*` coringa, `!` exclui.
    fn edit_db_filter(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.open_conn(conn_id).map(|o| o.meta.read(cx).cache.db_filter.clone()).unwrap_or_default();
        let (ws, id) = (cx.entity().downgrade(), conn_id.to_string());
        db_filter::open(current, move |patterns, window, cx| {
            ws.update(cx, |ws, cx| ws.set_db_filter(&id, patterns, window, cx)).ok();
        }, window, cx);
    }

    /// Grava o filtro no TOML e aplica na árvore e no autocomplete, sem reconectar.
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

    /// Seletor de role/warehouse/database/schema: abre na hora (recentes + última lista conhecida)
    /// e atualiza a lista em segundo plano.
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
        let recent = self.recent.get(&key).cloned().unwrap_or_default();
        let ws = cx.entity().downgrade();
        let (k2, meta2, session2) = (key.clone(), meta.clone(), session.clone());
        let on_pick: crate::picker::OnPick = Rc::new(move |name, window, cx| {
            let (session, meta, key) = (session2.clone(), meta2.clone(), k2.clone());
            ws.update(cx, |ws, _| {
                push_recent(ws.recent.entry(key.clone()).or_default(), &name);
                let (path, recent) = (config::settings_path(), ws.recent.clone());
                std::thread::spawn(move || {
                    if let Err(e) = config::update_settings(&path, |s| s.recent = recent) {
                        tracing::warn!("settings.toml: {e}");
                    }
                });
            })
            .ok();
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
        // Lista atualizada em segundo plano; o seletor já está aberto.
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

    /// Painel de objetos: cada contexto (aba de admin ou o resto) lembra o seu.
    fn objects_visible(&self) -> bool {
        if self.on_admin() { self.objects_on_admin } else { self.objects_open }
    }

    /// Conexão que a moldura mostra: a da aba ativa (é onde o próximo ⌘↵ roda), senão a atual.
    fn display_conn(&self, cx: &App) -> Option<&Open> {
        self.tabs.get(self.active_tab).and_then(|t| self.open_conn(&t.conn_id(cx))).or_else(|| self.current_open())
    }

    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            return drag_space("drag-empty", cx).size_full().into_any_element();
        }
        self.tabs
            .iter()
            .enumerate()
            .fold(
                TabBar::new("tabs")
                    .selected_index(self.active_tab)
                    .on_click(cx.listener(|ws, ix: &usize, _, cx| ws.select_tab(*ix, cx)))
                    // + logo depois da última aba; o TabBar só desenha esse espaço se houver sufixo.
                    // O vazio depois das abas também arrasta a janela (o "+" não).
                    .last_empty_space(
                        drag_space("drag-tabs", cx).flex_1().child(
                            div().on_mouse_down(MouseButton::Left, no_drag).child(Button::new("new-tab").label("+").ghost().tooltip(t!("ws.new_script").to_string()).on_click(cx.listener(|ws, _, window, cx| ws.new_sql_tab(window, cx)))),
                        ),
                    )
                    .suffix(div()),
                |bar, (i, t)| {
                    let id = t.conn_id(cx);
                    let cfg = self.connections.iter().find(|c| c.id == id);
                    let name = cfg.map_or(String::new(), |c| c.name.clone());
                    let label = format!("{name} · {}", t.title(cx));
                    // Script: duplo clique no nome renomeia o arquivo.
                    let label = match t {
                        TabView::Sql(s) => {
                            let s = s.clone();
                            div()
                                .id(("tab-label", i))
                                .whitespace_nowrap()
                                .tooltip(|w, cx| Tooltip::new(t!("ws.rename_hint").to_string()).build(w, cx))
                                .on_double_click(cx.listener(move |_, _, window, cx| rename_script(s.clone(), window, cx)))
                                .child(label)
                                .into_any_element()
                        }
                        _ => div().whitespace_nowrap().child(label).into_any_element(),
                    };
                    bar.child(
                        Tab::new()
                            .when_some(cfg, |tab, c| tab.prefix(div().pl_2().child(logo(c).small())))
                            .child(label)
                            .suffix(Button::new(("close", i)).label("×").ghost().tooltip(t!("ws.close_tab").to_string()).on_click(cx.listener(move |ws, _, window, cx| {
                                // Sem isto o clique sobe até a aba, que faria active_tab = i depois da remoção
                                // (índice inexistente ao fechar a última aba → pânico no render).
                                cx.stop_propagation();
                                ws.close_tab(i, window, cx)
                            }))),
                    )
                },
            )
            .into_any_element()
    }

    /// Linha do título: abas, barra de contexto (abre o menu) e ⌘K.
    fn render_title(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // A barra nativa é transparente e o app é dono do arrasto (TitleBar::window_options):
        // mesmo esquema do TitleBar do gpui-component.
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
            // Os botões da janela ficam à esquerda; em tela cheia eles somem.
            .map(|d| if window.is_fullscreen() { d.pl_3() } else { d.pl(px(84.)) })
            .pr_3()
            .gap_4()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(div().id("title-tabs").flex_1().min_w_0().overflow_hidden().on_mouse_down(MouseButton::Left, no_drag).child(self.render_tab_bar(cx)))
            .child(
                h_flex().flex_shrink_0().justify_end().child(
                    div().on_mouse_down(MouseButton::Left, no_drag).child(Button::new("goto").outline().small().icon(IconName::Search).label(format!("{}  ⌘K", t!("frame.go_to"))).on_click(cx.listener(|ws, _, w, cx| ws.quick_open(w, cx)))),
                ),
            )
    }

    /// Menu da barra de contexto: conexões e, no Snowflake, role/warehouse/database/schema.
    fn context_menu(&mut self, pop: Entity<PopoverState>, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let close = move |w: &mut Window, cx: &mut App| pop.update(cx, |p, cx| p.dismiss(w, cx));
        let mut menu = v_flex().w(px(360.)).gap_1().child(section_label(t!("frame.connections").to_string(), &theme));
        for (ix, c) in self.connections.iter().enumerate() {
            let open = self.open_conn(&c.id).is_some();
            let current = self.current.as_deref() == Some(c.id.as_str());
            let id = c.id.clone();
            let (c1, c2, c3, c4) = (close.clone(), close.clone(), close.clone(), close.clone());
            menu = menu.child(
                h_flex()
                    .gap_1()
                    .child(side_item(("m-conn", ix), Some(logo(c)), c.name.clone(), current, cx).flex_1().on_click(cx.listener(move |ws, _, w, cx| {
                        c1(w, cx);
                        ws.connect(ix, w, cx)
                    })))
                    .when_some(c.environment, |d, e| d.child(div().text_xs().text_color(theme.muted_foreground).child(e.label())))
                    .when(open, |d| {
                        d.child(Button::new(("m-disc", ix)).label("⏏").ghost().xsmall().tooltip(t!("ws.disconnect").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                            c2(w, cx);
                            ws.request_disconnect(&id, w, cx)
                        })))
                    })
                    .child(Button::new(("m-edit", ix)).label("✎").ghost().xsmall().tooltip(t!("conn.edit_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                        c3(w, cx);
                        let c = ws.connections[ix].clone();
                        conn_dialog::open(Some(c), w, cx)
                    })))
                    .child(Button::new(("m-del", ix)).label("×").ghost().xsmall().tooltip(t!("ws.remove_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                        c4(w, cx);
                        ws.confirm_remove(ix, w, cx)
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
            // Sem database atual, `show schemas` lista os de todas e o `use schema` cairia no lugar errado.
            let has_db = o.meta.read(cx).cache.current_db().is_some();
            for kind in ["role", "warehouse", "database", "schema"].into_iter().filter(|k| *k != "schema" || has_db) {
                let (id, cl) = (id.clone(), close.clone());
                menu = menu.child(side_item(ElementId::Name(format!("m-ctx-{kind}").into()), None, t!("ws.choose", kind = kind_label(kind)).to_string(), false, cx).on_click(cx.listener(move |ws, _, w, cx| {
                    cl(w, cx);
                    ws.pick_context(&id, kind, w, cx)
                })));
            }
        }
        menu.into_any_element()
    }

    /// Trilho de ícones à esquerda.
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
            .child(btn("rail-hist", IconName::RotateCcwClock, t!("frame.history").to_string(), false).on_click(cx.listener(|ws, _, w, cx| ws.show_history(w, cx))))
            .child(div().flex_1())
            .child(btn("rail-lang", IconName::Globe, lang::label(cx.global::<Lang>().0), false).on_click(cx.listener(|ws, _, w, cx| ws.pick_language(w, cx))))
            .child(btn("rail-theme", IconName::SunMoon, appearance::label(cx.global::<Appearance>().0, cx), false).on_click(cx.listener(|ws, _, w, cx| ws.pick_theme(w, cx))))
    }

    /// Painel de objetos: cabeçalho com o filtro (Snowflake) e a árvore da conexão atual.
    fn render_objects(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(o) = self.current_open() else {
            return div().p_3().text_sm().text_color(theme.muted_foreground).child(t!("frame.connect_hint").to_string()).into_any_element();
        };
        let cache = &o.meta.read(cx).cache;
        // Badge só com filtro ativo; os padrões ficam no tooltip.
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

    /// Sem conexão aberta: a lista de conexões ocupa a área principal (não há mais barra lateral).
    fn render_connections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().w_full().max_w(px(480.)).gap_1().child(section_label(t!("frame.connections").to_string(), &theme));
        for (ix, c) in self.connections.iter().enumerate() {
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(side_item(("e-conn", ix), Some(logo(c)), c.name.clone(), false, cx).flex_1().on_click(cx.listener(move |ws, _, w, cx| ws.connect(ix, w, cx))))
                    .when_some(c.environment, |d, e| d.child(div().text_xs().text_color(theme.muted_foreground).child(e.label())))
                    .child(Button::new(("e-edit", ix)).label("✎").ghost().xsmall().tooltip(t!("conn.edit_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| {
                        let c = ws.connections[ix].clone();
                        conn_dialog::open(Some(c), w, cx)
                    })))
                    .child(Button::new(("e-del", ix)).label("×").ghost().xsmall().tooltip(t!("ws.remove_title").to_string()).on_click(cx.listener(move |ws, _, w, cx| ws.confirm_remove(ix, w, cx)))),
            );
        }
        list = list
            .child(side_item("e-new", None, t!("frame.new_connection").to_string(), false, cx).on_click(cx.listener(|_, _, w, cx| conn_dialog::open(None, w, cx))))
            .children(self.connecting_row(cx).map(|r| div().pt_2().text_sm().text_color(theme.muted_foreground).child(r)));
        v_flex().size_full().items_center().justify_center().p_4().child(list).into_any_element()
    }

    /// Barra de contexto (ENV / ROLE / WAREHOUSE / DB.SCHEMA): na barra de status, para não disputar
    /// espaço com as abas na linha do título.
    fn context_pill(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let p = palette(theme.mode.is_dark());
        let ctx = self.display_conn(cx).map(|o| {
            let c = &o.meta.read(cx).cache;
            let env = o.conn.cfg.environment;
            let parts = context_parts(None, c.dialect, &o.conn.cfg.user, c.role.as_deref(), c.warehouse.as_deref(), &c.search_path);
            (o.conn.cfg.id.clone(), c.dialect, parts, env, env_color(env, theme.mode.is_dark()))
        });
        let ws = cx.entity().downgrade();
        let menu = Popover::new("ctx-menu")
            // Na barra de status: o menu abre para cima.
            .anchor(Anchor::BottomLeft)
            .trigger(
                Button::new("ctx-pill")
                    .ghost()
                    .xsmall()
                    .tooltip(t!("frame.context_menu").to_string())
                    .child(h_flex().gap_1().when(ctx.is_none(), |d| d.child(t!("frame.connect").to_string())).child(Icon::new(IconName::ChevronUp).xsmall())),
            )
            .content(move |_, window, cx| {
                // Montado a cada abertura: reflete conexões e contexto atuais.
                let pop = cx.entity();
                ws.upgrade().map(|ws| ws.update(cx, |ws, cx| ws.context_menu(pop, window, cx))).unwrap_or_else(|| div().into_any_element())
            });
        let Some((conn_id, dialect, parts, env, color)) = ctx else {
            return h_flex().child(menu);
        };
        let mut row = h_flex().gap_1().min_w_0().max_w(px(640.)).text_xs();
        if let Some(e) = env {
            row = row.child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1p5()
                    .pr_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color.unwrap_or(theme.muted_foreground))
                    .when_some(color, |d, c| d.child(div().size(px(7.)).rounded_full().bg(c)))
                    .child(e.label()),
            );
        }
        // Snowflake: cada parte abre o seletor dela (role, warehouse, database). Postgres: só texto.
        let kinds: &[&'static str] = if dialect == Dialect::Snowflake { &["role", "warehouse", "database"] } else { &[] };
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                row = row.child(div().text_color(p.border_strong).child("/"));
            }
            let kind = kinds.get(i).copied();
            let empty = part == "—";
            let label = if empty && kind == Some("database") { t!("frame.no_database").to_string() } else { part };
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
        row.child(menu)
    }

    /// Barra de status: "conectando…" e os atalhos. O status da execução fica no rodapé da aba SQL.
    fn render_status(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let hint = |s: String| div().font_family(theme.mono_font_family.clone()).child(s);
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
    }
}

/// Clique em aba/botão da linha do título não pode virar arrasto da janela.
/// Diálogo com o nome atual do script; Enter ou "Rename" aplica.
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

/// Área vazia dentro da faixa das abas que arrasta a janela como a barra de título: arma o
/// arrasto (o `title-row` começa o movimento) e o duplo clique maximiza.
fn drag_space(id: &'static str, cx: &mut Context<Workspace>) -> Stateful<Div> {
    h_flex()
        .id(id)
        .h_full()
        .window_control_area(WindowControlArea::Drag)
        .on_double_click(|_, window, _| window.titlebar_double_click())
        .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, _, _| ws.drag_armed = true))
}

/// `kind` vai cru para o SQL (`show roles`); na tela, traduzido.
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

/// Item clicável da barra lateral, alinhado à esquerda (o Button sempre centraliza).
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
        // Faixa de 2px no topo: laranja quando a aba ativa é de produção.
        let stripe = self.display_conn(cx).and_then(|o| env_color(o.conn.cfg.environment, theme.mode.is_dark())).unwrap_or(theme.border);
        // Painel fechado: sem o split, para ele não reescalar os tamanhos guardados (reabriria em 640px).
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
            .on_action(cx.listener(|ws, _: &CloseTab, window, cx| ws.close_tab(ws.active_tab, window, cx)))
            .on_action(cx.listener(|ws, _: &Quit, _, cx| {
                ws.save_all(cx);
                cx.quit();
            }))
            .child(self.render_title(window, cx))
            .child(h_flex().flex_1().min_h_0().child(self.render_rail(cx)).child(div().flex_1().min_w_0().h_full().child(split)))
            .child(self.render_status(cx))
    }
}
