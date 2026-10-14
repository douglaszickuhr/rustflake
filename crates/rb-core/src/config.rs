use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    #[default]
    Disable,
    Prefer,
    /// TLS obrigatório, com certificado verificado pelas raízes do sistema.
    Require,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl: SslMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default, rename = "connection")]
    pub connections: Vec<ConnectionConfig>,
}

impl ConnectionConfig {
    pub fn pg_config(&self, password: Option<&str>) -> tokio_postgres::Config {
        use tokio_postgres::config::SslMode as Pg;
        let mut c = tokio_postgres::Config::new();
        c.host(&self.host)
            .port(self.port)
            .dbname(&self.database)
            .user(&self.user)
            .application_name("rustbeaver")
            .connect_timeout(Duration::from_secs(10))
            .ssl_mode(match self.ssl {
                SslMode::Disable => Pg::Disable,
                SslMode::Prefer => Pg::Prefer,
                SslMode::Require => Pg::Require,
            });
        if let Some(p) = password {
            c.password(p);
        }
        c
    }
}

pub fn app_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/rustbeaver")
}

pub fn connections_path() -> PathBuf {
    app_dir().join("connections.toml")
}

pub fn scripts_dir() -> PathBuf {
    app_dir().join("scripts")
}

/// Cria `script-N.sql` vazio com o menor N livre. `create_new` garante que duas abas
/// nunca compartilhem o mesmo arquivo.
pub fn new_script(dir: &Path) -> Result<PathBuf, DbError> {
    std::fs::create_dir_all(dir).map_err(io)?;
    for n in 1.. {
        let path = dir.join(format!("script-{n}.sql"));
        match std::fs::File::create_new(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        }
    }
    unreachable!("1.. é infinito")
}

/// Move o script para `closed/` ao lado dele, sem sobrescrever nada que já esteja lá.
pub fn archive_script(path: &Path) -> Result<PathBuf, DbError> {
    let closed = path.parent().unwrap_or(Path::new(".")).join("closed");
    std::fs::create_dir_all(&closed).map_err(io)?;
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let mut target = closed.join(path.file_name().unwrap_or_default());
    for n in 2.. {
        if !target.exists() {
            break;
        }
        target = closed.join(format!("{stem}-{n}.sql"));
    }
    std::fs::rename(path, &target).map_err(io)?;
    Ok(target)
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("c{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn io(e: std::io::Error) -> DbError {
    DbError::Io(e.to_string())
}

pub fn load(path: &Path) -> Result<ConfigFile, DbError> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| DbError::Config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(io(e)),
    }
}

/// Grava de forma atômica (arquivo temporário + rename).
pub fn save(path: &Path, cfg: &ConfigFile) -> Result<(), DbError> {
    let text = toml::to_string_pretty(cfg).map_err(|e| DbError::Config(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rb-config-{}-{name}", std::process::id())).join("connections.toml")
    }

    fn sample() -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(),
            name: "local".into(),
            host: "localhost".into(),
            port: 5432,
            database: "app".into(),
            user: "postgres".into(),
            ssl: SslMode::Require,
        }
    }

    #[test]
    fn missing_file_is_empty_config() {
        assert_eq!(load(&tmp("missing")).unwrap(), ConfigFile::default());
    }

    #[test]
    fn save_then_load_round_trips_without_password() {
        let path = tmp("roundtrip");
        let cfg = ConfigFile { connections: vec![sample()] };
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[connection]]") && text.contains("ssl = \"require\""));
        assert!(!text.contains("password"));
        assert_eq!(load(&path).unwrap(), cfg);
    }

    #[test]
    fn invalid_toml_is_config_error() {
        let path = tmp("invalid");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[connection]]\nport = \"x\"").unwrap();
        assert!(matches!(load(&path), Err(DbError::Config(_))));
    }

    #[test]
    fn pg_config_carries_fields() {
        let pg = sample().pg_config(Some("pw"));
        assert_eq!(pg.get_user(), Some("postgres"));
        assert_eq!(pg.get_dbname(), Some("app"));
        assert_eq!(pg.get_ports(), &[5432]);
        assert_eq!(pg.get_password(), Some(&b"pw"[..]));
    }

    #[test]
    fn new_scripts_never_reuse_a_path() {
        let dir = tmp("scripts").parent().unwrap().join("scripts");
        let _ = std::fs::remove_dir_all(&dir);
        let a = new_script(&dir).unwrap();
        let b = new_script(&dir).unwrap();
        assert_ne!(a, b);
        assert!(a.exists() && b.exists());
    }

    #[test]
    fn archiving_never_overwrites() {
        let dir = tmp("archive").parent().unwrap().join("scripts");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("script-1.sql");
        std::fs::write(&p, "primeiro").unwrap();
        let first = archive_script(&p).unwrap();
        std::fs::write(&p, "segundo").unwrap();
        let second = archive_script(&p).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "primeiro");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "segundo");
        assert!(!p.exists());
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
