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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnowflakeAuth {
    #[default]
    ExternalBrowser,
    KeyPair,
    Pat,
}

/// Conexão Snowflake. `ConnectionConfig.user` e `.database` são reaproveitados.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnowflakeConfig {
    pub account: String,
    #[serde(default)]
    pub auth: SnowflakeAuth,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub warehouse: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    /// Padrões (`DEV_*`) das databases mostradas na árvore e no autocomplete; vazio = todas.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub db_filter: Vec<String>,
    /// Clone local do projeto dbt ligado a esta conexão (visão de tabela).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbt: Option<DbtConfig>,
    /// `name` da configuração com as convenções da empresa (pasta `configurations/`); None = nenhum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DbtConfig {
    /// Pasta do projeto (aceita `~/`).
    pub project_dir: String,
    /// Target do profiles.yml que aponta para esta conta (`dbt parse --target`).
    pub target: String,
    /// `AWS_PROFILE` do `dbt parse` (dbt-loom busca manifests no S3); None = o do shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl: SslMode,
    /// Com túnel, `host`/`port` são resolvidos pelo bastion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// Presente = conexão Snowflake; `host`, `port`, `ssl` e `ssh` são ignorados.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snowflake: Option<SnowflakeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
}

/// Rótulo na barra de contexto; Production também pinta a borda da janela.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Production,
    Staging,
    Development,
}

impl Environment {
    pub fn label(self) -> &'static str {
        match self {
            Environment::Production => "PROD",
            Environment::Staging => "STAGING",
            Environment::Development => "DEV",
        }
    }
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

/// `~/x` → `$HOME/x`; o resto fica igual.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

pub fn scripts_dir() -> PathBuf {
    app_dir().join("scripts")
}

/// Scripts da conexão `id` (em `root/<id>/`, ordenados). Scripts soltos em `root`, de antes
/// dos scripts por conexão, vão para a primeira conexão que pedir.
pub fn conn_scripts(root: &Path, id: &str) -> Result<Vec<PathBuf>, DbError> {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).map_err(io)?;
    for p in sql_files(root)? {
        let target = dir.join(p.file_name().unwrap_or_default());
        if !target.exists() {
            std::fs::rename(&p, target).map_err(io)?;
        }
    }
    sql_files(&dir)
}

fn sql_files(dir: &Path) -> Result<Vec<PathBuf>, DbError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(io)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    Ok(files)
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

/// Renomeia o script na mesma pasta (`nome` → `nome.sql`). Não sobrescreve outro script.
pub fn rename_script(path: &Path, name: &str) -> Result<PathBuf, DbError> {
    // Barra viraria subpasta; o resto o macOS aceita.
    let name = name.trim().trim_end_matches(".sql").replace(['/', ':'], "-");
    if name.is_empty() || name.starts_with('.') {
        return Err(DbError::Config(rust_i18n::t!("err.script_name").to_string()));
    }
    let target = path.with_file_name(format!("{name}.sql"));
    if target == path {
        return Ok(target);
    }
    // Só a caixa mudou (APFS não diferencia): o rename direto funciona.
    let same_file = target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if target.exists() && !same_file {
        return Err(DbError::Config(rust_i18n::t!("err.script_exists", name = name).to_string()));
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
    write_atomic(path, &toml::to_string_pretty(cfg).map_err(|e| DbError::Config(e.to_string()))?)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), DbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Idioma da interface; `Auto` segue o macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Auto,
    Pt,
    En,
    Es,
}

/// Tema da interface; `Auto` segue o claro/escuro do macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub language: Language,
    #[serde(default)]
    pub theme: ThemeChoice,
    /// Escolhas recentes do seletor, por "conexão:tipo" (role, warehouse…), mais recente primeiro.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub recent: std::collections::BTreeMap<String, Vec<String>>,
    /// Conexões abertas quando o app fechou: reabertas na próxima partida.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_open: Vec<String>,
}

