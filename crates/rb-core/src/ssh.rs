//! Túnel SSH em processo: resolução do `~/.ssh/config`, autenticação e known_hosts.

use std::path::{Path, PathBuf};

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
