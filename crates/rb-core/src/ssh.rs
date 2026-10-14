//! Túnel SSH em processo: resolução do `~/.ssh/config`, autenticação e known_hosts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, Msg};
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
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

/// Palavra-chave de uma linha do ssh_config (`Host x` ou `Host=x`), em minúsculas, e o resto.
fn keyword(line: &str) -> Option<(String, &str)> {
    let l = line.trim_start();
    let end = l.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(l.len());
    let (k, rest) = l.split_at(end);
    (!k.is_empty() && !k.starts_with('#'))
        .then(|| (k.to_ascii_lowercase(), rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim_end()))
}

/// Curinga do ssh_config (`*` e `?`) num nome de arquivo.
fn wildcard(p: &[u8], n: &[u8]) -> bool {
    match (p.first(), n.first()) {
        (None, None) => true,
        (Some(b'*'), _) => wildcard(&p[1..], n) || (!n.is_empty() && wildcard(p, &n[1..])),
        (Some(b'?'), Some(_)) => wildcard(&p[1..], &n[1..]),
        (Some(a), Some(b)) if a == b => wildcard(&p[1..], &n[1..]),
        _ => false,
    }
}

/// Arquivos de um `Include`; caminho relativo é relativo a `~/.ssh`, como no OpenSSH.
/// ponytail: curinga só no nome do arquivo, não em diretórios.
fn include_paths(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let path = if pattern.starts_with('~') || pattern.starts_with('/') { expand(pattern, home) } else { home.join(".ssh").join(pattern) };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else { return Vec::new() };
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| wildcard(name.as_bytes(), n.as_encoded_bytes())))
        .collect();
    found.sort();
    found
}

