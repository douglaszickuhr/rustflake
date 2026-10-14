# Túnel SSH e edição de conexão — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Conectar a Postgres atrás de um bastion SSH (agente ou arquivo de chave, `~/.ssh/config`, `known_hosts`) e editar conexões existentes.

**Architecture:** Módulo novo `rb-core/src/ssh.rs` com resolução de config (`ssh2-config`), `Tunnel` (`russh` 0.64.1) que abre canais `direct-tcpip` e verificação de `known_hosts`. A `Session` passa a criar clientes por um `Connector` (direto ou `connect_raw` sobre o canal SSH, sem porta local). O `Engine` compartilha um túnel por conexão do app via cache fraco. Na UI, o diálogo de conexão ganha a seção SSH e o modo edição (✎); a ativação trata `HostKeyUnknown` com um diálogo de confiança.

**Tech Stack:** Rust 1.96 (edition 2024), russh 0.64.1 (backend aws-lc-rs, o mesmo do rustls), ssh2-config 0.8.2, tokio 1.53, tokio-postgres 0.7.18, tokio-postgres-rustls 0.14, gpui-kit 0.7.3.

**Spec:** `docs/superpowers/specs/2026-10-14-ssh-tunnel-design.md`
**Notas da API de UI:** `docs/superpowers/plans/gpui-api-notes.md`

## Global Constraints

- Só macOS. Um salto SSH. Autenticação só por agente e arquivo de chave.
- Versões fixadas: `russh = "0.64.1"`, `ssh2-config = "0.8.2"`; demais como no `Cargo.toml` atual.
- `rb-core` não depende de GPUI e nunca entra em pânico em caminho de dados: erros viram `DbError`.
- A thread de UI não faz I/O de rede nem Keychain (leitura do `~/.ssh/config` e append no `known_hosts` são permitidos: arquivos pequenos, ação do usuário).
- Testes nunca tocam o `~/.ssh` real nem o agente do usuário: usam `SshEnv`/`SshTarget` com caminhos temporários e um `ssh-agent` próprio.
- Segredos: senha do banco na conta `<id>`, passphrase na conta `ssh:<id>`, serviço `dev.rustbeaver`.
- Postgres de teste: container `rb-pg` (trust, porta 55432 no host). sshd de teste: container `rb-sshd` (porta 52222), na rede Docker `rb-net` junto com `rb-pg`.
- Atributo de teste no `rb-app`: `#[::core::prelude::v1::test]` (o prelude do gpui sombreia `test`).
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. `~/.ssh/config` com diretivas desconhecidas, `Match`, `Include` ou `Host *` amplo — deve resolver sem erro e sem vazar valores de blocos `Match` (teste `match_blocks_do_not_leak`, Tarefa 1).
2. Chave em arquivo com passphrase, sem passphrase guardada — erro "exige passphrase", não "nenhuma chave aceita" (teste `key_file_with_passphrase`, Tarefa 2).
3. Bastion que cai durante a sessão — próxima query reabre o SSH e o canal (`Tunnel::open_stream` reconecta se `is_closed`; teste `reconnects_after_backend_is_killed_through_tunnel`, Tarefa 3).
4. Editar a conexão ativa com edições pendentes — recusado sem perder nada (guarda em `save_connection`, Tarefa 5; checklist manual).
5. Cancelar (`⌘.`) uma query que passa pelo túnel — o cancelamento precisa de canal próprio (teste `cancel_through_tunnel`, Tarefa 3).

---

### Task 1: Modelo SSH e resolução do `~/.ssh/config`

**Files:**
- Modify: `crates/rb-core/Cargo.toml`
- Modify: `crates/rb-core/src/error.rs`
- Modify: `crates/rb-core/src/config.rs`
- Modify: `crates/rb-core/src/lib.rs`
- Create: `crates/rb-core/src/ssh.rs`
- Modify: `crates/rb-core/tests/common/mod.rs:14-22` (campo novo `ssh`)
- Modify: `crates/rb-app/src/conn_dialog.rs:32-40` (campo novo `ssh: None`, para compilar)

**Interfaces:**
- Produces:
  - `rb_core::config::SshConfig { host: String, user: Option<String>, port: Option<u16>, identity_file: Option<String> }` (`Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`)
  - `ConnectionConfig.ssh: Option<SshConfig>`
  - `DbError::Ssh(String)`, `DbError::HostKeyUnknown { host: String, port: u16, fingerprint: String, key: String }`
  - `rb_core::ssh::SshEnv { home, config_file, known_hosts: PathBuf, agent: Option<PathBuf>, user: String }`, `SshEnv::system()`
  - `rb_core::ssh::SshTarget { hostname: String, port: u16, user: String, identity_files: Vec<PathBuf>, identities_explicit: bool, agent: Option<PathBuf>, known_hosts: PathBuf }`
  - `SshConfig::resolve(&self, env: &SshEnv) -> Result<SshTarget, DbError>`

