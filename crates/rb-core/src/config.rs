use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    /// TLS when the server offers it (libpq's default).
    #[default]
    Prefer,
    /// TLS required, with the certificate verified against the system roots.
    Require,
}

/// SSH tunnel of a connection. Empty fields come from `~/.ssh/config` or the OpenSSH defaults.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshConfig {
    /// `~/.ssh/config` alias or hostname.
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

/// Snowflake connection. `ConnectionConfig.user` and `.database` are reused.
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
    /// Patterns (`DEV_*`) of the databases shown in the tree and autocomplete; empty = all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub db_filter: Vec<String>,
    /// Local clone of the dbt project linked to this connection (table view).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbt: Option<DbtConfig>,
    /// `name` of the config with the company conventions (`configurations/` folder); None = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DbtConfig {
    /// Project folder (accepts `~/`).
    pub project_dir: String,
    /// profiles.yml target that points to this account (`dbt parse --target`).
    pub target: String,
    /// `AWS_PROFILE` for `dbt parse` (dbt-loom fetches manifests from S3); None = the shell's.
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
    /// With a tunnel, `host`/`port` are resolved by the bastion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// Present = Snowflake connection; `host`, `port`, `ssl` and `ssh` are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snowflake: Option<SnowflakeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
}

/// Label in the context bar; Production also paints the window border.
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
        c.host(&self.host).port(self.port).dbname(&self.database).user(&self.user).application_name("rustflake").connect_timeout(Duration::from_secs(10)).ssl_mode(match self.ssl {
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

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn app_dir() -> PathBuf {
    home_dir().join("Library/Application Support/rustflake")
}

/// Before anything reads it: moves the folder from when the app was called rustbeaver (once)
/// and makes it owner-only, since scripts and history may hold passwords.
pub fn prepare_app_dir() {
    let new = app_dir();
    let old = new.with_file_name("rustbeaver");
    if old.is_dir() && !new.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => tracing::info!("moved {} to {}", old.display(), new.display()),
            Err(e) => tracing::warn!("could not move {}: {e}", old.display()),
        }
    }
    let res = std::fs::create_dir_all(&new).and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o700))
    });
    if let Err(e) = res {
        tracing::warn!("{}: {e}", new.display());
    }
}

/// Created owner-only (0600); an existing file keeps its mode.
pub fn open_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).mode(0o600);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    o.open(path)
}

/// `std::fs::write`, but a new file is created owner-only.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    open_private(path, false)?.write_all(text.as_bytes())
}

pub fn connections_path() -> PathBuf {
    app_dir().join("connections.toml")
}

/// `~/x` → `$HOME/x`; anything else is unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    expand_home_in(path, &home_dir())
}

/// `~/x` → `home/x` (tests inject `home`).
pub fn expand_home_in(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(path),
    }
}

pub fn scripts_dir() -> PathBuf {
    app_dir().join("scripts")
}

/// Scripts of connection `id` (in `root/<id>/`, sorted).
pub fn conn_scripts(root: &Path, id: &str) -> Result<Vec<PathBuf>, DbError> {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).map_err(io)?;
    sql_files(&dir)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OpenTables {
    #[serde(default)]
    tables: Vec<[String; 2]>,
}

/// Table tabs ([schema, table]) a connection had open: `tables.toml` next to its scripts.
pub fn load_open_tables(conn_dir: &Path) -> Vec<[String; 2]> {
    let Ok(s) = std::fs::read_to_string(conn_dir.join("tables.toml")) else { return Vec::new() };
    toml::from_str::<OpenTables>(&s).map(|t| t.tables).unwrap_or_else(|e| {
        tracing::warn!("tables.toml: {e}");
        Vec::new()
    })
}

/// Writes the list; empty removes the file.
pub fn save_open_tables(conn_dir: &Path, tables: &[[String; 2]]) -> Result<(), DbError> {
    let path = conn_dir.join("tables.toml");
    if tables.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(conn_dir).map_err(io)?;
    let body = toml::to_string(&OpenTables { tables: tables.to_vec() }).map_err(|e| DbError::Io(e.to_string()))?;
    std::fs::write(&path, body).map_err(io)
}

fn sql_files(dir: &Path) -> Result<Vec<PathBuf>, DbError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map_err(io)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "sql")).collect();
    files.sort();
    Ok(files)
}

/// Creates an empty `script-N.sql` with the smallest free N. `create_new` ensures two tabs
/// never share the same file.
pub fn new_script(dir: &Path) -> Result<PathBuf, DbError> {
    std::fs::create_dir_all(dir).map_err(io)?;
    use std::os::unix::fs::OpenOptionsExt;
    for n in 1.. {
        let path = dir.join(format!("script-{n}.sql"));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        }
    }
    unreachable!("1.. is infinite")
}