/// O que reconectar na partida: na ordem salva, só as que ainda existem, sem repetir.
pub fn reconnect_order(last_open: &[String], existing: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in last_open {
        if existing.contains(id) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

pub fn settings_path() -> PathBuf {
    app_dir().join("settings.toml")
}

/// Ausente ou inválido = padrão (com aviso no log): preferência não vale uma tela de erro.
pub fn load_settings(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!("{}: {e}", path.display());
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(path: &Path, s: &Settings) -> Result<(), DbError> {
    write_atomic(path, &toml::to_string_pretty(s).map_err(|e| DbError::Config(e.to_string()))?)
}

/// Muda uma preferência sem apagar as outras.
pub fn update_settings(path: &Path, f: impl FnOnce(&mut Settings)) -> Result<(), DbError> {
    let mut s = load_settings(path);
    f(&mut s);
    save_settings(path, &s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reconnect_keeps_only_existing_connections_in_order() {
        let ids = ["a".to_string(), "b".to_string(), "c".to_string()];
        let last = vec!["c".to_string(), "gone".to_string(), "a".to_string(), "c".to_string()];
        assert_eq!(reconnect_order(&last, &ids), vec!["c", "a"]);
        let old: Settings = toml::from_str("").unwrap();
        assert!(old.last_open.is_empty());
    }

    #[test]
    fn settings_keep_recent_picks_and_load_old_files() {
        let old: Settings = toml::from_str("language = \"pt\"\n").unwrap();
        assert!(old.recent.is_empty());
        let mut s = Settings::default();
        s.recent.insert("c1:role".into(), vec!["A".into()]);
        let back: Settings = toml::from_str(&toml::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.recent["c1:role"], vec!["A"]);
    }

    #[test]
    fn environment_round_trip_and_legacy_files() {
        let legacy = "[[connection]]\nid = \"a\"\nname = \"x\"\ndatabase = \"d\"\nuser = \"u\"\n";
        let f: ConfigFile = toml::from_str(legacy).unwrap();
        assert_eq!(f.connections[0].environment, None);
        assert!(!toml::to_string(&f).unwrap().contains("environment"), "None não grava a chave");

        let mut f2 = f.clone();
        f2.connections[0].environment = Some(Environment::Production);
        let s = toml::to_string(&f2).unwrap();
        assert!(s.contains("environment = \"production\""), "{s}");
        assert_eq!(toml::from_str::<ConfigFile>(&s).unwrap(), f2);
        assert_eq!(Environment::Production.label(), "PROD");
        assert_eq!(Environment::Staging.label(), "STAGING");
        assert_eq!(Environment::Development.label(), "DEV");
    }

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rb-config-{}-{name}", std::process::id())).join("connections.toml")
    }

    fn sample() -> ConnectionConfig {
        ConnectionConfig {
            environment: None,
            id: "c1".into(),
            name: "local".into(),
            host: "localhost".into(),
            port: 5432,
            database: "app".into(),
            user: "postgres".into(),
            ssl: SslMode::Require,
            ssh: None,
            snowflake: None,
        }
    }

    #[test]
    fn snowflake_block_round_trips_and_host_is_optional() {
        let path = tmp("snowflake");
        let mut c = sample();
        c.host = String::new();
        c.port = 0;
        c.snowflake = Some(SnowflakeConfig {
            dbt: None,
            configuration: Some("ACME".into()),
            account: "xy12345.us-east-1".into(),
            auth: SnowflakeAuth::KeyPair,
            private_key: Some("~/.ssh/sf.p8".into()),
            warehouse: Some("COMPUTE_WH".into()),
            role: None,
            schema: Some("PUBLIC".into()),
            db_filter: vec!["DEV_*".into(), "AI_*".into()],
        });
        let cfg = ConfigFile { connections: vec![c] };
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[connection.snowflake]") && text.contains("auth = \"keypair\"") && text.contains("db_filter = ["), "{text}");
        assert_eq!(load(&path).unwrap(), cfg);

        std::fs::write(&path, "[[connection]]\nid = \"s\"\nname = \"n\"\ndatabase = \"\"\nuser = \"u\"\n[connection.snowflake]\naccount = \"a\"\n").unwrap();
        let loaded = load(&path).unwrap().connections.remove(0);
        assert_eq!((loaded.host.as_str(), loaded.port), ("", 0));
        assert_eq!(loaded.snowflake.unwrap().auth, SnowflakeAuth::ExternalBrowser);
    }

    #[test]
    fn expand_home_only_touches_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~/.ssh/k.p8"), home.join(".ssh/k.p8"));
        assert_eq!(expand_home("/abs/k.p8"), PathBuf::from("/abs/k.p8"));
    }

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

    #[test]
    fn settings_round_trip_and_fall_back_to_auto() {
        let path = tmp("settings").with_file_name("settings.toml");
        save_settings(&path, &Settings { language: Language::Es, ..Default::default() }).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("language = \"es\""));
        assert_eq!(load_settings(&path).language, Language::Es);
        std::fs::write(&path, "language = \"klingon\"").unwrap();
        assert_eq!(load_settings(&path).language, Language::Auto);
        assert_eq!(load_settings(&path.with_file_name("nao-existe.toml")).language, Language::Auto);
    }

    #[test]
    fn updating_one_setting_keeps_the_others() {
        let path = tmp("settings-update").with_file_name("settings.toml");
        update_settings(&path, |s| s.language = Language::Es).unwrap();
        update_settings(&path, |s| s.theme = ThemeChoice::Dark).unwrap();
        assert_eq!(load_settings(&path), Settings { language: Language::Es, theme: ThemeChoice::Dark, ..Default::default() });
        assert!(std::fs::read_to_string(&path).unwrap().contains("theme = \"dark\""));
        // Arquivo antigo, só com idioma: tema volta para Auto.
        std::fs::write(&path, "language = \"pt\"").unwrap();
        assert_eq!(load_settings(&path).theme, ThemeChoice::Auto);
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
    fn conn_scripts_adopt_loose_ones_once() {
        let root = tmp("adopt").parent().unwrap().join("adopt-scripts");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("closed")).unwrap();
        std::fs::write(root.join("script-2.sql"), "b").unwrap();
        std::fs::write(root.join("script-1.sql"), "a").unwrap();
        std::fs::write(root.join("notas.txt"), "x").unwrap();
        let first = conn_scripts(&root, "c1").unwrap();
        assert_eq!(first, vec![root.join("c1/script-1.sql"), root.join("c1/script-2.sql")]);
        assert_eq!(std::fs::read_to_string(&first[0]).unwrap(), "a");
        assert!(root.join("notas.txt").exists() && root.join("closed").is_dir());
        // Os soltos já foram para c1; a próxima conexão começa vazia.
        assert!(conn_scripts(&root, "c2").unwrap().is_empty());
        assert_eq!(conn_scripts(&root, "c1").unwrap().len(), 2);
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
    fn scripts_can_be_renamed_without_overwriting() {
        let dir = tmp("rename").parent().unwrap().join("rename-scripts");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("script-1.sql"), dir.join("script-2.sql"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let p = rename_script(&a, " daily checks.sql ").unwrap();
        assert_eq!(p, dir.join("daily checks.sql"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a");
        assert!(!a.exists());
        assert!(rename_script(&b, "daily checks").is_err(), "não sobrescreve");
        assert_eq!(rename_script(&b, "x/y").unwrap(), dir.join("x-y.sql"));
        assert!(rename_script(&p, "  ").is_err() && rename_script(&p, ".hidden").is_err());
        assert_eq!(rename_script(&p, "Daily Checks").unwrap(), dir.join("Daily Checks.sql"), "só caixa");
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