- [ ] **Step 1: Dependências**

Em `crates/rb-core/Cargo.toml`, `[dependencies]`:

```toml
russh = "0.64.1"
ssh2-config = "0.8.2"
tokio = { version = "1.53", features = ["rt-multi-thread", "sync", "time", "macros", "net"] }
```

(a linha do `tokio` substitui a atual; só acrescenta `net`).

Run: `cargo build -p rb-core 2>&1 | tail -3`
Expected: `Finished`.

- [ ] **Step 2: Variantes de erro**

Em `crates/rb-core/src/error.rs`, no `enum DbError`, depois de `Unsupported(String),`:

```rust
    /// Falha no túnel SSH (conexão, autenticação, canal, known_hosts).
    Ssh(String),
    /// Host SSH fora do known_hosts. `key` é a chave pública em formato OpenSSH, para `ssh::trust_host`.
    HostKeyUnknown { host: String, port: u16, fingerprint: String, key: String },
```

No `Display`, depois do braço `Unsupported`:

```rust
            DbError::Ssh(m) => write!(f, "SSH: {m}"),
            DbError::HostKeyUnknown { host, port, fingerprint, .. } => {
                write!(f, "SSH: host {host}:{port} não está no known_hosts ({fingerprint})")
            }
```

- [ ] **Step 3: Escrever os testes que falham**

Em `crates/rb-core/src/config.rs`, `mod tests`: em `sample()` acrescente `ssh: None,` e adicione:

```rust
    #[test]
    fn ssh_block_round_trips_and_old_files_still_load() {
        let path = tmp("ssh");
        let mut c = sample();
        c.ssh = Some(SshConfig {
            host: "prod-bastion".into(),
            user: Some("deploy".into()),
            port: None,
            identity_file: Some("~/.ssh/id_ed25519".into()),
        });
        let cfg = ConfigFile { connections: vec![c] };
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[connection.ssh]") && text.contains("host = \"prod-bastion\""), "{text}");
        assert_eq!(load(&path).unwrap(), cfg);

        std::fs::write(&path, "[[connection]]\nid = \"a\"\nname = \"n\"\nhost = \"h\"\nport = 5432\ndatabase = \"d\"\nuser = \"u\"\n").unwrap();
        assert_eq!(load(&path).unwrap().connections[0].ssh, None);
    }
```

Crie `crates/rb-core/src/ssh.rs` só com os testes (o código vem no Step 5):

```rust
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
```

Em `crates/rb-core/src/lib.rs` acrescente `pub mod ssh;` (em ordem alfabética, depois de `split`).

- [ ] **Step 4: Rodar e ver falhar**

Run: `cargo test -p rb-core --lib 2>&1 | grep -E "^error" -A3 | head -20`
Expected: erros de compilação `cannot find type SshConfig`, `SshEnv`, `ssh` em `ConnectionConfig`.

- [ ] **Step 5: Implementar**

Em `crates/rb-core/src/config.rs`, acima de `ConnectionConfig`:

```rust
/// Túnel SSH de uma conexão. Campos vazios vêm do `~/.ssh/config` ou dos padrões do OpenSSH.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshConfig {
    /// Alias do `~/.ssh/config` ou hostname.
    pub host: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub identity_file: Option<String>,
}
```

E no fim de `ConnectionConfig`:

```rust
    /// Com túnel, `host`/`port` são resolvidos pelo bastion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
```

Em `crates/rb-core/tests/common/mod.rs` e `crates/rb-app/src/conn_dialog.rs`, nos literais `ConnectionConfig { .. }`, acrescente `ssh: None,`.

Em `crates/rb-core/src/ssh.rs`, acima do `mod tests`:

```rust
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
```

- [ ] **Step 6: Rodar e ver passar**

Run: `cargo test -p rb-core --lib 2>&1 | tail -4`
Expected: `test result: ok.` sem falhas (os 5 testes de `ssh::tests` e o novo de `config::tests` incluídos).

Run: `cargo build --workspace 2>&1 | tail -2`
Expected: `Finished` (o app compila com `ssh: None`).

- [ ] **Step 7: Commit**

