use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::{self, ConfigFile, ConnectionConfig};
use rb_core::dialect::Dialect;
use rb_core::lexer::quote_always;
use rb_core::secrets;
use rb_core::session::Credentials;
use rb_core::ssh::{self, SshEnv};

use crate::meta::{ConnTarget, MetaStore};
use crate::schema_tree::SchemaTree;
use crate::sql_tab::SqlTab;
use crate::table_tab::TableTab;
use crate::{CloseTab, Db, NewSqlTab, Quit, conn_dialog};

pub struct Active {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
    pub tree: Entity<SchemaTree>,
}

#[derive(Clone)]
pub enum TabView {
    Sql(Entity<SqlTab>),
    Table(Entity<TableTab>),
}

impl TabView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            TabView::Sql(t) => t.read(cx).title(),
            TabView::Table(t) => t.read(cx).title(),
        }
    }
}

pub struct Workspace {
    focus: FocusHandle,
    pub connections: Vec<ConnectionConfig>,
    pub active: Option<Active>,
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
        Workspace { focus: cx.focus_handle(), connections, active: None, tabs: Vec::new(), active_tab: 0, connecting: false }
    }

    /// Cria ou atualiza (mesmo id). Senha/passphrase vazias mantêm o Keychain; sem túnel, a
    /// passphrase guardada é apagada. Editar a conexão ativa reconecta. Devolve `false` se recusou.
    pub fn save_connection(&mut self, cfg: ConnectionConfig, password: String, passphrase: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let id = cfg.id.clone();
        let reconnect = self.active.as_ref().is_some_and(|a| a.conn.cfg.id == id);
        if reconnect && self.has_pending_edits(cx) {
            window.push_notification(Notification::error("grave ou descarte as edições pendentes antes de editar a conexão ativa"), cx);
            return false;
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
                    ws.disconnect(cx);
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

    fn connect(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting || self.active.as_ref().is_some_and(|a| a.conn.cfg.id == self.connections[ix].id) {
            return;
        }
        if self.has_pending_edits(cx) {
            window.push_notification(Notification::error("grave ou descarte as edições pendentes antes de trocar de conexão"), cx);
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
                        let meta = cx.new(|cx| MetaStore::new(Arc::new(session), cx));
                        cx.observe(&meta, |_, _, cx| cx.notify()).detach();
                        let ws_weak = cx.entity().downgrade();
                        let open: Rc<dyn Fn(String, String, &mut Window, &mut App)> = Rc::new(move |schema, table, window, cx| {
                            ws_weak.update(cx, |ws, cx| ws.open_table(schema, table, window, cx)).ok();
                        });
                        let tree = cx.new(|cx| SchemaTree::new(meta.clone(), open, cx));
                        ws.active = Some(Active { conn, meta, tree });
                        ws.on_connected(window, cx);
                    }
                    Err(rb_core::DbError::HostKeyUnknown { host, port, fingerprint, key }) => {
                        ws.confirm_trust(ix, host, port, fingerprint, key, window, cx)
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Falha ao conectar: {e}")), cx),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn open_table(&mut self, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let title: SharedString = format!("{schema}.{table}").into();
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Table(_)) && t.title(cx) == title) {
            self.active_tab = ix;
            cx.notify();
            return;
        }
        let (conn, meta) = (active.conn.clone(), active.meta.clone());
        let tab = cx.new(|cx| TableTab::new(schema, table, conn, meta, window, cx));
        self.tabs.push(TabView::Table(tab));
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    pub fn new_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
        match config::new_script(&config::scripts_dir()) {
            Ok(path) => self.open_sql_file(path, conn, window, cx),
            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
        }
    }

    fn has_pending_edits(&self, cx: &App) -> bool {
        self.tabs.iter().any(|t| matches!(t, TabView::Table(t) if t.read(cx).has_pending(cx)))
    }

    /// Grava já o texto de todos os scripts abertos (o autosave espera 1 s).
    fn save_all(&self, cx: &App) {
        for t in &self.tabs {
            if let TabView::Sql(t) = t {
                t.read(cx).save_now(cx);
            }
        }
    }

    fn open_sql_file(&mut self, path: PathBuf, conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.active.as_ref().map(|a| a.meta.clone()) else { return };
        let tab = cx.new(|cx| SqlTab::new(path, conn, meta, window, cx));
        self.tabs.push(TabView::Sql(tab));
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    /// Fechar não apaga: o arquivo vai para scripts/closed/.
    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else { return };
        if let TabView::Table(t) = tab
            && t.read(cx).has_pending(cx)
        {
            window.push_notification(Notification::error("grave (⌘S) ou descarte as edições antes de fechar a aba"), cx);
            return;
        }
        if let TabView::Sql(t) = self.tabs.remove(ix) {
            let t = t.read(cx);
            t.save_now(cx);
            if let Err(e) = config::archive_script(&t.path) {
                tracing::warn!("falha ao arquivar script: {e}");
            }
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        cx.notify();
    }

    /// Reabre os scripts salvos (ordenados) ou cria um novo.
    fn on_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
        self.save_all(cx);
        self.tabs.clear();
        let mut files: Vec<PathBuf> = std::fs::read_dir(config::scripts_dir())
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "sql")).collect())
            .unwrap_or_default();
        files.sort();
        if files.is_empty() {
            self.new_sql_tab(window, cx);
        }
        for f in files {
            self.open_sql_file(f, conn.clone(), window, cx);
        }
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            let msg = match &self.active {
                Some(a) => format!("{} · {} schemas — ⌘T abre um script", a.conn.cfg.name, a.meta.read(cx).cache.schemas.len()),
                None => "Nenhuma conexão aberta".into(),
            };
            return v_flex().flex_1().h_full().items_center().justify_center().child(msg).into_any_element();
        }
        let bar = self.tabs.iter().enumerate().fold(
            TabBar::new("tabs").selected_index(self.active_tab).on_click(cx.listener(|ws, ix: &usize, _, cx| {
                ws.active_tab = *ix;
                cx.notify();
            })),
            |bar, (i, t)| {
                bar.child(
                    Tab::new()
                        .label(t.title(cx))
                        .suffix(Button::new(("close", i)).label("×").ghost().on_click(cx.listener(move |ws, _, window, cx| {
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
                .title("Remover conexão")
                .w(px(420.))
                .child(format!("Remover \"{}\"? A senha salva no Keychain também será apagada.", c.name))
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("rm-cancel").label("Cancelar").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("rm-ok").label("Remover").danger().on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            ws.update(cx, |ws, cx| ws.remove_connection(&id, window, cx)).ok();
                        })),
                )
        });
    }

    /// Tira a conexão do TOML e a senha do Keychain. Se estiver ativa, desconecta antes.
    fn remove_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.as_ref().is_some_and(|a| a.conn.cfg.id == id) {
            if self.has_pending_edits(cx) {
                window.push_notification(Notification::error("grave ou descarte as edições pendentes antes de remover a conexão"), cx);
                return;
            }
            self.disconnect(cx);
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

    /// Salva os scripts, fecha as abas e solta a sessão (e o túnel, se for o último uso).
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.save_all(cx);
        self.tabs.clear();
        self.active_tab = 0;
        self.active = None;
        cx.notify();
    }

    /// Host SSH fora do known_hosts: mostra o fingerprint e, se o usuário confiar, grava e reconecta.
    #[allow(clippy::too_many_arguments)]
    fn confirm_trust(&mut self, ix: usize, host: String, port: u16, fingerprint: String, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (ws, host, key) = (ws.clone(), host.clone(), key.clone());
            dialog
                .title("Host SSH desconhecido")
                .w(px(480.))
                .child(format!("{host}:{port} não está no known_hosts.\n{fingerprint}\nConfiar e continuar?"))
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("trust-cancel").label("Cancelar").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("trust-ok").label("Confiar").primary().on_click(move |_, window, cx| {
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
    fn pick_context(&mut self, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.active.as_ref().map(|a| a.meta.clone()) else { return };
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
                    dialog.title(format!("Escolher {kind}")).w(px(360.)).child(list)
                }),
            })
        })
        .detach();
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_1();
        for (ix, c) in self.connections.iter().enumerate() {
            let active = self.active.as_ref().is_some_and(|a| a.conn.cfg.id == c.id);
            let mut b = Button::new(("conn", ix)).icon(IconName::Database).label(c.name.clone()).ghost();
            if active {
                b = b.primary();
            }
            list = list.child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().child(b.w_full().on_click(cx.listener(move |ws, _, window, cx| ws.connect(ix, window, cx)))))
                    .child(Button::new(("edit-conn", ix)).label("✎").ghost().on_click(cx.listener(move |ws, _, window, cx| {
                        let c = ws.connections[ix].clone();
                        conn_dialog::open(Some(c), window, cx)
                    })))
                    .child(Button::new(("del-conn", ix)).label("×").ghost().on_click(cx.listener(move |ws, _, window, cx| ws.confirm_remove(ix, window, cx)))),
            );
        }
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child("Conexões")
                    .child(Button::new("new-conn").label("+").ghost().on_click(cx.listener(|_, _, window, cx| conn_dialog::open(None, window, cx)))),
            )
            .child(list)
            .when(self.connecting, |d| d.child("conectando…"))
            .when_some(self.active.as_ref().filter(|a| a.meta.read(cx).cache.dialect == Dialect::Snowflake), |d, a| {
                let c = &a.meta.read(cx).cache;
                let wh = c.warehouse.clone().unwrap_or_else(|| "—".into());
                let role = c.role.clone().unwrap_or_else(|| "—".into());
                d.child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("pick-wh").label(format!("WH: {wh}")).ghost().on_click(cx.listener(|ws, _, w, cx| ws.pick_context("warehouse", w, cx))))
                        .child(Button::new("pick-role").label(format!("Role: {role}")).ghost().on_click(cx.listener(|ws, _, w, cx| ws.pick_context("role", w, cx)))),
                )
            })
            .when_some(self.active.as_ref().map(|a| a.tree.clone()), |d, tree| d.child(div().flex_1().child(tree)))
    }
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
