use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::{self, ConfigFile, ConnectionConfig};
use rb_core::secrets;

use crate::meta::{ConnTarget, MetaStore};
use crate::{CloseTab, Db, NewSqlTab, conn_dialog};

pub struct Active {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
}

pub struct Workspace {
    focus: FocusHandle,
    pub connections: Vec<ConnectionConfig>,
    pub active: Option<Active>,
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
        Workspace { focus: cx.focus_handle(), connections, active: None, connecting: false }
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
                        ws.active = Some(Active { conn, meta });
                        ws.on_connected(window, cx);
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Falha ao conectar: {e}")), cx),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Ponto de extensão: Tarefas 12 e 14 criam a árvore e as abas aqui.
    fn on_connected(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

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
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.active {
            Some(a) => format!("{} · {} schemas", a.conn.cfg.name, a.meta.read(cx).cache.schemas.len()),
            None => "Nenhuma conexão aberta".into(),
        };
        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|_, _: &NewSqlTab, _, _| {}))
            .on_action(cx.listener(|_, _: &CloseTab, _, _| {}))
            .child(div().w(px(260.)).h_full().border_r_1().border_color(cx.theme().border).child(self.render_sidebar(cx)))
            .child(v_flex().flex_1().h_full().items_center().justify_center().child(status))
    }
}