```bash
git add crates/rb-core crates/rb-app/src/conn_dialog.rs
git commit -m "feat(core): modelo SSH e resolução do ~/.ssh/config

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: `Tunnel` — autenticação, known_hosts e canais

**Files:**
- Modify: `crates/rb-core/src/ssh.rs`
- Create: `scripts/test-sshd.sh`, `scripts/sshd.Dockerfile`
- Create: `crates/rb-core/tests/ssh.rs`

**Interfaces:**
- Consumes: `SshTarget`, `DbError::{Ssh, HostKeyUnknown}` (Tarefa 1).
- Produces:
  - `rb_core::ssh::Tunnel::connect(target: SshTarget, passphrase: Option<String>) -> Result<Tunnel, DbError>` (async)
  - `Tunnel::open_stream(&self, host: &str, port: u16) -> Result<russh::ChannelStream<russh::client::Msg>, DbError>` (async; reabre o SSH se caiu)
  - `rb_core::ssh::trust_host(known_hosts: &Path, host: &str, port: u16, key: &str) -> Result<(), DbError>`
  - `impl From<russh::Error> for DbError`

- [ ] **Step 1: Ambiente de teste**

`scripts/sshd.Dockerfile`:

```dockerfile
FROM alpine:3.20
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh rb \
 && echo 'rb:*' | chpasswd -e \
 && mkdir -p /home/rb/.ssh \
 && sed -i '/^AllowTcpForwarding/d;/^PasswordAuthentication/d;/^StrictModes/d' /etc/ssh/sshd_config \
 && printf 'AllowTcpForwarding yes\nPasswordAuthentication no\nStrictModes no\n' >> /etc/ssh/sshd_config
CMD ["/usr/sbin/sshd", "-D", "-e"]
```

`scripts/test-sshd.sh` (com `chmod +x`):

```bash
#!/usr/bin/env bash
# Sobe o bastion de teste (rb-sshd) na mesma rede do rb-pg. Chaves de teste ficam em target/test-ssh.
set -euo pipefail
cd "$(dirname "$0")/.."
D=target/test-ssh
mkdir -p "$D"
[ -f "$D/id_plain" ] || ssh-keygen -q -t ed25519 -N '' -C rb-plain -f "$D/id_plain"
[ -f "$D/id_pass" ] || ssh-keygen -q -t ed25519 -N 'segredo' -C rb-pass -f "$D/id_pass"
[ -f "$D/id_agent" ] || ssh-keygen -q -t ed25519 -N '' -C rb-agent -f "$D/id_agent"
cat "$D/id_plain.pub" "$D/id_pass.pub" "$D/id_agent.pub" > "$D/authorized_keys"
docker network inspect rb-net >/dev/null 2>&1 || docker network create rb-net >/dev/null
docker network connect rb-net rb-pg 2>/dev/null || true
docker build -q -t rb-sshd -f scripts/sshd.Dockerfile scripts >/dev/null
docker rm -f rb-sshd >/dev/null 2>&1 || true
docker run -d --name rb-sshd --network rb-net -p 127.0.0.1:52222:22 \
  -v "$PWD/$D/authorized_keys:/home/rb/.ssh/authorized_keys:ro" rb-sshd >/dev/null
for _ in $(seq 50); do nc -z 127.0.0.1 52222 && break; sleep 0.1; done
echo "export RB_TEST_SSH=127.0.0.1:52222"
```

Run: `scripts/test-sshd.sh && ssh -i target/test-ssh/id_plain -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p 52222 rb@127.0.0.1 'nc -z rb-pg 5432 && echo ok'`
Expected: `export RB_TEST_SSH=127.0.0.1:52222` e `ok` (o bastion alcança `rb-pg`). Se o `nc` do Alpine não existir, troque o comando remoto por `getent hosts rb-pg` (esperado: uma linha com IP).

- [ ] **Step 2: Escrever os testes que falham**

`crates/rb-core/tests/ssh.rs`:

```rust
//! Túnel SSH contra o container rb-sshd. Suba com `scripts/test-sshd.sh` e exporte RB_TEST_SSH.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

use rb_core::DbError;
use rb_core::ssh::{SshTarget, Tunnel, trust_host};

struct Env {
    host: String,
    port: u16,
    dir: PathBuf,
}

fn env() -> Option<Env> {
    let addr = std::env::var("RB_TEST_SSH").ok()?;
    let (host, port) = addr.rsplit_once(':').expect("RB_TEST_SSH = host:porta");
    Some(Env {
        host: host.into(),
        port: port.parse().expect("porta"),
        dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-ssh"),
    })
}

