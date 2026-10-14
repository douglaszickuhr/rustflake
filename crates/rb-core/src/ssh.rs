//! Túnel SSH em processo: resolução do `~/.ssh/config`, autenticação e known_hosts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, Msg};
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelStream};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::DbError;
use crate::config::SshConfig;

/// De onde vem a configuração SSH do usuário. Injetável para os testes não tocarem o `~/.ssh` real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshEnv {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub known_hosts: PathBuf,
    pub agent: Option<PathBuf>,
    pub user: String,
}

impl SshEnv {
    pub fn system() -> SshEnv {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        SshEnv {
            config_file: home.join(".ssh/config"),
            known_hosts: home.join(".ssh/known_hosts"),
            agent: std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from),
            user: std::env::var("USER").unwrap_or_default(),
            home,
        }
    }
}

/// Valores efetivos depois de aplicar TOML > `~/.ssh/config` > padrões do OpenSSH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<PathBuf>,
    /// As chaves vieram do TOML ou de `IdentityFile`, não dos padrões.
    pub identities_explicit: bool,
    pub agent: Option<PathBuf>,
    pub known_hosts: PathBuf,
}

const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

/// O ssh2-config não avalia `Match` e aplicaria o bloco a todos os hosts; a spec manda ignorá-lo.
fn strip_match_blocks(text: &str) -> String {
    let mut skipping = false;
    text.lines()
        .filter(|l| {
            match l.split_whitespace().next().map(str::to_ascii_lowercase).as_deref() {
                Some("match") => skipping = true,
                Some("host") => skipping = false,
                _ => {}
            }
            !skipping
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_config(path: &Path) -> Result<ssh2_config::SshConfig, DbError> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(ssh2_config::SshConfig::default()) };
    let rules = ssh2_config::ParseRule::ALLOW_UNKNOWN_FIELDS | ssh2_config::ParseRule::ALLOW_UNSUPPORTED_FIELDS;
    ssh2_config::SshConfig::default()
        .parse(&mut strip_match_blocks(&text).as_bytes(), rules)
        .map_err(|e| DbError::Ssh(format!("{}: {e}", path.display())))
}

impl SshConfig {
    pub fn resolve(&self, env: &SshEnv) -> Result<SshTarget, DbError> {
        let p = read_config(&env.config_file)?.query(&self.host);
        let configured: Vec<PathBuf> = match &self.identity_file {
            Some(f) => vec![expand(f, &env.home)],
            None => p.identity_file.clone().unwrap_or_default(),
        };
        let identities_explicit = !configured.is_empty();
        let identity_files = if identities_explicit {
            configured
        } else {
            DEFAULT_KEYS.iter().map(|k| env.home.join(".ssh").join(k)).filter(|p| p.exists()).collect()
        };
        Ok(SshTarget {
            hostname: p.host_name.clone().unwrap_or_else(|| self.host.clone()),
            port: self.port.or(p.port).unwrap_or(22),
            user: self.user.clone().or(p.user.clone()).unwrap_or_else(|| env.user.clone()),
            identity_files,
            identities_explicit,
            agent: env.agent.clone(),
            known_hosts: env.known_hosts.clone(),
        })
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

impl From<russh::Error> for DbError {
    fn from(e: russh::Error) -> Self {
        DbError::Ssh(e.to_string())
    }
}

/// Confere a chave do host contra o known_hosts durante o handshake.
struct Checker {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl client::Handler for Checker {
    type Error = DbError;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, DbError> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Err(DbError::Ssh(format!("{} apresentou certificado de host, não suportado", self.host)));
        };
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Ok(false) => Err(DbError::HostKeyUnknown {
                host: self.host.clone(),
                port: self.port,
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                key: key.to_openssh().map_err(|e| DbError::Ssh(e.to_string()))?,
            }),
            Err(russh::keys::Error::KeyChanged { line }) => Err(DbError::Ssh(format!(
                "chave do host {} mudou ({}:{line})",
                self.host,
                self.known_hosts.display()
            ))),
            Err(e) => Err(DbError::Ssh(format!("{}: {e}", self.known_hosts.display()))),
        }
    }
}

/// Sessão SSH até o bastion. Cada conexão Postgres usa um canal `direct-tcpip` próprio;
/// se a sessão SSH cair, o próximo canal a reabre.
pub struct Tunnel {
    target: SshTarget,
    passphrase: Option<String>,
    handle: Mutex<Handle<Checker>>,
}

impl Tunnel {
    pub async fn connect(target: SshTarget, passphrase: Option<String>) -> Result<Tunnel, DbError> {
        let handle = open_session(&target, passphrase.as_deref()).await?;
        Ok(Tunnel { target, passphrase, handle: Mutex::new(handle) })
    }

    /// Canal até `host:port`, resolvidos pelo bastion.
    pub async fn open_stream(&self, host: &str, port: u16) -> Result<ChannelStream<Msg>, DbError> {
        let mut h = self.handle.lock().await;
        if h.is_closed() {
            tracing::info!("reconectando túnel SSH");
            *h = open_session(&self.target, self.passphrase.as_deref()).await?;
        }
        h.channel_open_direct_tcpip(host, port.into(), "127.0.0.1", 0)
            .await
            .map(Channel::into_stream)
            .map_err(|e| DbError::Ssh(format!("o bastion recusou abrir canal para {host}:{port}: {e}")))
    }
}

async fn open_session(t: &SshTarget, passphrase: Option<&str>) -> Result<Handle<Checker>, DbError> {
    let unreachable = |cause: String| DbError::Ssh(format!("bastion {}:{} inacessível: {cause}", t.hostname, t.port));
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((t.hostname.as_str(), t.port)))
        .await
        .map_err(|_| unreachable("tempo esgotado".into()))?
        .map_err(|e| unreachable(e.to_string()))?;
    let _ = tcp.set_nodelay(true);
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let checker = Checker { host: t.hostname.clone(), port: t.port, known_hosts: t.known_hosts.clone() };
    let mut h = tokio::time::timeout(CONNECT_TIMEOUT, client::connect_stream(config, tcp, checker))
        .await
        .map_err(|_| unreachable("tempo esgotado no handshake".into()))??;
    authenticate(&mut h, t, passphrase).await?;
    Ok(h)
}

