//! Local dbt project (clone): manifest.json, run_results.json and git, for the table view.
//! Read-only; the only process that runs is `dbt parse` (and `git` for the GitHub link).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rust_i18n::t;
use serde::Deserialize;
use serde_json::Value;

use crate::DbError;
use crate::config::DbtConfig;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub unique_id: String,
    pub name: String,
    pub resource_type: String,
    pub relation: Option<String>,
    pub file: String,
    pub fqn: Vec<String>,
    pub description: String,
    /// (name, description) in schema.yml order.
    pub columns: Vec<(String, String)>,
    pub materialized: Option<String>,
    pub strategy: Option<String>,
    pub unique_key: Vec<String>,
    pub tags: Vec<String>,
    pub meta: Vec<(String, String)>,
    pub owner: Option<String>,
    pub raw_code: String,
    pub compiled_code: Option<String>,
    pub source_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestDef {
    pub unique_id: String,
    /// not_null, unique, accepted_values, relationships, …
    pub kind: String,
    pub attached: String,
    pub column: Option<String>,
    /// relationships: "col → model"; accepted_values: how many values.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub nodes: HashMap<String, Node>,
    pub child_map: HashMap<String, Vec<String>>,
    pub parent_map: HashMap<String, Vec<String>>,
    pub tests: Vec<TestDef>,
    by_relation: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct RawManifest {
    #[serde(default)]
    nodes: HashMap<String, RawNode>,
    #[serde(default)]
    sources: HashMap<String, RawNode>,
    #[serde(default)]
    child_map: HashMap<String, Vec<String>>,
    #[serde(default)]
    parent_map: HashMap<String, Vec<String>>,
}

/// Only the fields used: the manifest can be tens of MB.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawNode {
    unique_id: String,
    name: String,
    resource_type: String,
    relation_name: Option<String>,
    original_file_path: String,
    fqn: Vec<String>,
    description: Option<String>,
    columns: serde_json::Map<String, Value>,
    config: RawConfig,
    meta: serde_json::Map<String, Value>,
    raw_code: Option<String>,
    compiled_code: Option<String>,
    source_name: Option<String>,
    test_metadata: Option<RawTestMeta>,
    attached_node: Option<String>,
    column_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    materialized: Option<String>,
    incremental_strategy: Option<String>,
    unique_key: Value,
    tags: Value,
    meta: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTestMeta {
    name: String,
    kwargs: serde_json::Map<String, Value>,
}

/// Text or list of texts (unique_key, tags).
fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        o => o.to_string(),
    }
}

/// Nested object becomes `a.b.c: value`; list of texts becomes "x, y".
fn flatten_meta(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(o) => o.iter().for_each(|(k, v)| flatten_meta(&format!("{prefix}.{k}"), v, out)),
        Value::Array(a) => out.push((prefix.to_string(), a.iter().map(text).collect::<Vec<_>>().join(", "))),
        Value::Null => {}
        other => out.push((prefix.to_string(), text(other))),
    }
}

/// `"DB"."S"."T"` / `db.s.t` → `DB.S.T`.
pub fn norm_relation(s: &str) -> String {
    s.replace('"', "").to_uppercase()
}

/// `ref('x')` / `ref("p", "x")` → `x`.
fn ref_target(v: &str) -> String {
    let inner: Vec<&str> = v.split(['\'', '"']).enumerate().filter(|(i, _)| i % 2 == 1).map(|(_, p)| p).collect();
    inner.last().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())
}