fn skip() {
    eprintln!("RB_TEST_SSH ausente; teste de túnel pulado");
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("rb-ssh-{}-{name}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run<F: Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

fn target(e: &Env, key: &str, known_hosts: PathBuf, agent: Option<PathBuf>) -> SshTarget {
    SshTarget {
        hostname: e.host.clone(),
        port: e.port,
        user: "rb".into(),
        identity_files: vec![e.dir.join(key)],
        identities_explicit: true,
        agent,
        known_hosts,
    }
}

/// known_hosts temporário com a chave do rb-sshd, aprendida pelo fluxo de confiança.
fn trusted_known_hosts(e: &Env) -> PathBuf {
    let kh = scratch("kh").join("known_hosts");
    match run(Tunnel::connect(target(e, "id_plain", kh.clone(), None), None)).err() {
        Some(DbError::HostKeyUnknown { host, port, key, .. }) => trust_host(&kh, &host, port, &key).unwrap(),
        other => panic!("esperava HostKeyUnknown: {other:?}"),
    }
    kh
}

fn ssh_err(res: Option<DbError>, needle: &str) {
    assert!(matches!(&res, Some(DbError::Ssh(m)) if m.contains(needle)), "esperava SSH com {needle:?}: {res:?}");
}

#[test]
fn unreachable_bastion() {
    let t = SshTarget {
        hostname: "127.0.0.1".into(),
        port: 1,
        user: "x".into(),
        identity_files: vec![],
        identities_explicit: false,
        agent: None,
        known_hosts: scratch("unreach").join("known_hosts"),
    };
    ssh_err(run(Tunnel::connect(t, None)).err(), "bastion 127.0.0.1:1 inacessível");
}

#[test]
fn unknown_host_is_reported_then_trusted() {
    let Some(e) = env() else { return skip() };
    let kh = scratch("unknown").join("known_hosts");
    let err = run(Tunnel::connect(target(&e, "id_plain", kh.clone(), None), None)).err();
    let Some(DbError::HostKeyUnknown { host, port, fingerprint, key }) = err else { panic!("{err:?}") };
    assert_eq!((host.as_str(), port), (e.host.as_str(), e.port));
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
    trust_host(&kh, &host, port, &key).unwrap();
    assert!(std::fs::read_to_string(&kh).unwrap().starts_with(&format!("[{}]:{} ", e.host, e.port)));
    assert!(run(Tunnel::connect(target(&e, "id_plain", kh, None), None)).is_ok());
}

#[test]
fn changed_host_key_is_refused_without_touching_known_hosts() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    let real = std::fs::read_to_string(&kh).unwrap();
    let fake = std::fs::read_to_string(e.dir.join("id_plain.pub")).unwrap();
    let mut parts: Vec<String> = real.trim_end().splitn(3, ' ').map(String::from).collect();
    assert_eq!(parts[1], "ssh-ed25519", "o teste supõe chave de host ed25519");
    parts[2] = fake.split_whitespace().nth(1).unwrap().to_string();
    let tampered = format!("{}\n", parts.join(" "));
    std::fs::write(&kh, &tampered).unwrap();
    ssh_err(run(Tunnel::connect(target(&e, "id_plain", kh.clone(), None), None)).err(), "mudou");
    assert_eq!(std::fs::read_to_string(&kh).unwrap(), tampered);
}

#[test]
fn key_file_with_passphrase() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    let t = || target(&e, "id_pass", kh.clone(), None);
    ssh_err(run(Tunnel::connect(t(), None)).err(), "exige passphrase");
    ssh_err(run(Tunnel::connect(t(), Some("errada".into()))).err(), "passphrase incorreta");
    assert!(run(Tunnel::connect(t(), Some("segredo".into()))).is_ok());
}

#[test]
fn rejected_user_lists_what_was_tried() {
    let Some(e) = env() else { return skip() };
    let mut t = target(&e, "id_plain", trusted_known_hosts(&e), None);
    t.user = "ninguem".into();
    ssh_err(run(Tunnel::connect(t, None)).err(), "nenhuma chave aceita por ninguem@");
}

#[test]
fn agent_only_authentication() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    // Caminho curto: sockets Unix no macOS têm limite de ~104 bytes.
    let sock = PathBuf::from(format!("/tmp/rb-agent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let mut agent = std::process::Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let added = std::process::Command::new("ssh-add")
        .arg(e.dir.join("id_agent"))
        .env("SSH_AUTH_SOCK", &sock)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let mut t = target(&e, "id_agent", kh, Some(sock.clone()));
    t.identity_files.clear();
    t.identities_explicit = false;
    let res = run(Tunnel::connect(t, None)).err();
    agent.kill().ok();
    let _ = std::fs::remove_file(&sock);
    assert!(added.success());
    assert_eq!(res, None);
}

#[test]
fn channels_to_open_and_closed_ports() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    run(async {
        let t = Tunnel::connect(target(&e, "id_plain", kh, None), None).await.unwrap();
        assert!(t.open_stream("rb-pg", 5432).await.is_ok());
        ssh_err(t.open_stream("rb-pg", 9).await.err(), "recusou abrir canal para rb-pg:9");
    });
}
```

- [ ] **Step 3: Rodar e ver falhar**

Run: `RB_TEST_SSH=127.0.0.1:52222 cargo test -p rb-core --test ssh 2>&1 | grep -E "^error" -A3 | head`
Expected: `unresolved imports rb_core::ssh::Tunnel, rb_core::ssh::trust_host`.

- [ ] **Step 4: Implementar**

Em `crates/rb-core/src/ssh.rs`, troque o bloco de `use` do topo por:

```rust
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
```

E acrescente, antes do `mod tests`:

```rust
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
```

Nota: `key.public_key()` em `hash_for(key.public_key())` devolve `&PublicKey` de `ssh_key::PrivateKey`. Se o compilador apontar tipo diferente, use `hash_for(&key.public_key().clone())`.

- [ ] **Step 5: Rodar e ver passar**

Run: `RB_TEST_SSH=127.0.0.1:52222 cargo test -p rb-core --test ssh 2>&1 | tail -4`
Expected: `test result: ok. 7 passed`.

Se `changed_host_key_is_refused…` falhar na asserção `ssh-ed25519`, o russh negociou outro algoritmo: registre a ruling e gere a chave falsa com `ssh-keygen -t <algoritmo>` no script em vez de reusar `id_plain.pub`.

Run: `cargo test -p rb-core --test ssh 2>&1 | tail -3` (sem a variável)
Expected: `ok`, com os testes de container pulados e `unreachable_bastion` passando.

- [ ] **Step 6: Commit**

```bash
git add crates/rb-core scripts/test-sshd.sh scripts/sshd.Dockerfile
git commit -m "feat(core): túnel SSH com russh (agente, chave, known_hosts)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Sessão sobre o túnel

