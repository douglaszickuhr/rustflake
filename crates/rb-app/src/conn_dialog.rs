use std::rc::Rc;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::h_flex;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use rb_core::config::{ConnectionConfig, SshConfig, SslMode, new_id};
use rb_core::ssh::SshEnv;

use crate::workspace::Workspace;

struct Fields {
    id: String,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    ssl: Entity<InputState>,
    ssh_host: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_key: Entity<InputState>,
    ssh_passphrase: Entity<InputState>,
}

/// Bloco SSH a partir dos campos do diálogo; host vazio = sem túnel.
fn parse_ssh(host: &str, user: &str, port: &str, key: &str) -> Result<Option<SshConfig>, String> {
    if host.is_empty() {
        return Ok(None);
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let port = match port {
        "" => None,
        p => Some(p.parse::<u16>().map_err(|_| "porta SSH inválida".to_string())?),
    };
    Ok(Some(SshConfig { host: host.into(), user: opt(user), port, identity_file: opt(key) }))
}

impl Fields {
    /// (config, senha, passphrase); senha e passphrase vazias mantêm o que está no Keychain.
    fn read(&self, cx: &App) -> Result<(ConnectionConfig, String, String), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let port = v(&self.port).parse::<u16>().map_err(|_| "porta inválida".to_string())?;
        let ssl = match v(&self.ssl).as_str() {
            "" | "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            other => return Err(format!("ssl deve ser disable, prefer ou require (não {other})")),
        };
        let cfg = ConnectionConfig {
            id: self.id.clone(),
            name: v(&self.name),
            host: v(&self.host),
            port,
            database: v(&self.database),
            user: v(&self.user),
            ssl,
            ssh: parse_ssh(&v(&self.ssh_host), &v(&self.ssh_user), &v(&self.ssh_port), &v(&self.ssh_key))?,
        };
        if cfg.name.is_empty() || cfg.host.is_empty() || cfg.database.is_empty() || cfg.user.is_empty() {
            return Err("preencha nome, host, banco e usuário".into());
        }
        Ok((cfg, self.password.read(cx).value().to_string(), self.ssh_passphrase.read(cx).value().to_string()))
    }
}

/// Mostra como placeholder o que o `~/.ssh/config` resolve para o host digitado.
fn show_resolved(host: &str, user: &WeakEntity<InputState>, port: &WeakEntity<InputState>, key: &WeakEntity<InputState>, window: &mut Window, cx: &mut App) {
    let resolved = (!host.is_empty())
        .then(|| SshConfig { host: host.into(), user: None, port: None, identity_file: None }.resolve(&SshEnv::system()).ok())
        .flatten();
    let (u, p, k) = match resolved {
        Some(t) => (
            t.user,
            t.port.to_string(),
            t.identity_files.first().map_or("agente / chaves padrão".into(), |p| p.display().to_string()),
        ),
        None => Default::default(),
    };
    user.update(cx, |s, cx| s.set_placeholder(u, window, cx)).ok();
    port.update(cx, |s, cx| s.set_placeholder(p, window, cx)).ok();
    key.update(cx, |s, cx| s.set_placeholder(k, window, cx)).ok();
}

