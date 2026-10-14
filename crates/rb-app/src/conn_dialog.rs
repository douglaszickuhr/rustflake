use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::h_flex;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use rb_core::config::{ConnectionConfig, SslMode, new_id};

use crate::workspace::Workspace;

struct Fields {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    ssl: Entity<InputState>,
}

impl Fields {
    fn read(&self, cx: &App) -> Result<(ConnectionConfig, String), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let port = v(&self.port).parse::<u16>().map_err(|_| "porta inválida".to_string())?;
        let ssl = match v(&self.ssl).as_str() {
            "" | "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            other => return Err(format!("ssl deve ser disable, prefer ou require (não {other})")),
        };
        let cfg = ConnectionConfig {
            id: new_id(),
            name: v(&self.name),
            host: v(&self.host),
            port,
            database: v(&self.database),
            user: v(&self.user),
            ssl,
            ssh: None,
        };
        if cfg.name.is_empty() || cfg.host.is_empty() || cfg.database.is_empty() || cfg.user.is_empty() {
            return Err("preencha nome, host, banco e usuário".into());
        }
        Ok((cfg, self.password.read(cx).value().to_string()))
    }
}

pub fn open(window: &mut Window, cx: &mut Context<Workspace>) {
    let input = |window: &mut Window, cx: &mut Context<Workspace>, value: &str| {
        let value = value.to_string();
        cx.new(|cx| InputState::new(window, cx).default_value(value))
    };
    let f = std::rc::Rc::new(Fields {
        name: input(window, cx, "local"),
        host: input(window, cx, "localhost"),
        port: input(window, cx, "5432"),
        database: input(window, cx, "postgres"),
        user: input(window, cx, "postgres"),
        password: cx.new(|cx| InputState::new(window, cx).masked(true)),
        ssl: input(window, cx, "disable"),
    });
    let ws = cx.entity().downgrade();

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let (f, ws) = (f.clone(), ws.clone());
        dialog
            .title("Nova conexão")
            .w(px(460.))
            .child(
                v_form()
                    .child(field().label("Nome").child(Input::new(&f.name)))
                    .child(field().label("Host").child(Input::new(&f.host)))
                    .child(field().label("Porta").child(Input::new(&f.port)))
                    .child(field().label("Banco").child(Input::new(&f.database)))
                    .child(field().label("Usuário").child(Input::new(&f.user)))
                    .child(field().label("Senha (vai para o Keychain)").child(Input::new(&f.password).mask_toggle()))
                    .child(field().label("SSL (disable | prefer | require)").child(Input::new(&f.ssl))),
            )
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("conn-cancel").label("Cancelar").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("conn-save").label("Salvar").primary().on_click(move |_, window, cx| {
                        match f.read(cx) {
                            Ok((cfg, password)) => {
                                ws.update(cx, |ws, cx| ws.add_connection(cfg, password, window, cx)).ok();
                                window.close_dialog(cx);
                            }
                            Err(msg) => window.push_notification(Notification::error(msg), cx),
                        }
                    })),
            )
    });
}