**Files:**
- Modify: `crates/rb-core/src/session.rs`
- Modify: `crates/rb-core/tests/common/mod.rs:25`
- Modify: `crates/rb-core/tests/ssh.rs`
- Modify: `crates/rb-app/src/meta.rs:10-13`, `crates/rb-app/src/workspace.rs:96-103`, `crates/rb-app/src/sql_tab.rs:112`, `crates/rb-app/src/table_tab.rs:131`
- Modify: `docs/perf.md`

**Interfaces:**
- Consumes: `Tunnel::connect`, `Tunnel::open_stream`, `SshConfig::resolve`, `SshEnv::system` (Tarefas 1–2).
- Produces:
  - `rb_core::session::Credentials { password: Option<String>, ssh_passphrase: Option<String> }` (`Debug, Clone, Default, PartialEq, Eq`)
  - `Engine::connect(&self, cfg: &ConnectionConfig, creds: Credentials) -> Result<Session, DbError>` (assinatura nova)
  - `Engine::with_ssh_env(self, env: SshEnv) -> Engine`
  - `rb_app::meta::ConnTarget { cfg: ConnectionConfig, creds: Credentials }` (substitui `password`)

- [ ] **Step 1: Escrever os testes que falham**

No topo de `crates/rb-core/tests/ssh.rs`, acrescente aos `use`:

```rust
use pollster::block_on;
use rb_core::config::{ConnectionConfig, SshConfig, SslMode};
use rb_core::session::{Credentials, Engine};
use rb_core::ssh::SshEnv;
```

E no fim do arquivo:

```rust
/// Engine que só enxerga o home temporário; destino `rb-pg:5432` só resolve dentro do bastion.
fn tunneled(e: &Env) -> (Engine, ConnectionConfig) {
    let home = scratch("home");
    let env = SshEnv {
        config_file: home.join("nenhum"),
        known_hosts: trusted_known_hosts(e),
        agent: None,
        user: "rb".into(),
        home,
    };
    let cfg = ConnectionConfig {
        id: "ssh-test".into(),
        name: "ssh".into(),
        host: "rb-pg".into(),
        port: 5432,
        database: "postgres".into(),
        user: "postgres".into(),
        ssl: SslMode::Disable,
        ssh: Some(SshConfig {
            host: e.host.clone(),
            user: Some("rb".into()),
            port: Some(e.port),
            identity_file: Some(e.dir.join("id_plain").display().to_string()),
        }),
    };
    (Engine::new().unwrap().with_ssh_env(env), cfg)
}

#[test]
fn queries_go_through_the_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
    let s2 = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert_eq!(block_on(s2.execute("select 2".into())).unwrap().batch.rows(), 1);
}

#[test]
fn reconnects_after_backend_is_killed_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert!(block_on(s.execute("select pg_terminate_backend(pg_backend_pid())".into())).is_err());
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
}

#[test]
fn cancel_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    let started = std::time::Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            s.cancel();
        });
        block_on(s.execute("select pg_sleep(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Postgres { ref code, .. }) if code == "57014"), "{res:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// `cargo test --release -p rb-core --test ssh -- --ignored --nocapture` (precisa do seed de perf no rb-pg).
#[test]
#[ignore]
fn perf_big_first_batch_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    let out = block_on(s.execute("select * from perf_big".into())).unwrap();
    println!("primeiro lote de perf_big pelo túnel: {} ms", out.elapsed.as_millis());
    assert!(out.elapsed.as_millis() < 200);
}
```

Em `crates/rb-core/tests/common/mod.rs`, troque a linha do `connect` por:

```rust
    let creds = rb_core::session::Credentials { password, ..Default::default() };
    let session = pollster::block_on(engine.connect(&cfg, creds)).expect("conectar ao Postgres de teste");
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `RB_TEST_SSH=127.0.0.1:52222 cargo test -p rb-core --test ssh 2>&1 | grep -E "^error" -A3 | head`
Expected: `unresolved import rb_core::session::Credentials` e `no method named with_ssh_env`.

- [ ] **Step 3: Implementar**

Em `crates/rb-core/src/session.rs`:

Imports (substituem os de `std::sync` e acrescentam os novos):

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use russh::ChannelStream;
use russh::client::Msg;
use tokio_postgres::tls::MakeTlsConnect;

use crate::config::{ConnectionConfig, SshConfig};
use crate::ssh::{SshEnv, Tunnel};
```

