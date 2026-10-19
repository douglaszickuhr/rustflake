//! Scope of the classification roles in tag-based masking policies. The rules (role names,
//! tag) come from the connection config; here live the types and the query.

use crate::DbError;
use crate::org_config::OrgConfig;
use crate::session::Session;
use crate::snowflake::named;

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
    Ok(named(&names, &rows).filter_map(|r| Some(TagCount { database: r.text("database")?, schema: r.text("schema")?, value: r.text("value")?, columns: r.get("columns")?.parse().ok()? })).collect())
}