/// Pública de uma chave privada: `<arquivo>.pub` ou, sem passphrase, derivada do privado.
fn public_key_of(path: &Path) -> Option<PublicKey> {
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    PublicKey::read_openssh_file(Path::new(&pub_path))
        .ok()
        .or_else(|| russh::keys::load_secret_key(path, None).ok().map(|k| k.public_key().clone()))
}

fn load_key(path: &Path, passphrase: Option<&str>) -> Result<PrivateKey, DbError> {
    use russh::keys::{Error as KeyError, load_secret_key};
    let shown = path.display();
    match load_secret_key(path, None) {
        Ok(k) => Ok(k),
        Err(KeyError::KeyIsEncrypted) => match passphrase {
            None => Err(DbError::Ssh(format!("{shown} exige passphrase"))),
            Some(p) => load_secret_key(path, Some(p)).map_err(|_| DbError::Ssh(format!("passphrase incorreta para {shown}"))),
        },
        Err(e) => Err(DbError::Ssh(format!("não foi possível ler {shown}: {e}"))),
    }
}

/// Agente primeiro (só as chaves configuradas, ou todas se nenhuma foi configurada), depois arquivos.
/// ponytail: sem chave configurada, um agente com muitas chaves pode esgotar o MaxAuthTries do
/// servidor, como no OpenSSH; a saída é configurar a chave.
async fn authenticate(h: &mut Handle<Checker>, t: &SshTarget, passphrase: Option<&str>) -> Result<(), DbError> {
    let mut agent = match &t.agent {
        Some(sock) => AgentClient::connect_uds(sock).await.ok(),
        None => None,
    };
    let agent_keys: Vec<PublicKey> = match agent.as_mut() {
        Some(a) => a.request_identities().await.unwrap_or_default().iter().map(|i| i.public_key().into_owned()).collect(),
        None => Vec::new(),
    };
    let in_agent = |k: &PublicKey| agent_keys.iter().any(|a| a.key_data() == k.key_data());
    let via_agent: Vec<PublicKey> = if t.identities_explicit {
        t.identity_files.iter().filter_map(|f| public_key_of(f)).filter(|k| in_agent(k)).collect()
    } else {
        agent_keys.clone()
    };
    let rsa_hash = h.best_supported_rsa_hash().await?.flatten();
    let hash_for = |k: &PublicKey| if k.algorithm().is_rsa() { rsa_hash } else { None };

    if let Some(a) = agent.as_mut() {
        for key in &via_agent {
            match h.authenticate_publickey_with(&t.user, key.clone(), hash_for(key), a).await {
                Ok(r) if r.success() => return Ok(()),
                Ok(_) => {}
                Err(e) => tracing::warn!("agente SSH falhou: {e:?}"),
            }
        }
    }

    let mut files = Vec::new();
    let mut key_error = None;
    for path in &t.identity_files {
        let from_agent = public_key_of(path).is_some_and(|k| via_agent.iter().any(|a| a.key_data() == k.key_data()));
        if from_agent || !path.exists() {
            continue;
        }
        let key = match load_key(path, passphrase) {
            Ok(k) => k,
            Err(e) => {
                key_error.get_or_insert(e);
                continue;
            }
        };
        files.push(path.display().to_string());
        let hash = hash_for(key.public_key());
        if h.authenticate_publickey(&t.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?.success() {
            return Ok(());
        }
    }
    if let Some(e) = key_error {
        return Err(e);
    }
    let files = if files.is_empty() { "nenhum arquivo".to_string() } else { files.join(", ") };
    Err(DbError::Ssh(format!(
        "nenhuma chave aceita por {}@{} (tentadas: agente ×{}, {files})",
        t.user,
        t.hostname,
        via_agent.len()
    )))
}

/// Grava a chave do host no known_hosts, depois que o usuário confirmou o fingerprint.
pub fn trust_host(known_hosts: &Path, host: &str, port: u16, key: &str) -> Result<(), DbError> {
    let key = PublicKey::from_openssh(key).map_err(|e| DbError::Ssh(e.to_string()))?;
    russh::keys::known_hosts::learn_known_hosts_path(host, port, &key, known_hosts)
        .map_err(|e| DbError::Ssh(format!("{}: {e}", known_hosts.display())))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn env_with(config: Option<&str>) -> SshEnv {
        static N: AtomicU32 = AtomicU32::new(0);
        let home = std::env::temp_dir().join(format!("rb-sshcfg-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        if let Some(text) = config {
            std::fs::write(home.join(".ssh/config"), text).unwrap();
        }
        SshEnv {
            config_file: home.join(".ssh/config"),
            known_hosts: home.join(".ssh/known_hosts"),
            agent: None,
            user: "eu".into(),
            home,
        }
    }

    fn ssh(host: &str) -> SshConfig {
        SshConfig { host: host.into(), user: None, port: None, identity_file: None }
    }

    const CONFIG: &str = "Host prod-bastion\n  HostName 10.0.0.5\n  User deploy\n  Port 2200\n  IdentityFile /keys/bastion\n\n\
                          Host *.internal\n  User ops\n\n\
                          Host *\n  ServerAliveInterval 30\n  UseKeychain yes\n  AddKeysToAgent yes\n  ControlMaster auto\n  SomethingNew yes\n";

    #[test]
    fn alias_comes_from_ssh_config() {
        let t = ssh("prod-bastion").resolve(&env_with(Some(CONFIG))).unwrap();
        assert_eq!((t.hostname.as_str(), t.port, t.user.as_str()), ("10.0.0.5", 2200, "deploy"));
        assert_eq!(t.identity_files, vec![PathBuf::from("/keys/bastion")]);
        assert!(t.identities_explicit);
    }

    #[test]
    fn wildcard_hosts_match() {
        let t = ssh("db.internal").resolve(&env_with(Some(CONFIG))).unwrap();
        assert_eq!((t.hostname.as_str(), t.port, t.user.as_str()), ("db.internal", 22, "ops"));
    }

    #[test]
    fn toml_fields_win_over_ssh_config() {
        let env = env_with(Some(CONFIG));
        let cfg = SshConfig {
            host: "prod-bastion".into(),
            user: Some("outro".into()),
            port: Some(22),
            identity_file: Some("~/.ssh/outra".into()),
        };
        let t = cfg.resolve(&env).unwrap();
        assert_eq!((t.hostname.as_str(), t.port, t.user.as_str()), ("10.0.0.5", 22, "outro"));
        assert_eq!(t.identity_files, vec![env.home.join(".ssh/outra")]);
    }

    #[test]
    fn defaults_without_config_file() {
        let env = env_with(None);
        std::fs::write(env.home.join(".ssh/id_ed25519"), "").unwrap();
        let t = ssh("bastion.example").resolve(&env).unwrap();
        assert_eq!((t.hostname.as_str(), t.port, t.user.as_str()), ("bastion.example", 22, "eu"));
        assert_eq!(t.identity_files, vec![env.home.join(".ssh/id_ed25519")]);
        assert!(!t.identities_explicit);
        assert_eq!(t.known_hosts, env.known_hosts);
    }

    #[test]
    fn match_blocks_do_not_leak() {
        let env = env_with(Some("Match host x\n  User vazado\nHost a\n  HostName a.example\nHost c\n  Port 2222\n"));
        assert_eq!(ssh("c").resolve(&env).unwrap().user, "eu");
        assert_eq!(ssh("a").resolve(&env).unwrap().hostname, "a.example");
    }
}