Depois de `type Reply<T> = …;`:

```rust
/// Segredos de uma conexão, lidos do Keychain pelo app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub password: Option<String>,
    pub ssh_passphrase: Option<String>,
}

/// Um túnel por conexão do app (id + bloco SSH), compartilhado pelas sessões dela.
type TunnelCache = Mutex<HashMap<(String, SshConfig), Weak<Tunnel>>>;
```

`Engine` passa a ser:

```rust
#[derive(Clone)]
pub struct Engine {
    rt: Arc<Runtime>,
    ssh_env: Arc<SshEnv>,
    tunnels: Arc<TunnelCache>,
}
```

Em `Engine::new`, troque `Ok(Engine { rt: Arc::new(rt) })` por:

```rust
        Ok(Engine { rt: Arc::new(rt), ssh_env: Arc::new(SshEnv::system()), tunnels: Arc::default() })
```

Substitua `Engine::connect` por:

```rust
    /// Troca de onde vem a configuração SSH (testes).
    pub fn with_ssh_env(mut self, env: SshEnv) -> Engine {
        self.ssh_env = Arc::new(env);
        self
    }

    pub async fn connect(&self, cfg: &ConnectionConfig, creds: Credentials) -> Result<Session, DbError> {
        let pg = cfg.pg_config(creds.password.as_deref());
        let route = cfg.ssh.clone().map(|ssh| (cfg.id.clone(), ssh, creds.ssh_passphrase, cfg.host.clone(), cfg.port));
        let (env, tunnels) = (self.ssh_env.clone(), self.tunnels.clone());
        let handle = self.rt.handle().clone();
        self.rt
            .spawn(async move {
                let tunnel = match route {
                    None => None,
                    Some((id, ssh, passphrase, host, port)) => {
                        Some((shared_tunnel(&tunnels, id, ssh, passphrase, &env).await?, host, port))
                    }
                };
                Session::open(Connector { pg, tunnel }, handle).await
            })
            .await
            .map_err(|e| DbError::Connection(e.to_string()))?
    }
```

Depois do `impl Engine`:

```rust
/// O túnel cai quando a última sessão que o usa cai (o cache guarda só `Weak`).
/// ponytail: duas sessões abrindo ao mesmo tempo podem criar dois túneis; ambos funcionam
/// e o segundo fica no cache.
async fn shared_tunnel(
    cache: &TunnelCache,
    id: String,
    ssh: SshConfig,
    passphrase: Option<String>,
    env: &SshEnv,
) -> Result<Arc<Tunnel>, DbError> {
    let key = (id, ssh);
    if let Some(t) = cache.lock().expect("cache de túneis").get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let target = key.1.resolve(env)?;
    let tunnel = Arc::new(Tunnel::connect(target, passphrase).await?);
    cache.lock().expect("cache de túneis").insert(key, Arc::downgrade(&tunnel));
    Ok(tunnel)
}

/// Como abrir um cliente Postgres: direto, ou por um canal do túnel SSH (sem porta local).
#[derive(Clone)]
struct Connector {
    pg: tokio_postgres::Config,
    /// Túnel e destino (`host`, `port`) vistos pelo bastion.
    tunnel: Option<(Arc<Tunnel>, String, u16)>,
}

type ChannelTls = <MakeRustlsConnect as MakeTlsConnect<ChannelStream<Msg>>>::TlsConnect;

fn channel_tls(host: &str) -> Result<ChannelTls, DbError> {
    MakeTlsConnect::<ChannelStream<Msg>>::make_tls_connect(&mut tls(), host).map_err(|e| DbError::Connection(e.to_string()))
}

impl Connector {
    async fn client(&self) -> Result<Client, DbError> {
        let Some((tunnel, host, port)) = &self.tunnel else { return connect_client(&self.pg).await };
        let stream = tunnel.open_stream(host, *port).await?;
        let (client, conn) = self.pg.connect_raw(stream, channel_tls(host)?).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("conexão encerrada: {e}");
            }
        });
        Ok(client)
    }

    async fn cancel(&self, token: CancelToken) -> Result<(), DbError> {
        match &self.tunnel {
            None => token.cancel_query(tls()).await?,
            Some((tunnel, host, port)) => token.cancel_query_raw(tunnel.open_stream(host, *port).await?, channel_tls(host)?).await?,
        }
        Ok(())
    }
}
```

`Session` ganha o campo `connector: Connector`; `Session::open` e `cancel` ficam:

