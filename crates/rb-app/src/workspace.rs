use std::path::PathBuf;
use rust_i18n::t;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Icon, Sizable as _};
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

use crate::admin_tab::AdminTab;
use crate::indexer::Indexer;
use crate::appearance::{self, Appearance};
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
}

#[derive(Clone)]
pub enum TabView {
    Sql(Entity<SqlTab>),
    Table(Entity<TableTab>),
    Admin(Entity<AdminTab>),
}

impl TabView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            TabView::Sql(t) => t.read(cx).title(),
            TabView::Table(t) => t.read(cx).title(),
            TabView::Admin(t) => t.read(cx).title(),
        }
    }

    fn conn_id(&self, cx: &App) -> String {
        match self {
            TabView::Sql(t) => t.read(cx).conn_id().to_string(),
            TabView::Table(t) => t.read(cx).conn_id().to_string(),
            TabView::Admin(t) => t.read(cx).conn_id().to_string(),
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
    collapsed: bool,
    pub tabs: Vec<TabView>,
    pub active_tab: usize,
    connecting: bool,
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
        Workspace { focus: cx.focus_handle(), connections, open: Vec::new(), current: None, collapsed: false, tabs: Vec::new(), active_tab: 0, connecting: false }
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
            if self.current.as_deref() == Some(id.as_str()) {
                self.collapsed = !self.collapsed;
            } else {
                self.current = Some(id);
                self.collapsed = false;
            }
            cx.notify();
            return;
        }
        if self.connecting {
            return;
        }
        self.connecting = true;
        cx.notify();
        let cfg = self.connections[ix].clone();
        let engine = cx.global::<Db>().0.clone();
        cx.spawn_in(window, async move |this, cx| {
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
                Ok::<_, rb_core::DbError>((ConnTarget { cfg, creds }, session))
            }
            .await;
            this.update_in(cx, |ws, window, cx| {
                ws.connecting = false;
                match result {
                    Ok((conn, session)) => {
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
                        ws.open.push(Open { conn, meta, tree, indexer });
                        ws.current = Some(id.clone());
                        ws.collapsed = false;
                        ws.on_connected(&id, window, cx);
                    }
                    Err(rb_core::DbError::HostKeyUnknown { host, port, fingerprint, key }) => {
                        ws.confirm_trust(ix, host, port, fingerprint, key, window, cx)
                    }
                    Err(e) => window.push_notification(Notification::error(t!("ws.connect_failed", error = e).to_string()), cx),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn open_table(&mut self, conn_id: &str, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.open_conn(conn_id) else { return };
        let title: SharedString = format!("{schema}.{table}").into();
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Table(_)) && t.conn_id(cx) == conn_id && t.title(cx) == title) {
            self.select_tab(ix, cx);
            return;
        }
        let (conn, meta) = (o.conn.clone(), o.meta.clone());
        let tab = cx.new(|cx| TableTab::new(schema, table, conn, meta, window, cx));
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

    /// Busca rápida (⌘K) na conexão atual.
    fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(o) = self.current_open() else {
            window.push_notification(Notification::error(t!("ws.open_connection_to_search").to_string()), cx);
            return;
        };
        let (meta, indexer, id) = (o.meta.clone(), o.indexer.clone(), o.conn.cfg.id.clone());
        let ws = cx.entity().downgrade();
        quick_open::open(
            meta,
            indexer,
            move |hit, window, cx| {
                ws.update(cx, |ws, cx| ws.reveal_hit(&id, hit, window, cx)).ok();
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
        self.collapsed = false;
        tree.update(cx, |t, cx| t.reveal(hit.tree_path(), cx));
        if matches!(hit.kind, HitKind::Table | HitKind::View)
            && let Some(schema) = hit.schema_key()
        {
            self.open_table(conn_id, schema, hit.name, window, cx);
        }
        cx.notify();
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
        let tab = cx.new(|cx| SqlTab::new(path, conn, meta, window, cx));
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
                tracing::warn!("falha ao arquivar script: {e}");
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
            let msg = match self.current_open() {
                Some(o) => format!("{} — {}", o.conn.cfg.name, t!("ws.new_script_hint")),
                None => t!("ws.no_connection").to_string(),
            };
            return v_flex().flex_1().h_full().items_center().justify_center().child(msg).into_any_element();
        }
        let bar = self.tabs.iter().enumerate().fold(
            TabBar::new("tabs")
                .selected_index(self.active_tab)
                .on_click(cx.listener(|ws, ix: &usize, _, cx| ws.select_tab(*ix, cx)))
                // + logo depois da última aba; o TabBar só desenha esse espaço se houver sufixo.
                .last_empty_space(h_flex().flex_1().child(Button::new("new-tab").label("+").ghost().tooltip(t!("ws.new_script").to_string()).on_click(cx.listener(|ws, _, window, cx| ws.new_sql_tab(window, cx)))))
                .suffix(div()),
            |bar, (i, t)| {
                let id = t.conn_id(cx);
                let cfg = self.connections.iter().find(|c| c.id == id);
                let name = cfg.map_or(String::new(), |c| c.name.clone());
                bar.child(
                    Tab::new()
                        .when_some(cfg, |tab, c| tab.prefix(div().pl_2().child(logo(c).small())))
                        .label(format!("{name} · {}", t.title(cx)))
                        .suffix(Button::new(("close", i)).label("×").ghost().tooltip(t!("ws.close_tab").to_string()).on_click(cx.listener(move |ws, _, window, cx| {
                            // Sem isto o clique sobe até a aba, que faria active_tab = i depois da remoção
                            // (índice inexistente ao fechar a última aba → pânico no render).
                            cx.stop_propagation();
                            ws.close_tab(i, window, cx)
                        }))),
                )
            },
        );
        let content = match &self.tabs[self.active_tab] {
            TabView::Sql(t) => t.clone().into_any_element(),
            TabView::Table(t) => t.clone().into_any_element(),
            TabView::Admin(t) => t.clone().into_any_element(),
        };
        // min_w_0: sem ele o item flex mede no mínimo a largura do conteúdo (todas as colunas da
        // grade) e vaza da janela em vez de deixar a grade rolar na horizontal.
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(bar)
            .child(div().flex_1().min_w_0().min_h_0().child(content))
            .into_any_element()
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

    fn pick_context(&mut self, conn_id: &str, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.open_conn(conn_id).map(|o| o.meta.clone()) else { return };
        let session = meta.read(cx).session.clone();
        cx.spawn_in(window, async move |_, cx| {
            let listed = session.query_named(&format!("show {kind}s")).await.map(|(n, rows)| rb_core::snowflake::first_named(&n, &rows, "name"));
            cx.update(|window, cx| match listed {
                Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                Ok(names) => window.open_dialog(cx, move |dialog, _, _| {
                    let list = names.iter().enumerate().fold(v_flex().gap_1(), |l, (i, n)| {
                        let (session, meta, sql) = (session.clone(), meta.clone(), format!("use {kind} {}", quote_always(n)));
                        l.child(Button::new(("ctx", i)).label(n.clone()).ghost().on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let (session, meta, sql) = (session.clone(), meta.clone(), sql.clone());
                            window
                                .spawn(cx, async move |cx| {
                                    let res = session.execute(sql).await;
                                    cx.update(|window, cx| match res {
                                        Ok(_) => meta.update(cx, |m, cx| m.reload_roots(cx)),
                                        Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                                    })
                                })
                                .detach();
                        }))
                    });
                    dialog.title(t!("ws.choose", kind = kind).to_string()).w(px(360.)).child(list)
                }),
            })
        })
        .detach();
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().flex_1().min_h_0().gap_1();
        for (ix, c) in self.connections.iter().enumerate() {
            let open = self.open_conn(&c.id);
            let current = self.current.as_deref() == Some(c.id.as_str());
            let id = c.id.clone();
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(side_item(("conn", ix), Some(logo(c)), c.name.clone(), current, cx).flex_1().on_click(cx.listener(move |ws, _, window, cx| ws.connect(ix, window, cx))))
                    .when(open.is_some(), |r| {
                        r.child(Button::new(("disc-conn", ix)).label("⏏").ghost().tooltip(t!("ws.disconnect").to_string()).on_click(cx.listener(move |ws, _, window, cx| ws.request_disconnect(&id, window, cx))))
                    })
                    .child(Button::new(("edit-conn", ix)).label("✎").ghost().tooltip(t!("conn.edit_title").to_string()).on_click(cx.listener(move |ws, _, window, cx| {
                        let c = ws.connections[ix].clone();
                        conn_dialog::open(Some(c), window, cx)
                    })))
                    .child(Button::new(("del-conn", ix)).label("×").ghost().tooltip(t!("ws.remove_title").to_string()).on_click(cx.listener(move |ws, _, window, cx| ws.confirm_remove(ix, window, cx)))),
            );
            // Detalhes da conexão atual logo abaixo dela; ocupam o espaço que sobra.
            if let Some(o) = open.filter(|_| current && !self.collapsed) {
                let cache = &o.meta.read(cx).cache;
                let ctx = (cache.dialect == Dialect::Snowflake).then(|| (cache.warehouse.clone().unwrap_or_else(|| "—".into()), cache.role.clone().unwrap_or_else(|| "—".into())));
                // Badge só com filtro ativo; os padrões ficam no tooltip.
                let filter = (cache.dialect == Dialect::Snowflake).then(|| {
                    let badge = (!cache.db_filter.is_empty()).then(|| format!("{}/{}", cache.visible_databases().count(), cache.databases.len()));
                    let tip = if cache.db_filter.is_empty() { t!("filter.title").to_string() } else { cache.db_filter.join(", ") };
                    (badge, tip)
                });
                let (wh_id, role_id, filter_id, admin_id) = (c.id.clone(), c.id.clone(), c.id.clone(), c.id.clone());
                list = list.child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .pl_3()
                        .gap_1()
                        .when_some(ctx, |d, (wh, role)| {
                            // Lado a lado, cada um com metade da largura: o valor corta com "…" e aparece inteiro no tooltip.
                            d.child(
                                h_flex()
                                    .gap_1()
                                    .child(chip("pick-wh", IconName::Cpu, format!("Warehouse: {wh}"), wh, cx).on_click(cx.listener(move |ws, _, w, cx| ws.pick_context(&wh_id, "warehouse", w, cx))))
                                    .child(chip("pick-role", IconName::CircleUser, format!("Role: {role}"), role, cx).on_click(cx.listener(move |ws, _, w, cx| ws.pick_context(&role_id, "role", w, cx)))),
                            )
                            .child(
                                side_item("adm-open", Some(Icon::new(IconName::Users)), t!("admin.title").to_string(), false, cx)
                                    .on_click(cx.listener(move |ws, _, w, cx| ws.open_admin(&admin_id, w, cx))),
                            )
                        })
                        .when_some(filter, |d, (badge, tip)| {
                            let theme = cx.theme();
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
                            d.child(
                                h_flex()
                                    .justify_between()
                                    .pt_1()
                                    .pr_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("tree.databases").to_string().to_uppercase())
                                    .child(funnel),
                            )
                        })
                        .child(div().flex_1().min_h_0().child(o.tree.clone())),
                );
            }
        }
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child(t!("ws.connections").to_string())
                    .child(Button::new("new-conn").label("+").ghost().tooltip(t!("conn.new_title").to_string()).on_click(cx.listener(|_, _, window, cx| conn_dialog::open(None, window, cx)))),
            )
            .child(list)
            .when(self.connecting, |d| d.child(t!("ws.connecting").to_string()))
            // `list` cresce (flex_1): o seletor fica no pé da barra.
            .child(side_item("lang", None, format!("🌐 {}", lang::label(cx.global::<Lang>().0)), false, cx).on_click(cx.listener(|ws, _, w, cx| ws.pick_language(w, cx))))
            .child(side_item("theme", None, format!("◐ {}", appearance::label(cx.global::<Appearance>().0, cx)), false, cx).on_click(cx.listener(|ws, _, w, cx| ws.pick_theme(w, cx))))
    }
}