/// Nova conexão (`None`) ou edição de uma existente, mantendo o id.
pub fn open(existing: Option<ConnectionConfig>, window: &mut Window, cx: &mut Context<Workspace>) {
    let editing = existing.is_some();
    let c = existing.unwrap_or_else(|| ConnectionConfig {
        id: new_id(),
        name: "local".into(),
        host: "localhost".into(),
        port: 5432,
        database: "postgres".into(),
        user: "postgres".into(),
        ssl: SslMode::Disable,
        ssh: None,
    });
    let ssh = c.ssh.clone();
    let input = |window: &mut Window, cx: &mut Context<Workspace>, value: String| cx.new(|cx| InputState::new(window, cx).default_value(value));
    let masked = |window: &mut Window, cx: &mut Context<Workspace>| cx.new(|cx| InputState::new(window, cx).masked(true));
    let ssl = match c.ssl {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
    };
    let f = Rc::new(Fields {
        id: c.id.clone(),
        name: input(window, cx, c.name),
        host: input(window, cx, c.host),
        port: input(window, cx, c.port.to_string()),
        database: input(window, cx, c.database),
        user: input(window, cx, c.user),
        password: masked(window, cx),
        ssl: input(window, cx, ssl.into()),
        ssh_host: input(window, cx, ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default()),
        ssh_user: input(window, cx, ssh.as_ref().and_then(|s| s.user.clone()).unwrap_or_default()),
        ssh_port: input(window, cx, ssh.as_ref().and_then(|s| s.port).map(|p| p.to_string()).unwrap_or_default()),
        ssh_key: input(window, cx, ssh.as_ref().and_then(|s| s.identity_file.clone()).unwrap_or_default()),
        ssh_passphrase: masked(window, cx),
    });

    let (u, p, k) = (f.ssh_user.downgrade(), f.ssh_port.downgrade(), f.ssh_key.downgrade());
    // ponytail: lê o ~/.ssh/config na thread de UI ao sair do campo; arquivo pequeno.
    let initial = f.ssh_host.read(cx).value().to_string();
    show_resolved(&initial, &u, &p, &k, window, cx);
    cx.subscribe_in(&f.ssh_host, window, move |_, state, ev: &InputEvent, window, cx| {
        if matches!(ev, InputEvent::Blur) {
            let host = state.read(cx).value().trim().to_string();
            show_resolved(&host, &u, &p, &k, window, cx);
        }
    })
    .detach();

    let ws = cx.entity().downgrade();
    let (title, keep) = if editing { ("Editar conexão", " (em branco mantém a atual)") } else { ("Nova conexão", "") };
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let (f, ws) = (f.clone(), ws.clone());
        dialog
            .title(title)
            .w(px(460.))
            .child(
                v_form()
                    .child(field().label("Nome").child(Input::new(&f.name)))
                    .child(field().label("Host").child(Input::new(&f.host)))
                    .child(field().label("Porta").child(Input::new(&f.port)))
                    .child(field().label("Banco").child(Input::new(&f.database)))
                    .child(field().label("Usuário").child(Input::new(&f.user)))
                    .child(field().label(format!("Senha{keep}")).child(Input::new(&f.password).mask_toggle()))
                    .child(field().label("SSL (disable | prefer | require)").child(Input::new(&f.ssl)))
                    .child(field().label("Túnel SSH: host ou alias do ~/.ssh/config (vazio = sem túnel)").child(Input::new(&f.ssh_host)))
                    .child(field().label("Usuário SSH").child(Input::new(&f.ssh_user)))
                    .child(field().label("Porta SSH").child(Input::new(&f.ssh_port)))
                    .child(field().label("Chave privada").child(Input::new(&f.ssh_key)))
                    .child(field().label(format!("Passphrase da chave{keep}")).child(Input::new(&f.ssh_passphrase).mask_toggle())),
            )
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("conn-cancel").label("Cancelar").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("conn-save").label("Salvar").primary().on_click(move |_, window, cx| {
                        match f.read(cx) {
                            Ok((cfg, password, passphrase)) => {
                                let saved = ws.update(cx, |ws, cx| ws.save_connection(cfg, password, passphrase, window, cx)).unwrap_or(false);
                                if saved {
                                    window.close_dialog(cx);
                                }
                            }
                            Err(msg) => window.push_notification(Notification::error(msg), cx),
                        }
                    })),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn empty_ssh_host_means_no_tunnel() {
        assert_eq!(parse_ssh("", "u", "22", "k"), Ok(None));
    }

    #[::core::prelude::v1::test]
    fn ssh_fields_become_optional_overrides() {
        let ssh = parse_ssh("prod-bastion", "", "", "~/.ssh/id_ed25519").unwrap().unwrap();
        assert_eq!(ssh, SshConfig { host: "prod-bastion".into(), user: None, port: None, identity_file: Some("~/.ssh/id_ed25519".into()) });
        assert_eq!(parse_ssh("b", "deploy", "2200", "").unwrap().unwrap().port, Some(2200));
        assert!(parse_ssh("b", "", "x", "").is_err());
    }
}