```rust
    async fn open(connector: Connector, rt: Handle) -> Result<Session, DbError> {
        let started = Instant::now();
        let client = connector.client().await?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "conectado");
        let cancel = Arc::new(Mutex::new(client.cancel_token()));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Actor { client, connector: connector.clone(), cancel: cancel.clone(), last_exec: 0 }.run(rx));
        Ok(Session { tx, cancel, rt, connector })
    }
```

```rust
    /// Pede ao servidor para cancelar o comando em andamento desta sessão.
    pub fn cancel(&self) {
        let token = self.cancel.lock().expect("mutex do cancel token").clone();
        let connector = self.connector.clone();
        self.rt.spawn(async move {
            if let Err(e) = connector.cancel(token).await {
                tracing::warn!("cancelamento falhou: {e}");
            }
        });
    }
```

Em `struct Actor`, troque `pg: tokio_postgres::Config,` por `connector: Connector,`; em `ensure_connected`, troque `connect_client(&self.pg)` por `self.connector.client()`.

No app — `crates/rb-app/src/meta.rs`:

```rust
use rb_core::session::{Credentials, Session};

#[derive(Clone)]
pub struct ConnTarget {
    pub cfg: ConnectionConfig,
    pub creds: Credentials,
}
```

`crates/rb-app/src/sql_tab.rs:112` e `crates/rb-app/src/table_tab.rs:131`: `engine.connect(&conn.cfg, conn.creds.clone())`.

`crates/rb-app/src/workspace.rs`, dentro de `connect`, substitua as duas linhas de `password`/`session` e o `Ok((ConnTarget { cfg, password }, session))` por:

```rust
                let has_ssh = cfg.ssh.is_some();
                let creds = cx
                    .background_spawn(async move {
                        Ok::<_, rb_core::DbError>(Credentials {
                            password: secrets::get_password(&id)?,
                            ssh_passphrase: if has_ssh { secrets::get_password(&secrets::ssh_account(&id))? } else { None },
                        })
                    })
                    .await?;
                let session = engine.connect(&cfg, creds.clone()).await?;
                Ok::<_, rb_core::DbError>((ConnTarget { cfg, creds }, session))
```

com `use rb_core::session::Credentials;` no topo. Em `crates/rb-core/src/secrets.rs`, depois de `const NOT_FOUND`:

```rust
/// Conta do Keychain que guarda a passphrase da chave SSH de uma conexão.
pub fn ssh_account(id: &str) -> String {
    format!("ssh:{id}")
}
```

- [ ] **Step 4: Rodar e ver passar**

Run: `RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace 2>&1 | grep "test result" `
Expected: todas as linhas `ok`, 0 failed (os 78 anteriores + 5 de `ssh::tests` + 1 de `config` + 10 de `tests/ssh.rs`, com 1 ignorado a mais).

- [ ] **Step 5: Medir a performance pelo túnel**

Run: `RB_TEST_SSH=127.0.0.1:52222 cargo test --release -p rb-core --test ssh -- --ignored --nocapture 2>&1 | grep "primeiro lote"`
Expected: `primeiro lote de perf_big pelo túnel: N ms` com N < 200.

Acrescente a `docs/perf.md`, na tabela de resultados, uma linha: `| Primeiro lote de perf_big pelo túnel SSH (local) | N ms | < 200 ms |`.

- [ ] **Step 6: Commit**

```bash
git add crates docs/perf.md
git commit -m "feat(core): sessões Postgres sobre o túnel SSH, compartilhado por conexão

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Confiança no host e limpeza de segredos

**Files:**
- Modify: `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `DbError::HostKeyUnknown`, `rb_core::ssh::{trust_host, SshEnv}`, `secrets::ssh_account` (Tarefas 1–3).
- Produces: `Workspace::disconnect(&mut self, cx: &mut Context<Self>)` (usado pela Tarefa 5).

UI sem teste automatizado (padrão do projeto); a verificação é compilar e o checklist manual da Tarefa 5.

- [ ] **Step 1: Extrair `disconnect` e apagar a passphrase ao remover**

Em `remove_connection`, troque o bloco interno do `if self.active…` (de `self.save_all(cx);` até `self.active = None;`) por `self.disconnect(cx);` e acrescente o método:

```rust
    /// Salva os scripts, fecha as abas e solta a sessão (e o túnel, se for o último uso).
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.save_all(cx);
        self.tabs.clear();
        self.active_tab = 0;
        self.active = None;
        cx.notify();
    }
```

No `background_spawn` de `remove_connection`, troque `secrets::delete_password(&id)` por:

```rust
                    secrets::delete_password(&id)?;
                    secrets::delete_password(&secrets::ssh_account(&id))
```

- [ ] **Step 2: Diálogo de confiança**

Em `connect`, troque o braço de erro:

```rust
                    Err(rb_core::DbError::HostKeyUnknown { host, port, fingerprint, key }) => {
                        ws.confirm_trust(ix, host, port, fingerprint, key, window, cx)
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Falha ao conectar: {e}")), cx),
```