/// Pílula de contexto (warehouse/role): ícone + valor cortado; o valor inteiro vai no tooltip.
fn chip(id: &'static str, icon: IconName, tip: String, value: String, cx: &App) -> Stateful<Div> {
    let t = cx.theme();
    h_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .gap_1()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(t.border)
        .text_xs()
        .cursor_pointer()
        .hover(|d| d.bg(t.secondary_hover))
        .child(Icon::new(icon).xsmall().flex_shrink_0().text_color(t.muted_foreground))
        .child(div().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(value))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|ws, _: &NewSqlTab, window, cx| ws.new_sql_tab(window, cx)))
            .on_action(cx.listener(|ws, _: &QuickOpen, window, cx| ws.quick_open(window, cx)))
            .on_action(cx.listener(|ws, _: &ShowHistory, window, cx| ws.show_history(window, cx)))
            .on_action(cx.listener(|ws, _: &CloseTab, window, cx| ws.close_tab(ws.active_tab, window, cx)))
            .on_action(cx.listener(|ws, _: &Quit, _, cx| {
                ws.save_all(cx);
                cx.quit();
            }))
            .child(
                h_resizable("sidebar-split")
                    .child(resizable_panel().size(px(260.)).size_range(px(180.)..px(640.)).child(self.render_sidebar(cx)))
                    .child(resizable_panel().child(self.render_main(cx))),
            )
    }
}
