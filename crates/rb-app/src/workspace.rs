use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::{self, ConfigFile, ConnectionConfig};
use rb_core::secrets;

use crate::meta::{ConnTarget, MetaStore};
use crate::schema_tree::SchemaTree;
use crate::sql_tab::SqlTab;
use crate::{CloseTab, Db, NewSqlTab, conn_dialog};

pub struct Active {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
    pub tree: Entity<SchemaTree>,
}

#[derive(Clone)]
pub enum TabView {
    Sql(Entity<SqlTab>),
}

impl TabView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            TabView::Sql(t) => t.read(cx).title(),
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

    pub fn add_connection(&mut self, cfg: ConnectionConfig, password: String, window: &mut Window, cx: &mut Context<Self>) {
        self.connections.push(cfg.clone());
        let file = ConfigFile { connections: self.connections.clone() };
        let id = cfg.id.clone();
        cx.spawn_in(window, async move |_, cx| {
            let res = cx
                .background_spawn(async move {
                    config::save(&config::connections_path(), &file)?;
                    if !password.is_empty() {
                        secrets::set_password(&id, &password)?;
                    }
                    Ok::<_, rb_core::DbError>(())
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

    fn connect(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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
                let password = cx.background_spawn(async move { secrets::get_password(&id) }).await?;
                let session = engine.connect(&cfg, password.clone()).await?;
                Ok::<_, rb_core::DbError>((ConnTarget { cfg, password }, session))
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
                    Err(e) => window.push_notification(Notification::error(format!("Falha ao conectar: {e}")), cx),
                }
                cx.notify();
            })
        })
        .detach();
    }

    pub fn open_table(&mut self, _schema: String, _table: String, _window: &mut Window, _cx: &mut Context<Self>) {}

    pub fn new_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
        let dir = config::scripts_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            window.push_notification(Notification::error(e.to_string()), cx);
            return;
        }
        let n = (1..).find(|n| !dir.join(format!("script-{n}.sql")).exists()).unwrap_or(1);
        let path = dir.join(format!("script-{n}.sql"));
        self.open_sql_file(path, conn, window, cx);
    }

    fn open_sql_file(&mut self, path: PathBuf, conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.active.as_ref().map(|a| a.meta.clone()) else { return };
        let tab = cx.new(|cx| SqlTab::new(path, conn, meta, window, cx));
        self.tabs.push(TabView::Sql(tab));
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    /// Fechar não apaga: o arquivo vai para scripts/closed/.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if let TabView::Sql(t) = self.tabs.remove(ix) {
            let t = t.read(cx);
            t.save_now(cx);
            let closed = config::scripts_dir().join("closed");
            let moved = std::fs::create_dir_all(&closed).and_then(|_| std::fs::rename(&t.path, closed.join(t.path.file_name().unwrap_or_default())));
            if let Err(e) = moved {
                tracing::warn!("falha ao arquivar script: {e}");
            }
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        cx.notify();
    }

    /// Reabre os scripts salvos (ordenados) ou cria um novo.
    fn on_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
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
                        .suffix(Button::new(("close", i)).label("×").ghost().on_click(cx.listener(move |ws, _, _, cx| ws.close_tab(i, cx)))),
                )
            },
        );
        let content = match &self.tabs[self.active_tab] {
            TabView::Sql(t) => t.clone().into_any_element(),
        };
        v_flex().flex_1().h_full().child(bar).child(div().flex_1().child(content)).into_any_element()
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_1();
        for (ix, c) in self.connections.iter().enumerate() {
            let active = self.active.as_ref().is_some_and(|a| a.conn.cfg.id == c.id);
            let mut b = Button::new(("conn", ix)).label(c.name.clone()).ghost();
            if active {
                b = b.primary();
            }
            list = list.child(b.on_click(cx.listener(move |ws, _, window, cx| ws.connect(ix, window, cx))));
        }
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child("Conexões")
                    .child(Button::new("new-conn").label("+").ghost().on_click(cx.listener(|_, _, window, cx| conn_dialog::open(window, cx)))),
            )
            .child(list)
            .when(self.connecting, |d| d.child("conectando…"))
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
            .on_action(cx.listener(|ws, _: &CloseTab, _, cx| ws.close_tab(ws.active_tab, cx)))
            .child(div().w(px(260.)).h_full().border_r_1().border_color(cx.theme().border).child(self.render_sidebar(cx)))
            .child(self.render_main(cx))
    }
}