/// Junta os `Include` no texto e descarta blocos `Match`. O ssh2-config não avalia `Match` (aplicaria
/// o bloco a todos os hosts) e resolveria os `Include` pelo HOME real, sem tirar o `Match` deles.
/// ponytail: um Include dentro de um bloco Host é inlinado sem restaurar o bloco depois.
fn preprocess(text: &str, home: &Path, depth: u8, out: &mut String) {
    let mut skipping = false;
    for line in text.lines() {
        let kw = keyword(line);
        match kw.as_ref().map(|(k, _)| k.as_str()) {
            Some("match") => skipping = true,
            Some("host") => skipping = false,
            _ => {}
        }
        if skipping {
            continue;
        }
        if let Some((k, rest)) = &kw
            && k == "include"
        {
            for pattern in rest.split_whitespace().filter(|_| depth < 16) {
                for path in include_paths(pattern, home) {
                    if let Ok(t) = std::fs::read_to_string(&path) {
                        preprocess(&t, home, depth + 1, out);
                    }
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
}

fn read_config(env: &SshEnv) -> Result<ssh2_config::SshConfig, DbError> {
    let Ok(raw) = std::fs::read_to_string(&env.config_file) else { return Ok(ssh2_config::SshConfig::default()) };
    let mut text = String::new();
    preprocess(&raw, &env.home, 0, &mut text);
    let rules = ssh2_config::ParseRule::ALLOW_UNKNOWN_FIELDS | ssh2_config::ParseRule::ALLOW_UNSUPPORTED_FIELDS;
    ssh2_config::SshConfig::default()
        .parse(&mut text.as_bytes(), rules)
        .map_err(|e| DbError::Ssh(format!("{}: {e}", env.config_file.display())))
}

impl SshConfig {
    pub fn resolve(&self, env: &SshEnv) -> Result<SshTarget, DbError> {
        let p = read_config(env)?.query(&self.host);
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
    handle: Mutex<Arc<Handle<Checker>>>,
}

impl Tunnel {
    pub async fn connect(target: SshTarget, passphrase: Option<String>) -> Result<Tunnel, DbError> {
        let handle = open_session(&target, passphrase.as_deref()).await?;
        Ok(Tunnel { target, passphrase, handle: Mutex::new(Arc::new(handle)) })
    }

    /// Canal até `host:port`, resolvidos pelo bastion.
    /// A trava cobre só a reconexão: um destino que demora a responder não segura os outros canais.
    pub async fn open_stream(&self, host: &str, port: u16) -> Result<ChannelStream<Msg>, DbError> {
        let h = {
            let mut h = self.handle.lock().await;
            if h.is_closed() {
                tracing::info!("reconectando túnel SSH");
                *h = Arc::new(open_session(&self.target, self.passphrase.as_deref()).await?);
            }
            h.clone()
        };
        tokio::time::timeout(CONNECT_TIMEOUT, h.channel_open_direct_tcpip(host, port.into(), "127.0.0.1", 0))
            .await
            .map_err(|_| DbError::Ssh(format!("{host}:{port} não respondeu pelo bastion em {} s", CONNECT_TIMEOUT.as_secs())))?
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
    let mut config = client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    };
    prefer_known_key_types(&mut config, t);
    let checker = Checker { host: t.hostname.clone(), port: t.port, known_hosts: t.known_hosts.clone() };
    let mut h = tokio::time::timeout(CONNECT_TIMEOUT, client::connect_stream(Arc::new(config), tcp, checker))
        .await
        .map_err(|_| unreachable("tempo esgotado no handshake".into()))??;
    authenticate(&mut h, t, passphrase).await?;
    Ok(h)
}

/// Pede primeiro os tipos de chave de host já gravados no known_hosts, como o OpenSSH. Sem isso,
/// um host conhecido só por ECDSA ou RSA apresentaria ed25519 e pareceria desconhecido.
fn prefer_known_key_types(config: &mut client::Config, t: &SshTarget) {
    let family = |a: &Algorithm| match a {
        Algorithm::Rsa { .. } => Algorithm::Rsa { hash: None },
        other => other.clone(),
    };
    let known: Vec<Algorithm> = russh::keys::known_hosts::known_host_keys_path(&t.hostname, t.port, &t.known_hosts)
        .unwrap_or_default()
        .iter()
        .map(|(_, k)| family(&k.algorithm()))
        .collect();
    if known.is_empty() {
        return;
    }
    let (mut first, rest): (Vec<Algorithm>, Vec<Algorithm>) = config.preferred.key.iter().cloned().partition(|a| known.contains(&family(a)));
    first.extend(rest);
    config.preferred.key = first.into();
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
    fn includes_are_inlined_without_leaking_match() {
        let env = env_with(Some("Include inc.conf conf.d/*.conf\nHost=b\n  HostName b.example\nHost a\n  HostName a.example\n"));
        std::fs::write(env.home.join(".ssh/inc.conf"), "Match host other\n  User vazado\n  Port 2999\nHost incl\n  User de-include\n").unwrap();
        std::fs::create_dir_all(env.home.join(".ssh/conf.d")).unwrap();
        std::fs::write(env.home.join(".ssh/conf.d/x.conf"), "Host g\n  User globbed\n").unwrap();
        let a = ssh("a").resolve(&env).unwrap();
        assert_eq!((a.hostname.as_str(), a.user.as_str(), a.port), ("a.example", "eu", 22));
        assert_eq!(ssh("incl").resolve(&env).unwrap().user, "de-include");
        assert_eq!(ssh("g").resolve(&env).unwrap().user, "globbed");
        assert_eq!(ssh("b").resolve(&env).unwrap().hostname, "b.example");
    }

    #[test]
    fn match_blocks_do_not_leak() {
        let env = env_with(Some("Match host x\n  User vazado\nHost a\n  HostName a.example\nHost c\n  Port 2222\n"));
        assert_eq!(ssh("c").resolve(&env).unwrap().user, "eu");
        assert_eq!(ssh("a").resolve(&env).unwrap().hostname, "a.example");
    }
}