/// Moves the script to `closed/` next to it, without overwriting anything already there.
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

/// Renames the script in the same folder (`name` → `name.sql`). Does not overwrite another script.
pub fn rename_script(path: &Path, name: &str) -> Result<PathBuf, DbError> {
    // A slash would become a subfolder; macOS accepts the rest.
    let name = name.trim().trim_end_matches(".sql").replace(['/', ':'], "-");
    if name.is_empty() || name.starts_with('.') {
        return Err(DbError::Config(rust_i18n::t!("err.script_name").to_string()));
    }
    let target = path.with_file_name(format!("{name}.sql"));
    if target == path {
        return Ok(target);
    }
    // Only the case changed (APFS is case-insensitive): a direct rename works.
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

/// Writes atomically (temp file + rename).
pub fn save(path: &Path, cfg: &ConfigFile) -> Result<(), DbError> {
    write_atomic(path, &toml::to_string_pretty(cfg).map_err(|e| DbError::Config(e.to_string()))?)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), DbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Interface language; `Auto` follows macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Auto,
    Pt,
    En,
    Es,
}

/// Interface theme; `Auto` follows macOS light/dark.
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
    /// Recent picker choices, by "connection:type" (role, warehouse…), most recent first.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub recent: std::collections::BTreeMap<String, Vec<String>>,
    /// Connections open when the app closed: reopened on the next launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_open: Vec<String>,
}

/// What to reconnect on launch: in saved order, only those that still exist, without repeats.
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

/// Missing or invalid = default (with a log warning): a preference isn't worth an error screen.
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
        assert!(!toml::to_string(&f).unwrap().contains("environment"), "None does not write the key");

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
        c.ssh = Some(SshConfig { host: "prod-bastion".into(), user: Some("deploy".into()), port: None, identity_file: Some("~/.ssh/id_ed25519".into()) });
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
        assert_eq!(load_settings(&path.with_file_name("missing.toml")).language, Language::Auto);
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
    fn new_scripts_never_reuse_a_path_and_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("scripts").parent().unwrap().join("scripts");
        let _ = std::fs::remove_dir_all(&dir);
        let a = new_script(&dir).unwrap();
        let b = new_script(&dir).unwrap();
        assert_ne!(a, b);
        assert!(a.exists() && b.exists());
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        let cfg = dir.join("connections.toml");
        save(&cfg, &ConfigFile::default()).unwrap();
        assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn conn_scripts_are_sorted_sql_files_of_the_connection() {
        let root = tmp("conn").parent().unwrap().join("conn-scripts");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("c1")).unwrap();
        std::fs::write(root.join("c1/script-2.sql"), "b").unwrap();
        std::fs::write(root.join("c1/script-1.sql"), "a").unwrap();
        std::fs::write(root.join("c1/notes.txt"), "x").unwrap();
        std::fs::write(root.join("loose.sql"), "x").unwrap();
        assert_eq!(conn_scripts(&root, "c1").unwrap(), vec![root.join("c1/script-1.sql"), root.join("c1/script-2.sql")]);
        assert!(conn_scripts(&root, "c2").unwrap().is_empty());
    }

    #[test]
    fn archiving_never_overwrites() {
        let dir = tmp("archive").parent().unwrap().join("scripts");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("script-1.sql");
        std::fs::write(&p, "first").unwrap();
        let first = archive_script(&p).unwrap();
        std::fs::write(&p, "second").unwrap();
        let second = archive_script(&p).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "second");
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
        assert!(rename_script(&b, "daily checks").is_err(), "does not overwrite");
        assert_eq!(rename_script(&b, "x/y").unwrap(), dir.join("x-y.sql"));
        assert!(rename_script(&p, "  ").is_err() && rename_script(&p, ".hidden").is_err());
        assert_eq!(rename_script(&p, "Daily Checks").unwrap(), dir.join("Daily Checks.sql"), "case only");
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn open_tables_round_trip_next_to_the_scripts() {
        let dir = std::env::temp_dir().join(format!("rf-tables-{}", std::process::id()));
        assert!(load_open_tables(&dir).is_empty(), "no file = nothing");
        let t = vec![["MARTS_PROD.MART_CDP".to_string(), "DIM_COMPANIES_LATEST".to_string()], ["public".to_string(), "users".to_string()]];
        save_open_tables(&dir, &t).unwrap();
        assert_eq!(load_open_tables(&dir), t);
        save_open_tables(&dir, &[]).unwrap();
        assert!(!dir.join("tables.toml").exists(), "empty list removes the file");
        save_open_tables(&dir, &[]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