pub fn parse_manifest(json: &str) -> Result<Manifest, DbError> {
    let raw: RawManifest = serde_json::from_str(json).map_err(|e| DbError::Config(t!("err.dbt_manifest", error = e).to_string()))?;
    let mut m = Manifest { child_map: raw.child_map, parent_map: raw.parent_map, ..Default::default() };
    // Sources first: if a model has the same relation, the model wins (always, not by HashMap luck).
    for (id, n) in raw.sources.into_iter().chain(raw.nodes) {
        if n.resource_type == "test" {
            if let (Some(tm), Some(att)) = (n.test_metadata, n.attached_node) {
                let col = n.column_name.or_else(|| tm.kwargs.get("column_name").map(text));
                let detail = match tm.name.as_str() {
                    "relationships" => tm.kwargs.get("to").map(|to| format!("{} → {}", col.clone().unwrap_or_default(), ref_target(&text(to)))),
                    "accepted_values" => tm.kwargs.get("values").and_then(Value::as_array).map(|a| a.len().to_string()),
                    _ => None,
                };
                m.tests.push(TestDef { unique_id: id, kind: tm.name, attached: att, column: col, detail });
            }
            continue;
        }
        // dbt copies `meta` to `config.meta`: merge, flatten and drop the repeats.
        let mut meta: Vec<(String, String)> = Vec::new();
        for (k, v) in n.config.meta.iter().chain(n.meta.iter()) {
            flatten_meta(k, v, &mut meta);
        }
        meta.sort();
        meta.dedup();
        let owner = n.meta.get("owner").or_else(|| n.config.meta.get("owner")).map(text);
        let node = Node {
            unique_id: id.clone(),
            name: n.name,
            resource_type: n.resource_type,
            relation: n.relation_name.clone(),
            file: n.original_file_path,
            fqn: n.fqn,
            description: n.description.unwrap_or_default(),
            columns: n.columns.iter().map(|(k, v)| (v.get("name").and_then(Value::as_str).unwrap_or(k).to_string(), v.get("description").and_then(Value::as_str).unwrap_or("").to_string())).collect(),
            materialized: n.config.materialized,
            strategy: n.config.incremental_strategy,
            unique_key: strings(&n.config.unique_key),
            tags: strings(&n.config.tags),
            meta,
            owner,
            raw_code: n.raw_code.unwrap_or_default(),
            compiled_code: n.compiled_code,
            source_name: n.source_name,
        };
        if let Some(r) = &n.relation_name {
            m.by_relation.insert(norm_relation(r), id.clone());
        }
        m.nodes.insert(id, node);
    }
    Ok(m)
}

impl Manifest {
    pub fn by_table(&self, db: &str, schema: &str, name: &str) -> Option<&Node> {
        let key = norm_relation(&format!("{db}.{schema}.{name}"));
        self.by_relation.get(&key).and_then(|id| self.nodes.get(id))
    }

    /// Exact by relation; otherwise a single node with the same table name (manifest generated with
    /// another target, e.g. dev: different database/schema). `bool` = matched exactly.
    pub fn find_table(&self, db: &str, schema: &str, name: &str) -> Option<(&Node, bool)> {
        if let Some(n) = self.by_table(db, schema, name) {
            return Some((n, true));
        }
        let want = name.replace('"', "").to_uppercase();
        let mut hits = self.by_relation.iter().filter(|(rel, _)| rel.rsplit('.').next() == Some(want.as_str()));
        match (hits.next(), hits.next()) {
            (Some((_, id)), None) => self.nodes.get(id).map(|n| (n, false)),
            _ => None,
        }
    }

    pub fn tests_of(&self, uid: &str) -> Vec<&TestDef> {
        self.tests.iter().filter(|t| t.attached == uid).collect()
    }

    /// Layer for lineage: first folder under `models/`; sources and seeds are "Sources".
    pub fn layer(&self, uid: &str) -> String {
        let Some(n) = self.nodes.get(uid) else { return "Sources".into() };
        if n.resource_type != "model" && n.resource_type != "snapshot" {
            return "Sources".into();
        }
        let folder = if n.fqn.len() > 2 { n.fqn[1].as_str() } else { "models" };
        let mut c = folder.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let k = |t: &str| self.nodes.values().filter(|n| n.resource_type == t).count();
        (k("model"), k("source"), k("seed"))
    }
}

/// `unique_id` → (status, completed_at) of the last run; only if it came from the same target.
pub fn parse_run_results(json: &str, target: &str) -> HashMap<String, (String, String)> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return HashMap::new() };
    if v.pointer("/args/target").and_then(Value::as_str) != Some(target) {
        return HashMap::new();
    }
    v.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let id = r.get("unique_id")?.as_str()?.to_string();
            let status = r.get("status").map(text).unwrap_or_default();
            let done = r.get("timing").and_then(Value::as_array).and_then(|t| t.iter().filter_map(|x| x.get("completed_at").and_then(Value::as_str)).last()).unwrap_or("").to_string();
            Some((id, (status, done)))
        })
        .collect()
}

