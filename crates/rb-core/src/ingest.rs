//! Snowpipe pipes (status, recent loads) and stages (location, files).

use crate::lexer::{qualify, sql_str};
use crate::snowflake::{Rows, named};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PipeInfo {
    pub db: String,
    pub schema: String,
    pub name: String,
    pub definition: String,
    pub owner: Option<String>,
    pub comment: Option<String>,
    /// Auto-ingest pipes have a notification channel (an SQS queue / event grid).
    pub auto_ingest: bool,
    /// Why Snowflake considers the pipe broken, if it does.
    pub invalid_reason: Option<String>,
}

pub fn pipes_in_schema_sql(db: &str, schema: &str) -> String {
    format!("show pipes in schema {}", qualify(&[db, schema]))
}

fn some(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

pub fn parse_pipes(names: &[String], rows: &Rows) -> Vec<PipeInfo> {
    named(names, rows)
        .filter_map(|r| {
            Some(PipeInfo {
                db: r.text("database_name")?,
                schema: r.text("schema_name")?,
                name: r.text("name")?,
                definition: r.text("definition").unwrap_or_default(),
                owner: some(r.text("owner")),
                comment: some(r.text("comment")),
                auto_ingest: some(r.text("notification_channel")).is_some(),
                invalid_reason: some(r.text("invalid_reason")),
            })
        })
        .collect()
}

/// `NAME`, `S.NAME` or `DB.S.NAME` (quotes dropped) to `DB.SCHEMA.NAME`, missing parts from the pipe's own.
fn qualify_in(p: &PipeInfo, raw: &str) -> String {
    let parts: Vec<String> = raw.split('.').map(|s| s.trim().trim_matches('"').to_string()).collect();
    match parts.as_slice() {
        [t] => format!("{}.{}.{t}", p.db, p.schema),
        [s, t] => format!("{}.{s}.{t}", p.db),
        _ => parts.join("."),
    }
}

/// The definition with string literals and comments blanked out, so an `@` or `copy into` inside them isn't matched.
/// ponytail: regex over the code, not a parse. sqlparser can't read `@stage (PATTERN => …)`, the usual pipe form.
fn code_only(sql: &str) -> String {
    let mut out = sql.as_bytes().to_vec();
    for t in crate::lexer::tokenize(sql) {
        if matches!(t.kind, crate::lexer::TokKind::Str | crate::lexer::TokKind::DollarStr | crate::lexer::TokKind::Comment) {
            out[t.start..t.end].fill(b' ');
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// One- to three-part object name, quoted parts allowed.
const OBJ: &str = r#"((?:"[^"]+"|[^\s(./;)"@%~]+)(?:\s*\.\s*(?:"[^"]+"|[^\s(./;)"@]+)){0,2})"#;

/// The table a pipe loads into, `DB.SCHEMA.TABLE` (from `COPY INTO …`).
pub fn pipe_target(p: &PipeInfo) -> Option<String> {
    let re = regex::Regex::new(&format!("(?is)copy\\s+into\\s+{OBJ}")).ok()?;
    Some(qualify_in(p, re.captures(&code_only(&p.definition))?.get(1)?.as_str()))
}

/// The stage a pipe reads from, `DB.SCHEMA.STAGE` (the first `@stage` of its definition; `@~` and `@%t` aren't named stages).
pub fn pipe_stage(p: &PipeInfo) -> Option<String> {
    let re = regex::Regex::new(&format!("@{OBJ}")).ok()?;
    Some(qualify_in(p, re.captures(&code_only(&p.definition))?.get(1)?.as_str()))
}

/// `SYSTEM$PIPE_STATUS`, the parts worth showing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PipeStatus {
    /// `RUNNING`, `PAUSED`, `STOPPED_…`.
    pub state: String,
    pub pending_files: u64,
    pub last_ingested: Option<String>,
    pub last_file: Option<String>,
    pub error: Option<String>,
}

pub fn pipe_status_sql(db: &str, schema: &str, name: &str) -> String {
    format!("select system$pipe_status({}) as status", sql_str(&qualify(&[db, schema, name])))
}

pub fn parse_pipe_status(json: &str) -> Option<PipeStatus> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let s = |k: &str| v[k].as_str().map(String::from);
    Some(PipeStatus {
        state: s("executionState").unwrap_or_default(),
        pending_files: v["pendingFileCount"].as_u64().unwrap_or(0),
        last_ingested: s("lastIngestedTimestamp"),
        last_file: s("lastIngestedFilePath"),
        error: s("error").or_else(|| s("lastIngestedErrorMessage")),
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileLoad {
    pub file: String,
    /// `Loaded`, `Load failed`, `Partially loaded`.
    pub status: String,
    pub rows: Option<u64>,
    pub errors: Option<u64>,
    pub first_error: Option<String>,
    pub at: Option<String>,
}

/// Files the pipe loaded into `target` (`DB.SCHEMA.TABLE`) over the last `days` days; problems first.
pub fn loads_sql(target: &str, pipe: &str, days: u32) -> String {
    let db = target.split('.').next().unwrap_or_default();
    format!(
        "select file_name, status, row_count, error_count, first_error_message, last_load_time from table({}.information_schema.copy_history(\
         table_name => {}, start_time => dateadd('day', -{days}, current_timestamp()))) where pipe_name = {} \
         order by iff(status = 'Loaded', 1, 0), last_load_time desc limit 500",
        qualify(&[db]),
        sql_str(target),
        sql_str(pipe)
    )
}

pub fn parse_loads(names: &[String], rows: &Rows) -> Vec<FileLoad> {
    named(names, rows)
        .map(|r| FileLoad {
            file: r.text("file_name").unwrap_or_default(),
            status: r.text("status").unwrap_or_default(),
            rows: r.text("row_count").and_then(|v| v.parse().ok()),
            errors: r.text("error_count").and_then(|v| v.parse().ok()),
            first_error: some(r.text("first_error_message")),
            at: r.text("last_load_time"),
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageInfo {
    pub db: String,
    pub schema: String,
    pub name: String,
    /// `INTERNAL` or `EXTERNAL`.
    pub kind: String,
    pub url: Option<String>,
    pub cloud: Option<String>,
    pub region: Option<String>,
    pub storage_integration: Option<String>,
    pub directory: bool,
    pub owner: Option<String>,
    pub comment: Option<String>,
}

impl StageInfo {
    /// `@"DB"."S"."NAME"`, as SQL writes a stage.
    pub fn at(&self) -> String {
        format!("@{}", qualify(&[&self.db, &self.schema, &self.name]))
    }
}

pub fn stages_in_schema_sql(db: &str, schema: &str) -> String {
    format!("show stages in schema {}", qualify(&[db, schema]))
}

pub fn parse_stages(names: &[String], rows: &Rows) -> Vec<StageInfo> {
    named(names, rows)
        .filter_map(|r| {
            Some(StageInfo {
                db: r.text("database_name")?,
                schema: r.text("schema_name")?,
                name: r.text("name")?,
                kind: r.text("type").unwrap_or_default(),
                url: some(r.text("url")),
                cloud: some(r.text("cloud")),
                region: some(r.text("region")),
                storage_integration: some(r.text("storage_integration")),
                directory: r.text("directory_enabled").as_deref() == Some("Y"),
                owner: some(r.text("owner")),
                comment: some(r.text("comment")),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageFile {
    /// Path inside the stage (what `@stage/<path>` reads).
    pub path: String,
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: Option<i64>,
}

/// Files under `prefix` (empty = all). `LIST` has no limit: a prefix keeps a big bucket manageable.
pub fn list_sql(s: &StageInfo, prefix: &str) -> String {
    format!("list {}/{}", s.at(), prefix.trim().trim_start_matches('/'))
}

/// Newest first, at most `cap`. `LIST` names external files by full URL (`s3://bucket/dir/f`) and
/// internal ones as `stage_name/dir/f`; both become the path inside the stage.
pub fn parse_list(names: &[String], rows: &Rows, s: &StageInfo, cap: usize) -> (Vec<StageFile>, usize) {
    let base = s.url.as_deref().map(|u| u.trim_end_matches('/').to_string());
    let internal = format!("{}/", s.name.to_lowercase());
    let mut files: Vec<StageFile> = named(names, rows)
        .filter_map(|r| {
            let name = r.text("name")?;
            let path = match &base {
                Some(b) if name.starts_with(b.as_str()) => name[b.len()..].trim_start_matches('/').to_string(),
                _ if name.to_lowercase().starts_with(&internal) => name[internal.len()..].to_string(),
                _ => name,
            };
            let modified = r.text("last_modified").and_then(|m| chrono::DateTime::parse_from_rfc2822(&m).ok()).map(|t| t.timestamp());
            Some(StageFile { path, size: r.text("size").and_then(|v| v.parse().ok()), modified })
        })
        .collect();
    let total = files.len();
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files.truncate(cap);
    (files, total)
}

/// A query reading one file with the stage's own file format. Semi-structured files (Parquet, JSON, Avro,
/// ORC, XML) come as one `$1` column (asking for `$2` is an error); CSV gets its first five columns.
pub fn preview_sql(s: &StageInfo, path: &str) -> String {
    let lower = path.to_lowercase();
    let semi =
        [".parquet", ".json", ".jsonl", ".ndjson", ".avro", ".orc", ".xml"].iter().any(|e| lower.trim_end_matches(".gz").trim_end_matches(".snappy").ends_with(e) || lower.contains(&format!("{e}.")));
    let cols = if semi { "$1" } else { "$1, $2, $3, $4, $5" };
    format!("select {cols}\nfrom {}/{}\nlimit 100;", s.at(), path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe(def: &str) -> PipeInfo {
        PipeInfo { db: "RAW".into(), schema: "DATADOG".into(), name: "P".into(), definition: def.into(), ..Default::default() }
    }

    #[test]
    fn pipe_target_comes_from_copy_into() {
        assert_eq!(pipe_target(&pipe("COPY INTO RAW.DATADOG.RUM_ACTION FROM (SELECT 1 FROM @s)")).as_deref(), Some("RAW.DATADOG.RUM_ACTION"));
        assert_eq!(pipe_target(&pipe("copy into events from @s")).as_deref(), Some("RAW.DATADOG.events"), "the pipe's database and schema fill in");
        assert_eq!(pipe_target(&pipe("COPY INTO \"Raw\".\"My Schema\".\"t\"(a, b) FROM @s")).as_deref(), Some("Raw.My Schema.t"));
    }

    #[test]
    fn pipe_stage_comes_from_the_at_reference() {
        assert_eq!(pipe_stage(&pipe("COPY INTO t FROM (SELECT $1 FROM @RAW.DATADOG.STG/actions/) FILE_FORMAT = (TYPE = PARQUET)")).as_deref(), Some("RAW.DATADOG.STG"));
        assert_eq!(pipe_stage(&pipe("copy into t from @stg pattern = '.*'")).as_deref(), Some("RAW.DATADOG.stg"));
        assert_eq!(pipe_stage(&pipe("copy into t from @\"Other\".\"My Stage\"/x")).as_deref(), Some("RAW.Other.My Stage"));
        assert_eq!(pipe_stage(&pipe("copy into t from @%t")), None, "a table stage has no tab");
        let tricky = "-- copy into old_t from @old_stage\ncopy into t from (select $1, 'me@x.com' from @RAW.SANA.RAW_CIN_SANA (PATTERN => \".*a\\/.*\\.parquet$\"))";
        assert_eq!((pipe_target(&pipe(tricky)).as_deref(), pipe_stage(&pipe(tricky)).as_deref()), (Some("RAW.DATADOG.t"), Some("RAW.SANA.RAW_CIN_SANA")), "comments and strings are skipped");
    }

    #[test]
    fn pipe_status_json_is_read() {
        let st = parse_pipe_status(r#"{"executionState":"RUNNING","pendingFileCount":3,"lastIngestedTimestamp":"2026-10-21T20:31:14.003Z","lastIngestedFilePath":"a/b.parquet"}"#).unwrap();
        assert_eq!((st.state.as_str(), st.pending_files, st.last_file.as_deref(), st.error), ("RUNNING", 3, Some("a/b.parquet"), None));
    }

    #[test]
    fn stage_files_become_paths_inside_the_stage_newest_first() {
        let names: Vec<String> = ["name", "size", "last_modified"].map(String::from).into();
        let row = |n: &str, d: &str| vec![Some(n.to_string()), Some("10".to_string()), Some(d.to_string())];
        let ext = StageInfo { name: "EXT".into(), url: Some("s3://bucket/data/".into()), ..Default::default() };
        let rows = vec![row("s3://bucket/data/a/old.json", "Tue, 20 Oct 2026 10:00:00 GMT"), row("s3://bucket/data/a/new.json", "Wed, 21 Oct 2026 10:00:00 GMT")];
        let (files, total) = parse_list(&names, &rows, &ext, 10);
        assert_eq!((files[0].path.as_str(), files[1].path.as_str(), total), ("a/new.json", "a/old.json", 2));
        let int = StageInfo { name: "MY_STAGE".into(), ..Default::default() };
        let (files, _) = parse_list(&names, &[row("my_stage/x/f.csv", "Tue, 20 Oct 2026 10:00:00 GMT")], &int, 10);
        assert_eq!(files[0].path, "x/f.csv");
        let (capped, total) = parse_list(&names, &rows, &ext, 1);
        assert_eq!((capped.len(), total), (1, 2), "capped, but the total is known");
    }

    #[test]
    fn preview_reads_semi_structured_files_as_one_column() {
        let s = StageInfo { db: "RAW".into(), schema: "S".into(), name: "ST".into(), ..Default::default() };
        assert!(preview_sql(&s, "a/f.parquet").starts_with("select $1\n"));
        assert!(preview_sql(&s, "a/f.json.gz").starts_with("select $1\n"));
        assert!(preview_sql(&s, "a/f.snappy.parquet").starts_with("select $1\n"));
        assert!(preview_sql(&s, "a/f.csv").starts_with("select $1, $2"));
    }

    #[test]
    fn loads_query_runs_in_the_targets_database() {
        let q = loads_sql("RAW.DATADOG.RUM_ACTION", "P", 7);
        assert!(q.contains(r#"table("RAW".information_schema.copy_history("#) && q.contains("table_name => 'RAW.DATADOG.RUM_ACTION'") && q.contains("where pipe_name = 'P'"), "{q}");
    }
}
