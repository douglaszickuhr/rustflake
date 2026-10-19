//! Scope of the classification roles in tag-based masking policies. The rules (role names,
//! tag) come from the connection config; here live the types and the query.

use crate::DbError;
use crate::org_config::OrgConfig;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// `masking.global`: the whole account.
    Global,
    /// `masking.group`: the databases matching the config glob.
    Group,
    /// `<DB>` + `scoped_suffix`.
    Database(String),
    /// `<DB>_<SCHEMA>` + `scoped_suffix`.
    Schema(String, String),
    /// Matched no known database (or does not follow the pattern).
    Unresolved(String),
}

/// Columns with the classification tag, aggregated by schema and value.
#[derive(Debug, Clone, PartialEq)]
pub struct TagCount {
    pub database: String,
    pub schema: String,
    pub value: String,
    pub columns: u64,
}

/// No classification tag in the config: nothing to count.
pub async fn tag_counts(s: &Session, org: &OrgConfig) -> Result<Vec<TagCount>, DbError> {
    let Some(sql) = org.tag_counts_sql() else { return Ok(Vec::new()) };
    let (names, rows) = s.query_named(&sql).await?;
    let col = |r: &[Option<String>], c: &str| names.iter().position(|n| n.eq_ignore_ascii_case(c)).and_then(|i| r.get(i).cloned().flatten());
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(TagCount { database: col(r, "database")?, schema: col(r, "schema")?, value: col(r, "value")?, columns: col(r, "columns")?.parse().ok()? })
        })
        .collect())
}