/// Git remote → GitHub repo page (SSH, HTTPS or ssh://).
pub fn github_url(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches(".git");
    let path =
        r.strip_prefix("git@github.com:").or_else(|| r.strip_prefix("https://github.com/")).or_else(|| r.strip_prefix("ssh://git@github.com/")).or_else(|| r.strip_prefix("http://github.com/"))?;
    Some(format!("https://github.com/{}", path.trim_matches('/')))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitInfo {
    pub repo: Option<String>,
    pub branch: String,
    pub sha: String,
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn git_info(dir: &Path) -> GitInfo {
    GitInfo {
        repo: git(dir, &["remote", "get-url", "origin"]).and_then(|r| github_url(&r)),
        branch: git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default(),
        sha: git(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default(),
    }
}

pub fn file_url(g: &GitInfo, file: &str) -> Option<String> {
    Some(format!("{}/blob/{}/{file}", g.repo.as_ref()?, if g.branch.is_empty() { "main" } else { &g.branch }))
}

pub fn commit_url(g: &GitInfo, sha: &str) -> Option<String> {
    Some(format!("{}/commit/{sha}", g.repo.as_ref()?))
}

/// One `git blame` line: who touched it last and in which commit.
#[derive(Debug, Clone, PartialEq)]
pub struct BlameLine {
    /// 7 characters.
    pub sha: String,
    pub author: String,
    pub time: i64,
    pub summary: String,
    pub text: String,
    /// Local change not yet committed.
    pub uncommitted: bool,
}

/// `git blame --porcelain`: commit data comes only on its first line; afterwards only the sha.
pub fn parse_blame(out: &str) -> Vec<BlameLine> {
    let mut info: HashMap<String, (String, i64, String)> = HashMap::new();
    let mut lines = Vec::new();
    let mut cur_sha = String::new();
    for l in out.lines() {
        if let Some(text) = l.strip_prefix('\t') {
            let (author, time, summary) = info.get(&cur_sha).cloned().unwrap_or_default();
            lines.push(BlameLine { sha: cur_sha.chars().take(7).collect(), author, time, summary, text: text.to_string(), uncommitted: cur_sha.chars().all(|c| c == '0') });
            continue;
        }
        let mut parts = l.splitn(2, ' ');
        let (k, v) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if k.len() == 40 && k.chars().all(|c| c.is_ascii_hexdigit()) {
            cur_sha = k.to_string();
            info.entry(cur_sha.clone()).or_default();
            continue;
        }
        if let Some(e) = info.get_mut(&cur_sha) {
            match k {
                "author" => e.0 = v.to_string(),
                "author-time" => e.1 = v.parse().unwrap_or(0),
                "summary" => e.2 = v.to_string(),
                _ => {}
            }
        }
    }
    lines
}

/// `git blame` of the model file (run off the UI thread).
pub fn blame(dir: &Path, file: &str) -> Result<Vec<BlameLine>, DbError> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["blame", "--porcelain", "--", file]).output().map_err(|e| DbError::Io(e.to_string()))?;
    if !out.status.success() {
        return Err(DbError::Config(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(parse_blame(&String::from_utf8_lossy(&out.stdout)))
}

/// `~/x` → absolute path.
pub fn expand_dir(dir: &str) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(dir),
    }
}

#[derive(Debug, Clone)]
pub struct DbtProject {
    pub dir: PathBuf,
    pub target: String,
    pub manifest: Manifest,
    pub results: HashMap<String, (String, String)>,
    pub git: GitInfo,
    pub parsed_at: Option<SystemTime>,
}

/// Reads `target/manifest.json` (+ run_results and git). Run off the UI thread: large file.
pub fn load(cfg: &DbtConfig) -> Result<DbtProject, DbError> {
    let dir = expand_dir(&cfg.project_dir);
    let path = dir.join("target").join("manifest.json");
    let json = std::fs::read_to_string(&path).map_err(|_| DbError::Config(t!("err.dbt_no_manifest", path = path.display()).to_string()))?;
    let manifest = parse_manifest(&json)?;
    let results = std::fs::read_to_string(dir.join("target").join("run_results.json")).map(|j| parse_run_results(&j, &cfg.target)).unwrap_or_default();
    let parsed_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    Ok(DbtProject { git: git_info(&dir), dir, target: cfg.target.clone(), manifest, results, parsed_at })
}

/// Strips terminal colors (`ESC[…m`) from dbt output.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            it.next();
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `env -0`: `KEY=value` pairs separated by NUL (values may contain `=` and line breaks).
pub fn parse_env(out: &str) -> Vec<(String, String)> {
    out.split('\0').filter_map(|kv| kv.split_once('=')).filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// The user's login shell environment: when opened from the Dock/Finder, the app inherits only
/// `/usr/bin:/bin:…`, without brew/pipx dbt or the variables profiles.yml reads (`env_var`).
fn login_env() -> Vec<(String, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    std::process::Command::new(shell).args(["-lc", "env -0"]).output().ok().filter(|o| o.status.success()).map(|o| parse_env(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

/// Profiles from `~/.aws/config` (`[default]`, `[profile x]`), in file order.
pub fn aws_profiles(config: &str) -> Vec<String> {
    config
        .lines()
        .filter_map(|l| l.trim().strip_prefix('[')?.strip_suffix(']').map(str::trim))
        .filter_map(|h| if h == "default" { Some(h.to_string()) } else { h.strip_prefix("profile ").map(|p| p.trim().to_string()) })
        .collect()
}

pub fn read_aws_profiles() -> Vec<String> {
    std::fs::read_to_string(expand_dir("~/.aws/config")).map(|c| aws_profiles(&c)).unwrap_or_default()
}

/// `dbt parse` failed because the AWS SSO session (dbt-loom → S3) expired.
pub fn needs_aws_login(output: &str) -> bool {
    let o = output.to_lowercase();
    o.contains("aws sso login") || (o.contains("sso") && o.contains("token") && o.contains("expired"))
}

/// The login shell's `AWS_PROFILE` (what dbt would use with no configuration).
pub fn shell_aws_profile() -> Option<String> {
    login_env().into_iter().find(|(k, _)| k == "AWS_PROFILE").map(|(_, v)| v).filter(|v| !v.is_empty())
}

/// `aws sso login --profile <p>`: opens the browser and waits for the login (up to 5 min).
pub fn aws_login(profile: &str) -> Result<(), DbError> {
    let out = std::process::Command::new("aws")
        .args(["sso", "login", "--profile", profile])
        .envs(login_env())
        .output()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { DbError::Config(t!("err.aws_not_found").to_string()) } else { DbError::Io(e.to_string()) })?;
    if out.status.success() { Ok(()) } else { Err(DbError::Config(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string())) }
}

/// The project virtualenv's dbt, if any: it is what the project expects.
pub fn venv_dbt(dir: &Path) -> Option<PathBuf> {
    let p = dir.join(".venv").join("bin").join("dbt");
    p.is_file().then_some(p)
}

/// Order of attempts: the project venv's dbt, dbt Fusion (Rust engine, `dbtf`) and dbt-core.
pub fn dbt_candidates(dir: &Path) -> Vec<PathBuf> {
    venv_dbt(dir).into_iter().chain([PathBuf::from("dbtf"), PathBuf::from("dbt")]).collect()
}

/// Last `n` lines of the log (no colors), to follow `dbt parse` live.
pub fn log_tail(log: &Path, n: usize) -> Vec<String> {
    let text = strip_ansi(&std::fs::read_to_string(log).unwrap_or_default());
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}

/// One log per run: two connections may Refresh at the same time.
pub fn log_path() -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("rb-dbt-parse-{}-{n}.log", std::process::id()))
}

/// `dbt parse --target <t>` in the folder: only reads files, does not query the warehouse. 120 s cap.
pub fn refresh(cfg: &DbtConfig) -> Result<(), DbError> {
    refresh_logged(cfg, &log_path())
}

/// Same as `refresh`, with the log at a known path: the screen reads it while dbt runs.
pub fn refresh_logged(cfg: &DbtConfig, log: &Path) -> Result<(), DbError> {
    run_dbt(cfg, &["parse", "--target", &cfg.target], log)
}

/// `dbt compile` for the model only: may query the warehouse (macros with run_query), unlike parse.
pub fn compile_args<'a>(model: &'a str, target: &'a str) -> Vec<&'a str> {
    vec!["compile", "--select", model, "--target", target]
}

/// Where dbt writes the compiled SQL: `target/compiled/<project>/<model file>`.
pub fn compiled_path(dir: &Path, unique_id: &str, file: &str) -> Option<PathBuf> {
    let project = unique_id.split('.').nth(1)?;
    Some(dir.join("target").join("compiled").join(project).join(file))
}

/// Compiles only this model and returns the compiled SQL.
pub fn compile_model(cfg: &DbtConfig, node: &Node, log: &Path) -> Result<String, DbError> {
    run_dbt(cfg, &compile_args(&node.name, &cfg.target), log)?;
    let path = compiled_path(&expand_dir(&cfg.project_dir), &node.unique_id, &node.file).ok_or_else(|| DbError::Config(node.unique_id.clone()))?;
    std::fs::read_to_string(&path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))
}

/// Already existing compiled SQL on disk (from a previous compile/run) and when it was generated.
pub fn compiled_on_disk(dir: &Path, node: &Node) -> Option<(String, SystemTime)> {
    let p = compiled_path(dir, &node.unique_id, &node.file)?;
    let sql = std::fs::read_to_string(&p).ok()?;
    Some((sql, std::fs::metadata(&p).and_then(|m| m.modified()).ok()?))
}

/// Runs dbt in the project folder with the login shell environment; 120 s cap; output in `log`.
fn run_dbt(cfg: &DbtConfig, args: &[&str], log: &Path) -> Result<(), DbError> {
    let dir = expand_dir(&cfg.project_dir);
    let log = log.to_path_buf();
    let file = std::fs::File::create(&log).map_err(|e| DbError::Io(e.to_string()))?;
    let err_file = file.try_clone().map_err(|e| DbError::Io(e.to_string()))?;
    let env = login_env();
    // The first executable that exists (the shell PATH resolves pipx/brew/Fusion).
    let mut spawned = None;
    for program in dbt_candidates(&dir) {
        let (out, err) = (file.try_clone().map_err(|e| DbError::Io(e.to_string()))?, err_file.try_clone().map_err(|e| DbError::Io(e.to_string()))?);
        let mut cmd = std::process::Command::new(&program);
        cmd.envs(env.iter().cloned()).env("DBT_USE_COLORS", "false");
        // Profile chosen on the connection wins over the shell's (dbt-loom/S3).
        if let Some(p) = &cfg.aws_profile {
            cmd.env("AWS_PROFILE", p);
        }
        cmd.args(args).current_dir(&dir).stdout(out).stderr(err);
        match cmd.spawn() {
            Ok(c) => {
                spawned = Some((c, program));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(DbError::Io(e.to_string())),
        }
    }
    let Some((mut child, program)) = spawned else { return Err(DbError::Config(t!("err.dbt_not_found").to_string())) };
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| DbError::Io(e.to_string()))? {
            break Some(s);
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            // Without wait the dead process stays as a zombie.
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let output = strip_ansi(&std::fs::read_to_string(&log).unwrap_or_default());
    let _ = std::fs::remove_file(&log);
    match status {
        Some(s) if s.success() => Ok(()),
        Some(_) => Err(DbError::Config(t!("err.dbt_parse_failed", output = format!("$ {} {}\n{}", program.display(), args.join(" "), output.trim())).to_string())),
        None => Err(DbError::Config(t!("err.dbt_timeout").to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
      "metadata": {"project_name": "proj"},
      "nodes": {
        "model.proj.cost_daily": {
          "unique_id": "model.proj.cost_daily", "name": "cost_daily", "resource_type": "model",
          "relation_name": "\"SALES_PROD\".\"MART_CRM\".\"COST_DAILY\"",
          "original_file_path": "models/marts/aws/cost_daily.sql",
          "fqn": ["proj", "marts", "aws", "cost_daily"],
          "description": "Daily AWS cost",
          "columns": {"usage_date": {"name": "usage_date", "description": "Day of cost"}, "account_id": {"name": "account_id", "description": ""}},
          "config": {"materialized": "incremental", "incremental_strategy": "merge", "unique_key": ["usage_date", "account_id"], "tags": ["aws", "finance"], "meta": {"owner": "data-platform", "tier": "gold"}},
          "meta": {},
          "raw_code": "select 1", "compiled_code": "select 2"
        },
        "model.proj.stg": {
          "unique_id": "model.proj.stg", "name": "stg_aws__accounts", "resource_type": "model",
          "relation_name": "staging_prod.stg_aws.stg_aws__accounts", "original_file_path": "models/staging/aws/stg.sql",
          "fqn": ["proj", "staging", "aws", "stg"], "config": {"materialized": "view", "unique_key": "account_id"}, "meta": {"owner": "ingest"}, "raw_code": "select 3"
        },
        "seed.proj.mapping": {"unique_id": "seed.proj.mapping", "name": "mapping", "resource_type": "seed", "relation_name": "SEEDS.FIN.MAPPING", "fqn": ["proj", "mapping"], "original_file_path": "seeds/mapping.csv"},
        "test.proj.nn": {"unique_id": "test.proj.nn", "resource_type": "test", "test_metadata": {"name": "not_null", "kwargs": {"column_name": "usage_date"}}, "attached_node": "model.proj.cost_daily", "column_name": "usage_date"},
        "test.proj.av": {"unique_id": "test.proj.av", "resource_type": "test", "test_metadata": {"name": "accepted_values", "kwargs": {"column_name": "service", "values": ["a","b","c","d","e","f"]}}, "attached_node": "model.proj.cost_daily", "column_name": "service"},
        "test.proj.rel": {"unique_id": "test.proj.rel", "resource_type": "test", "test_metadata": {"name": "relationships", "kwargs": {"column_name": "account_id", "to": "ref('stg_aws__accounts')", "field": "account_id"}}, "attached_node": "model.proj.cost_daily", "column_name": "account_id"},
        "test.proj.uniq": {"unique_id": "test.proj.uniq", "resource_type": "test", "test_metadata": {"name": "unique_combination_of_columns", "namespace": "dbt_utils", "kwargs": {}}, "attached_node": "model.proj.cost_daily"}
      },
      "sources": {
        "source.proj.aws_org.accounts": {"unique_id": "source.proj.aws_org.accounts", "name": "accounts", "source_name": "aws_org", "resource_type": "source", "relation_name": "RAW_PROD.AWS_ORG.ACCOUNTS", "fqn": ["proj", "aws_org", "accounts"], "original_file_path": "models/staging/aws/_sources.yml", "description": "Org accounts"}
      },
      "child_map": {"model.proj.stg": ["model.proj.cost_daily"]},
      "parent_map": {"model.proj.cost_daily": ["model.proj.stg", "seed.proj.mapping"]}
    }"#;

    #[test]
    fn manifest_nodes_are_found_by_relation_regardless_of_quotes_and_case() {
        let m = parse_manifest(MANIFEST).unwrap();
        let n = m.by_table("sales_prod", "MART_CRM", "cost_daily").expect("model");
        assert_eq!(n.unique_id, "model.proj.cost_daily");
        assert_eq!((n.materialized.as_deref(), n.strategy.as_deref()), (Some("incremental"), Some("merge")));
        assert_eq!(n.unique_key, vec!["usage_date", "account_id"]);
        assert_eq!(n.owner.as_deref(), Some("data-platform"), "config.meta.owner");
        assert_eq!(n.tags, vec!["aws", "finance"]);
        assert!(n.meta.contains(&("tier".to_string(), "gold".to_string())));
        assert_eq!(n.compiled_code.as_deref(), Some("select 2"));
        assert_eq!(m.by_table("STAGING_PROD", "STG_AWS", "STG_AWS__ACCOUNTS").unwrap().owner.as_deref(), Some("ingest"), "meta.owner");
        assert_eq!(m.by_table("STAGING_PROD", "STG_AWS", "STG_AWS__ACCOUNTS").unwrap().unique_key, vec!["account_id"], "unique_key text");
        assert!(m.by_table("RAW_PROD", "AWS_ORG", "ACCOUNTS").is_some(), "source too");
    }

    #[test]
    fn layers_come_from_the_models_folder() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.layer("model.proj.cost_daily"), "Marts");
        assert_eq!(m.layer("model.proj.stg"), "Staging");
        assert_eq!(m.layer("seed.proj.mapping"), "Sources");
        assert_eq!(m.layer("source.proj.aws_org.accounts"), "Sources");
    }

    #[test]
    fn tests_carry_column_and_detail() {
        let m = parse_manifest(MANIFEST).unwrap();
        let t = m.tests_of("model.proj.cost_daily");
        assert_eq!(t.len(), 4);
        let rel = t.iter().find(|x| x.kind == "relationships").unwrap();
        assert_eq!((rel.column.as_deref(), rel.detail.as_deref()), (Some("account_id"), Some("account_id → stg_aws__accounts")));
        assert_eq!(t.iter().find(|x| x.kind == "accepted_values").unwrap().detail.as_deref(), Some("6"));
        assert_eq!(t.iter().find(|x| x.kind == "unique_combination_of_columns").unwrap().column, None);
        assert_eq!(m.child_map["model.proj.stg"], vec!["model.proj.cost_daily"]);
    }

    #[test]
    fn run_results_only_for_the_same_target() {
        let rr = r#"{"args": {"target": "prod"}, "results": [{"unique_id": "test.proj.nn", "status": "pass", "timing": [{"name": "compile", "completed_at": "a"}, {"name": "execute", "completed_at": "2026-10-16T09:00:00Z"}]}]}"#;
        let r = parse_run_results(rr, "prod");
        assert_eq!(r["test.proj.nn"], ("pass".to_string(), "2026-10-16T09:00:00Z".to_string()));
        assert!(parse_run_results(rr, "dev").is_empty());
        assert!(parse_run_results(r#"{"results": []}"#, "prod").is_empty(), "no target: cannot confirm");
    }

    #[test]
    fn github_urls_from_remotes() {
        for r in ["git@github.com:org/repo.git", "https://github.com/org/repo", "https://github.com/org/repo.git", "ssh://git@github.com/org/repo.git"] {
            assert_eq!(github_url(r).as_deref(), Some("https://github.com/org/repo"), "{r}");
        }
        assert_eq!(github_url("git@gitlab.com:org/repo.git"), None);
        let g = GitInfo { repo: Some("https://github.com/org/repo".into()), branch: "main".into(), sha: "4f2a91c".into() };
        assert_eq!(file_url(&g, "models/x.sql").as_deref(), Some("https://github.com/org/repo/blob/main/models/x.sql"));
    }

    #[test]
    fn broken_manifest_is_an_error() {
        assert!(parse_manifest("{").is_err());
    }

    #[test]
    fn login_shell_env_is_parsed_from_nul_separated_output() {
        let env = parse_env("PATH=/opt/homebrew/bin:/usr/bin\0SNOWFLAKE_USER=me\0WEIRD\0X=a=b\0");
        assert_eq!(env, vec![("PATH".into(), "/opt/homebrew/bin:/usr/bin".into()), ("SNOWFLAKE_USER".into(), "me".into()), ("X".into(), "a=b".into())]);
    }

    #[test]
    fn project_venv_dbt_wins() {
        let dir = std::env::temp_dir().join(format!("rb-dbt-venv-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
        assert_eq!(venv_dbt(&dir), None, "no file, nothing");
        std::fs::write(dir.join(".venv/bin/dbt"), "").unwrap();
        assert_eq!(venv_dbt(&dir), Some(dir.join(".venv/bin/dbt")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_files_are_unique_per_refresh() {
        assert_ne!(log_path(), log_path());
    }

    #[test]
    fn models_win_over_sources_with_the_same_relation() {
        let m = parse_manifest(
            r#"{"nodes": {"model.p.x": {"unique_id": "model.p.x", "name": "x", "resource_type": "model", "relation_name": "D.S.X"}},
            "sources": {"source.p.s.x": {"unique_id": "source.p.s.x", "name": "x", "resource_type": "source", "relation_name": "D.S.X"}}}"#,
        )
        .unwrap();
        for _ in 0..5 {
            assert_eq!(m.by_table("D", "S", "X").unwrap().resource_type, "model");
        }
    }

    #[test]
    fn falls_back_to_a_unique_table_name_when_the_target_differs() {
        let m = parse_manifest(
            r#"{"nodes": {
            "model.p.a": {"unique_id": "model.p.a", "name": "dim_product_groups", "resource_type": "model", "relation_name": "DEV_JD.MART_CX.DIM_PRODUCT_GROUPS"},
            "model.p.b": {"unique_id": "model.p.b", "name": "x", "resource_type": "model", "relation_name": "DEV_JD.S1.DUP"},
            "model.p.c": {"unique_id": "model.p.c", "name": "y", "resource_type": "model", "relation_name": "DEV_JD.S2.DUP"}
        }}"#,
        )
        .unwrap();
        let (n, exact) = m.find_table("SALES_PROD", "MART_CX", "DIM_PRODUCT_GROUPS").unwrap();
        assert_eq!((n.unique_id.as_str(), exact), ("model.p.a", false));
        assert!(m.find_table("SALES_PROD", "X", "DUP").is_none(), "ambiguous name: no guessing");
        assert_eq!(m.find_table("DEV_JD", "S1", "DUP").map(|(n, e)| (n.unique_id.as_str(), e)), Some(("model.p.b", true)));
    }

    #[test]
    fn ansi_colors_are_stripped_from_dbt_output() {
        assert_eq!(strip_ansi("\u{1b}[0m17:10:56  Running\u{1b}[31m err\u{1b}[0m"), "17:10:56  Running err");
    }

    #[test]
    fn dbt_candidates_prefer_venv_then_fusion_then_core() {
        let dir = std::env::temp_dir().join(format!("rb-dbt-cand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dbt_candidates(&dir), vec![PathBuf::from("dbtf"), PathBuf::from("dbt")]);
        std::fs::create_dir_all(dir.join(".venv/bin")).unwrap();
        std::fs::write(dir.join(".venv/bin/dbt"), "").unwrap();
        assert_eq!(dbt_candidates(&dir)[0], dir.join(".venv/bin/dbt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aws_profiles_come_from_the_config_file() {
        let cfg = "[default]\nregion=x\n[profile data-dev]\nsso_region = a\n[profile data-prod]\n[sso-session s]\n[profile  spaced ]\n";
        assert_eq!(aws_profiles(cfg), vec!["default", "data-dev", "data-prod", "spaced"]);
    }

    #[test]
    fn expired_aws_sso_is_detected_in_dbt_output() {
        assert!(needs_aws_login(
            "Runtime Error\n  The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile."
        ));
        assert!(needs_aws_login("Error when retrieving token from sso: Token has expired and refresh failed"));
        assert!(!needs_aws_login("The profile 'dev' does not have a target named 'prod'."));
    }

    #[test]
    fn dbt_config_aws_profile_is_optional_in_toml() {
        let c: DbtConfig = toml::from_str("project_dir = \"~/p\"\ntarget = \"dev\"\n").unwrap();
        assert_eq!(c.aws_profile, None);
        let c2 = DbtConfig { aws_profile: Some("data-prod".into()), ..c };
        assert!(toml::to_string(&c2).unwrap().contains("aws_profile = \"data-prod\""));
    }

    #[test]
    fn meta_is_flattened_and_deduplicated() {
        let m = parse_manifest(
            r#"{"nodes": {"model.p.a": {"unique_id": "model.p.a", "name": "a", "resource_type": "model",
            "config": {"meta": {"metaplane": {"createMonitors": {"monitors": ["ROW_COUNT"]}}, "slack_channel": "alerts", "atlan": {"attributes": {"ownerUsers": ["varushi.jain", "x.y"]}}}},
            "meta": {"metaplane": {"createMonitors": {"monitors": ["ROW_COUNT"]}}, "slack_channel": "alerts", "atlan": {"attributes": {"ownerUsers": ["varushi.jain", "x.y"]}}}}}}"#,
        )
        .unwrap();
        let n = &m.nodes["model.p.a"];
        assert_eq!(
            n.meta,
            vec![
                ("atlan.attributes.ownerUsers".to_string(), "varushi.jain, x.y".to_string()),
                ("metaplane.createMonitors.monitors".to_string(), "ROW_COUNT".to_string()),
                ("slack_channel".to_string(), "alerts".to_string()),
            ]
        );
    }

    #[test]
    fn blame_porcelain_reuses_commit_info_for_later_lines() {
        let out = "c7a7dc175f8b7d54c8df3ce3867158876b9c47eb 1 1 2\nauthor Ana Reis\nauthor-mail <ana@x>\nauthor-time 1758121550\nauthor-tz +0200\nsummary feat: working days model (#606)\nfilename m.sql\n\tWITH\nc7a7dc175f8b7d54c8df3ce3867158876b9c47eb 2 2\n\tsource AS (\n0000000000000000000000000000000000000000 3 3 1\nauthor Not Committed Yet\nauthor-time 1760000000\nsummary Version of m.sql from m.sql\nfilename m.sql\n\t  SELECT 1\n";
        let b = parse_blame(out);
        assert_eq!(b.len(), 3);
        assert_eq!((b[0].sha.as_str(), b[0].author.as_str(), b[0].time, b[0].text.as_str()), ("c7a7dc1", "Ana Reis", 1758121550, "WITH"));
        assert_eq!((b[1].sha.as_str(), b[1].summary.as_str(), b[1].text.as_str()), ("c7a7dc1", "feat: working days model (#606)", "source AS ("));
        assert!(b[2].uncommitted && b[2].text == "  SELECT 1");
        let g = GitInfo { repo: Some("https://github.com/org/repo".into()), ..Default::default() };
        assert_eq!(commit_url(&g, "c7a7dc1").as_deref(), Some("https://github.com/org/repo/commit/c7a7dc1"));
    }

    #[test]
    fn compiled_sql_lives_under_target_compiled_project() {
        let p = compiled_path(Path::new("/p"), "model.acme_analytics.dim_x", "models/marts/dim_x.sql");
        assert_eq!(p, Some(PathBuf::from("/p/target/compiled/acme_analytics/models/marts/dim_x.sql")));
        assert_eq!(compiled_path(Path::new("/p"), "bad", "x.sql"), None);
        assert_eq!(compile_args("dim_x", "dev"), vec!["compile", "--select", "dim_x", "--target", "dev"]);
    }
}
