//! Query history per connection: JSON Lines in `app_dir()/history/<id>.jsonl`.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DbError;
// Owner-only: the SQL may contain a password (`ALTER USER … PASSWORD`).
use crate::config::open_private;

pub const MAX_ENTRIES: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix ms.
    pub ts: i64,
    pub sql: String,
    pub ms: u64,
    /// Rows read (query) or affected (DML).
    pub rows: Option<u64>,
    pub error: Option<String>,
}

pub fn history_path(conn_id: &str) -> PathBuf {
    crate::config::app_dir().join("history").join(format!("{conn_id}.jsonl"))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn io(e: impl std::fmt::Display) -> DbError {
    DbError::Io(e.to_string())
}

pub fn append(path: &Path, e: &Entry) -> Result<(), DbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let line = serde_json::to_string(e).map_err(io)?;
    writeln!(open_private(path, true).map_err(io)?, "{line}").map_err(io)
}

/// Newest first. Invalid lines are ignored; with an invalid line or above
/// `MAX_ENTRIES`, the file is rewritten with only the most recent valid entries.
pub fn load(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let lines = text.lines().count();
    let mut all: Vec<Entry> = text.lines().filter_map(|l| serde_json::from_str(l).map_err(|e| tracing::warn!("{}: invalid line: {e}", path.display())).ok()).collect();
    if all.len() > MAX_ENTRIES || all.len() != lines {
        all.drain(..all.len().saturating_sub(MAX_ENTRIES));
        let tmp = path.with_extension("jsonl.tmp");
        let body: String = all.iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
        let res = open_private(&tmp, false).and_then(|mut f| f.write_all(body.as_bytes())).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = res {
            tracing::warn!("{}: {e}", path.display());
        }
    }
    all.reverse();
    all
}

/// "Contains", case-insensitive; the same SQL (after `trim`) appears once, in the newest entry.
pub fn search<'a>(entries: &'a [Entry], text: &str) -> Vec<&'a Entry> {
    let needle = text.trim().to_lowercase();
    let mut seen = HashSet::new();
    entries.iter().filter(|e| e.sql.to_lowercase().contains(&needle)).filter(|e| seen.insert(e.sql.trim())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rb-history-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("c1.jsonl")
    }

    fn e(ts: i64, sql: &str) -> Entry {
        Entry { ts, sql: sql.into(), ms: 3, rows: Some(1), error: None }
    }

    #[test]
    fn round_trip_keeps_multiline_sql_newest_first() {
        let p = tmp("rt");
        let weird = "select 'a''b', \"Ç\" -- 🚀\nfrom t\n\twhere x = 1";
        append(&p, &e(1, "select 1")).unwrap();
        append(&p, &e(2, weird)).unwrap();
        let got = load(&p);
        assert_eq!(got.iter().map(|x| x.sql.as_str()).collect::<Vec<_>>(), [weird, "select 1"]);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn corrupt_lines_are_skipped_and_excess_is_trimmed() {
        let p = tmp("trim");
        append(&p, &e(1, "a")).unwrap();
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{broken\n").unwrap();
        for i in 0..MAX_ENTRIES as i64 + 10 {
            append(&p, &e(i + 2, "b")).unwrap();
        }
        let got = load(&p);
        assert_eq!(got.len(), MAX_ENTRIES);
        assert_eq!(got[0].ts, MAX_ENTRIES as i64 + 11);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), MAX_ENTRIES, "file rewritten");
    }

    #[test]
    fn search_ignores_case_and_collapses_repeats() {
        let all = vec![e(5, "select * from Accounts "), e(4, "select * from Accounts"), e(3, "select 2"), e(2, "select * from accounts")];
        let hits: Vec<i64> = search(&all, "ACCOUNTS").iter().map(|x| x.ts).collect();
        assert_eq!(hits, [5, 2]);
        assert_eq!(search(&all, "").len(), 3);
    }
}