E acrescente:

```rust
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
```

com `use rb_core::ssh::{self, SshEnv};` no topo.

- [ ] **Step 3: Compilar**

Run: `cargo build -p rb-app 2>&1 | grep -E "^(warning|error)" -A5 | head -20; cargo build -p rb-app 2>&1 | tail -1`
Expected: sem erros nem warnings; `Finished`.

- [ ] **Step 4: Commit**

```bash
git add crates/rb-app/src/workspace.rs
git commit -m "feat(app): confiança em host SSH novo e remoção da passphrase

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Diálogo com SSH e edição de conexão

**Files:**
- Modify: `crates/rb-app/src/conn_dialog.rs`
- Modify: `crates/rb-app/src/workspace.rs` (`add_connection` → `save_connection`, botão ✎)
- Modify: `docs/manual-checklist.md`

**Interfaces:**
- Consumes: `SshConfig`, `SshConfig::resolve`, `SshEnv::system`, `secrets::ssh_account`, `Workspace::disconnect` (Tarefas 1–4).
- Produces: `conn_dialog::open(existing: Option<ConnectionConfig>, window, cx)`, `Workspace::save_connection(cfg, password, passphrase, window, cx) -> bool`.

- [ ] **Step 1: Escrever o teste que falha**

No fim de `crates/rb-app/src/conn_dialog.rs`:

```rust
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
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-app conn_dialog 2>&1 | grep -E "^error" -A3 | head`
Expected: `cannot find function parse_ssh`.

- [ ] **Step 3: Implementar o diálogo**

Substitua `crates/rb-app/src/conn_dialog.rs` (mantendo o `mod tests` do Step 1 no fim) por:

```rust
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
    show_resolved(&f.ssh_host.read(cx).value(), &u, &p, &k, window, cx);
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
```

Se `set_placeholder` não aceitar `String` por causa do `impl Into<SharedString>`, ele aceita: `String: Into<SharedString>`. Se `state.read(cx).value()` devolver `SharedString`, `.trim()` funciona via `Deref<Target = str>`.

- [ ] **Step 4: `save_connection` e botão ✎**

Em `crates/rb-app/src/workspace.rs`, substitua `add_connection` por:

```rust
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
                        if !has_ssh {
                            secrets::delete_password(&secrets::ssh_account(&id))?;
                        } else if !passphrase.is_empty() {
                            secrets::set_password(&secrets::ssh_account(&id), &passphrase)?;
                        }
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
```

Em `render_sidebar`, entre o botão da conexão e o `×`:

```rust
                    .child(Button::new(("edit-conn", ix)).label("✎").ghost().on_click(cx.listener(move |ws, _, window, cx| {
                        let c = ws.connections[ix].clone();
                        conn_dialog::open(Some(c), window, cx)
                    })))
```

e o botão `+` passa a chamar `conn_dialog::open(None, window, cx)`.

- [ ] **Step 5: Rodar e ver passar**

Run: `cargo test -p rb-app 2>&1 | grep "test result"; cargo build -p rb-app 2>&1 | grep -cE "^(warning|error)"`
Expected: `test result: ok. 4 passed` (2 de `grid` + 2 novos) e `0`.

- [ ] **Step 6: Checklist manual**

Acrescente a `docs/manual-checklist.md`:

```markdown
## Túnel SSH e edição de conexão

- [ ] "+" mostra a seção SSH; com host SSH vazio a conexão funciona como antes.
- [ ] Digitar um alias do ~/.ssh/config e sair do campo mostra usuário, porta e chave resolvidos como placeholder.
- [ ] Conexão com túnel para um host fora do known_hosts abre "Host SSH desconhecido" com o fingerprint; "Confiar" conecta e grava a linha no ~/.ssh/known_hosts.
- [ ] Chave com passphrase: sem passphrase o erro diz "exige passphrase"; com ela, conecta; reabrir o app conecta sem pedir de novo (Keychain).
- [ ] ✎ abre o diálogo preenchido; salvar com senha em branco mantém a senha (reconectar funciona).
- [ ] ✎ na conexão ativa com edição pendente numa aba de tabela: recusa com aviso e nada muda.
- [ ] ✎ na conexão ativa sem pendências: salva, reabre os scripts e reconecta.
- [ ] Apagar o host SSH de uma conexão e salvar: passphrase some do Keychain (Acesso às Chaves, conta ssh:<id>).
- [ ] ⌘. cancela uma query longa que passa pelo túnel.
- [ ] Bastion real do autor: árvore, autocomplete, grade e edição funcionam.
```

- [ ] **Step 7: Suite completa e commit**

Run: `scripts/test-sshd.sh >/dev/null && RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace 2>&1 | grep "test result"`
Expected: todas `ok`, 0 failed.

```bash
git add crates/rb-app docs/manual-checklist.md
git commit -m "feat(app): seção SSH no diálogo e edição de conexão (✎)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
