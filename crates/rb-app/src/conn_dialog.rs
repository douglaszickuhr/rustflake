use std::cell::Cell;
use rust_i18n::t;
use std::rc::Rc;

use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{IconName, IndexPath, Selectable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use rb_core::config::{ConnectionConfig, SnowflakeAuth, SnowflakeConfig, SshConfig, SslMode, new_id};
use rb_core::ssh::{SshEnv, host_aliases};

use crate::workspace::{Workspace, kind_logo};

const SSL_MODES: [&str; 3] = ["disable", "prefer", "require"];

struct Fields {
    id: String,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    ssl: Entity<SelectState<Vec<&'static str>>>,
    ssh_host: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_key: Entity<InputState>,
    ssh_passphrase: Entity<InputState>,
    /// true = Snowflake.
    snowflake: Rc<Cell<bool>>,
    sf_auth: Rc<Cell<SnowflakeAuth>>,
    sf_account: Entity<InputState>,
    sf_key: Entity<InputState>,
    sf_key_pass: Entity<InputState>,
    sf_warehouse: Entity<InputState>,
    sf_role: Entity<InputState>,
    sf_schema: Entity<InputState>,
}

/// Bloco Snowflake a partir dos campos do diálogo.
fn parse_snowflake(account: &str, auth: SnowflakeAuth, key: &str, warehouse: &str, role: &str, schema: &str) -> Result<SnowflakeConfig, String> {
    if account.is_empty() {
        return Err(t!("conn.err_account").to_string());
    }
    if auth == SnowflakeAuth::KeyPair && key.is_empty() {
        return Err(t!("conn.err_key_path").to_string());
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Ok(SnowflakeConfig { account: account.into(), auth, private_key: opt(key), warehouse: opt(warehouse), role: opt(role), schema: opt(schema), db_filter: Vec::new() })
}

/// Bloco SSH a partir dos campos do diálogo; host vazio = sem túnel.
fn parse_ssh(host: &str, user: &str, port: &str, key: &str) -> Result<Option<SshConfig>, String> {
    if host.is_empty() {
        return Ok(None);
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let port = match port {
        "" => None,
        p => Some(p.parse::<u16>().map_err(|_| t!("conn.err_ssh_port").to_string())?),
    };
    Ok(Some(SshConfig { host: host.into(), user: opt(user), port, identity_file: opt(key) }))
}

impl Fields {
    /// (config, senha ou PAT, passphrase da chave SSH ou Snowflake); vazios mantêm o Keychain.
    fn read(&self, cx: &App) -> Result<(ConnectionConfig, String, String), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        if self.snowflake.get() {
            let snowflake = parse_snowflake(&v(&self.sf_account), self.sf_auth.get(), &v(&self.sf_key), &v(&self.sf_warehouse), &v(&self.sf_role), &v(&self.sf_schema))?;
            if v(&self.name).is_empty() || v(&self.user).is_empty() {
                return Err(t!("conn.err_name_user").to_string());
            }
            let cfg = ConnectionConfig {
                id: self.id.clone(),
                name: v(&self.name),
                host: String::new(),
                port: 0,
                database: v(&self.database),
                user: v(&self.user),
                ssl: SslMode::Disable,
                ssh: None,
                snowflake: Some(snowflake),
            };
            return Ok((cfg, self.password.read(cx).value().to_string(), self.sf_key_pass.read(cx).value().to_string()));
        }
        let port = v(&self.port).parse::<u16>().map_err(|_| t!("conn.err_port").to_string())?;
        let ssl = match self.ssl.read(cx).selected_value().copied() {
            Some("prefer") => SslMode::Prefer,
            Some("require") => SslMode::Require,
            _ => SslMode::Disable,
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
            snowflake: None,
        };
        if cfg.name.is_empty() || cfg.host.is_empty() || cfg.database.is_empty() || cfg.user.is_empty() {
            return Err(t!("conn.err_required").to_string());
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
            t.identity_files.first().map_or(t!("conn.default_keys").to_string(), |p| p.display().to_string()),
        ),
        None => Default::default(),
    };
    user.update(cx, |s, cx| s.set_placeholder(u, window, cx)).ok();
    port.update(cx, |s, cx| s.set_placeholder(p, window, cx)).ok();
    key.update(cx, |s, cx| s.set_placeholder(k, window, cx)).ok();
}

/// Caminho escolhido no seletor do macOS; dentro do home vira `~/…` (o SSH e o Snowflake expandem).
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// Campo de caminho de chave com "Escolher…", que abre o seletor de arquivos do macOS.
fn key_field(id: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    let target = input.downgrade();
    h_flex().gap_2().child(div().flex_1().child(Input::new(input))).child(Button::new(id).label(t!("conn.choose_file").to_string()).on_click(move |_, window, cx| {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some(t!("conn.choose_file").to_string().into()) });
        let target = target.clone();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = rx.await else { return };
                let Some(path) = paths.first() else { return };
                let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
                let value = tilde(path, &home);
                cx.update(|window, cx| target.update(cx, |s, cx| s.set_value(value, window, cx))).ok();
            })
            .detach();
    }))
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
        snowflake: None,
    });
    let ssh = c.ssh.clone();
    let sf = c.snowflake.clone();
    let sfv = |get: fn(&SnowflakeConfig) -> Option<String>| sf.as_ref().and_then(get).unwrap_or_default();
    let input = |window: &mut Window, cx: &mut Context<Workspace>, value: String| cx.new(|cx| InputState::new(window, cx).default_value(value));
    let masked = |window: &mut Window, cx: &mut Context<Workspace>| cx.new(|cx| InputState::new(window, cx).masked(true));
    let ssl = match c.ssl {
        SslMode::Disable => 0,
        SslMode::Prefer => 1,
        SslMode::Require => 2,
    };
    let f = Rc::new(Fields {
        id: c.id.clone(),
        name: input(window, cx, c.name),
        host: input(window, cx, c.host),
        port: input(window, cx, c.port.to_string()),
        database: input(window, cx, c.database),
        user: input(window, cx, c.user),
        password: masked(window, cx),
        ssl: cx.new(|cx| SelectState::new(SSL_MODES.to_vec(), Some(IndexPath::new(ssl)), window, cx)),
        ssh_host: input(window, cx, ssh.as_ref().map(|s| s.host.clone()).unwrap_or_default()),
        ssh_user: input(window, cx, ssh.as_ref().and_then(|s| s.user.clone()).unwrap_or_default()),
        ssh_port: input(window, cx, ssh.as_ref().and_then(|s| s.port).map(|p| p.to_string()).unwrap_or_default()),
        ssh_key: input(window, cx, ssh.as_ref().and_then(|s| s.identity_file.clone()).unwrap_or_default()),
        ssh_passphrase: masked(window, cx),
        snowflake: Rc::new(Cell::new(sf.is_some())),
        sf_auth: Rc::new(Cell::new(sf.as_ref().map(|s| s.auth).unwrap_or_default())),
        sf_account: input(window, cx, sfv(|s| Some(s.account.clone()))),
        sf_key: input(window, cx, sfv(|s| s.private_key.clone())),
        sf_key_pass: masked(window, cx),
        sf_warehouse: input(window, cx, sfv(|s| s.warehouse.clone())),
        sf_role: input(window, cx, sfv(|s| s.role.clone())),
        sf_schema: input(window, cx, sfv(|s| s.schema.clone())),
    });

    // ponytail: lido uma vez ao abrir o diálogo; um alias novo no arquivo pede reabrir.
    let aliases = Rc::new(host_aliases(&SshEnv::system()));
    let (u, p, k) = (f.ssh_user.downgrade(), f.ssh_port.downgrade(), f.ssh_key.downgrade());
    let resolved_targets = (u.clone(), p.clone(), k.clone());
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
    let (title, keep) = if editing { (t!("conn.edit_title").to_string(), format!(" {}", t!("conn.keep_current"))) } else { (t!("conn.new_title").to_string(), String::new()) };
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let (f, ws, aliases, targets) = (f.clone(), ws.clone(), aliases.clone(), resolved_targets.clone());
        // O builder roda a cada render: os botões de tipo e de autenticação trocam os campos.
        let snowflake = f.snowflake.get();
        let tipo = {
            let (k1, k2) = (f.snowflake.clone(), f.snowflake.clone());
            h_flex()
                .gap_2()
                .child(Button::new("kind-pg").icon(kind_logo(false)).label("Postgres").selected(!snowflake).on_click(move |_, w, _| {
                    k1.set(false);
                    w.refresh();
                }))
                .child(Button::new("kind-sf").icon(kind_logo(true)).label("Snowflake").selected(snowflake).on_click(move |_, w, _| {
                    k2.set(true);
                    w.refresh();
                }))
        };
        let form = v_form().child(field().label(t!("conn.kind").to_string()).child(tipo)).child(field().label(t!("conn.name").to_string()).child(Input::new(&f.name)));
        let form = if snowflake {
            let auth = f.sf_auth.get();
            let pick = |id: &'static str, label: String, a: SnowflakeAuth| {
                let cell = f.sf_auth.clone();
                Button::new(id).label(label).selected(auth == a).on_click(move |_, w, _| {
                    cell.set(a);
                    w.refresh();
                })
            };
            let mut form = form
                .child(field().label(t!("conn.account").to_string()).child(Input::new(&f.sf_account)))
                .child(field().label(t!("conn.user").to_string()).child(Input::new(&f.user)))
                .child(
                    field().label(t!("conn.auth").to_string()).child(
                        h_flex()
                            .gap_2()
                            .child(pick("auth-sso", t!("conn.auth_sso").to_string(), SnowflakeAuth::ExternalBrowser))
                            .child(pick("auth-key", "Key-pair".into(), SnowflakeAuth::KeyPair))
                            .child(pick("auth-pat", "PAT".into(), SnowflakeAuth::Pat)),
                    ),
                );
            if auth == SnowflakeAuth::KeyPair {
                form = form
                    .child(field().label(t!("conn.sf_key").to_string()).child(key_field("sf-key-pick", &f.sf_key)))
                    .child(field().label(format!("{}{keep}", t!("conn.key_passphrase"))).child(Input::new(&f.sf_key_pass).mask_toggle()));
            }
            if auth == SnowflakeAuth::Pat {
                form = form.child(field().label(format!("{}{keep}", t!("conn.pat"))).child(Input::new(&f.password).mask_toggle()));
            }
            form.child(field().label("Warehouse").child(Input::new(&f.sf_warehouse)))
                .child(field().label("Role").child(Input::new(&f.sf_role)))
                .child(field().label("Database").child(Input::new(&f.database)))
                .child(field().label("Schema").child(Input::new(&f.sf_schema)))
        } else {
            form.child(field().label("Host").child(Input::new(&f.host)))
                    .child(field().label(t!("conn.port").to_string()).child(Input::new(&f.port)))
                    .child(field().label(t!("conn.database").to_string()).child(Input::new(&f.database)))
                    .child(field().label(t!("conn.user").to_string()).child(Input::new(&f.user)))
                    .child(field().label(format!("{}{keep}", t!("conn.password"))).child(Input::new(&f.password).mask_toggle()))
                    .child(field().label("SSL").child(Select::new(&f.ssl)))
                    .child(field().label(t!("conn.ssh_host").to_string()).child(ssh_host_field(&f.ssh_host, aliases, targets)))
                    .child(field().label(t!("conn.ssh_user").to_string()).child(Input::new(&f.ssh_user)))
                    .child(field().label(t!("conn.ssh_port").to_string()).child(Input::new(&f.ssh_port)))
                    .child(field().label(t!("conn.ssh_key").to_string()).child(key_field("ssh-key-pick", &f.ssh_key)))
                .child(field().label(format!("{}{keep}", t!("conn.key_passphrase"))).child(Input::new(&f.ssh_passphrase).mask_toggle()))
        };
        dialog
            .title(title.clone())
            .w(px(480.))
            .child(form)
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("conn-cancel").label(t!("common.cancel").to_string()).on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("conn-save").label(t!("common.save").to_string()).primary().on_click(move |_, window, cx| {
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

type Resolved = (WeakEntity<InputState>, WeakEntity<InputState>, WeakEntity<InputState>);

/// Host SSH: digitável, com os aliases do `~/.ssh/config` num menu ao lado.
fn ssh_host_field(input: &Entity<InputState>, aliases: Rc<Vec<String>>, targets: Resolved) -> impl IntoElement {
    let host = input.downgrade();
    h_flex().gap_2().child(div().flex_1().child(Input::new(input))).when(!aliases.is_empty(), |d| {
        d.child(Button::new("ssh-aliases").icon(IconName::ChevronDown).tooltip(t!("conn.ssh_aliases").to_string()).dropdown_menu(move |menu, _, _| {
            aliases.iter().fold(menu, |m, a| {
                let (a, host, (u, p, k)) = (a.clone(), host.clone(), targets.clone());
                m.item(PopupMenuItem::new(a.clone()).on_click(move |_, window, cx| {
                    host.update(cx, |s, cx| s.set_value(a.clone(), window, cx)).ok();
                    show_resolved(&a, &u, &p, &k, window, cx);
                }))
            })
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn snowflake_fields() {
        let c = parse_snowflake("xy12345.us-east-1", SnowflakeAuth::KeyPair, "~/.ssh/sf.p8", "WH", "", "PUBLIC").unwrap();
        assert_eq!(c.private_key.as_deref(), Some("~/.ssh/sf.p8"));
        assert_eq!((c.warehouse.as_deref(), c.role, c.schema.as_deref()), (Some("WH"), None, Some("PUBLIC")));
        assert!(parse_snowflake("", SnowflakeAuth::Pat, "", "", "", "").is_err(), "conta obrigatória");
        assert!(parse_snowflake("a", SnowflakeAuth::KeyPair, "", "", "", "").is_err(), "keypair sem chave");
    }

    #[::core::prelude::v1::test]
    fn chosen_key_inside_home_is_shortened() {
        let home = Path::new("/Users/eu");
        assert_eq!(tilde(Path::new("/Users/eu/.ssh/id_ed25519"), home), "~/.ssh/id_ed25519");
        assert_eq!(tilde(Path::new("/etc/key.p8"), home), "/etc/key.p8");
        assert_eq!(tilde(Path::new("/k"), Path::new("")), "/k");
    }

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
