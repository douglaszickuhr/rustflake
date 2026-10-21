//! Table definition (Snowflake): header facts, columns, classification, masking and
//! column lineage, joined with dbt descriptions and tests.

use std::collections::HashMap;

use crate::lexer::{qualify, quote_always, sql_str};
use crate::org_config::OrgConfig;
use crate::snowflake::{Named, Rows, named};

fn fq(db: &str, schema: &str, table: &str) -> String {
    qualify(&[db, schema, table])
}

/// Always the INFORMATION_SCHEMA of the table's database (may not be the session's).
pub fn facts_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select table_type, row_count, bytes, created, last_altered, comment, is_iceberg, is_dynamic from {}.information_schema.tables where table_schema = {} and table_name = {}",
        quote_always(db),
        sql_str(schema),
        sql_str(table)
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub kind: String,
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    pub created: Option<String>,
    pub last_altered: Option<String>,
    pub comment: Option<String>,
    pub iceberg: bool,
    pub dynamic: bool,
}

pub fn parse_facts(names: &[String], rows: &Rows) -> Option<Facts> {
    let r = named(names, rows).next()?;
    Some(Facts {
        kind: r.text("table_type").unwrap_or_default(),
        rows: r.text("row_count").and_then(|v| v.parse().ok()),
        bytes: r.text("bytes").and_then(|v| v.parse().ok()),
        created: r.text("created"),
        last_altered: r.text("last_altered"),
        comment: r.text("comment"),
        iceberg: r.text("is_iceberg").as_deref() == Some("YES"),
        dynamic: r.text("is_dynamic").as_deref() == Some("YES"),
    })
}

/// Function or procedure, for `GET_DDL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routine {
    Function,
    Procedure,
}

/// `NAME(VARCHAR, NUMBER)` as `SHOW` lists it → `GET_DDL` of it, fully qualified.
pub fn routine_ddl_sql(kind: Routine, db: &str, schema: &str, signature: &str) -> String {
    let (name, args) = signature.split_once('(').map_or((signature, ")"), |(n, a)| (n, a));
    let what = match kind {
        Routine::Function => "function",
        Routine::Procedure => "procedure",
    };
    format!("select get_ddl('{what}', {})", sql_str(&format!("{}.{}({args}", qualify(&[db, schema]), quote_always(name))))
}

/// A `CALL` with the argument types as placeholders, to fill in (commented: it isn't runnable yet).
pub fn call_template(db: &str, schema: &str, signature: &str) -> String {
    let (name, args) = signature.split_once('(').map_or((signature, ""), |(n, a)| (n, a.trim_end_matches(')')));
    let holes: Vec<String> = args.split(',').map(str::trim).filter(|a| !a.is_empty()).map(|a| format!("/* {a} */")).collect();
    format!("-- CALL {}.{}({});", qualify(&[db, schema]), quote_always(name), holes.join(", "))
}

/// Where an Iceberg table's data lives (`SHOW ICEBERG TABLES`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IcebergInfo {
    pub catalog: Option<String>,
    pub external_volume: Option<String>,
    pub base_location: Option<String>,
    /// `iceberg_table_type`: `MANAGED` (Snowflake is the catalog) or `UNMANAGED` (an external catalog).
    pub managed: Option<bool>,
}

/// `LIKE` is a pattern (`_` matches any character): the parser keeps only the exact name.
pub fn iceberg_sql(db: &str, schema: &str, table: &str) -> String {
    format!("show iceberg tables like {} in schema {}", sql_str(table), qualify(&[db, schema]))
}

pub fn parse_iceberg(names: &[String], rows: &Rows, table: &str) -> Option<IcebergInfo> {
    let r = named(names, rows).find(|r| r.text("name").as_deref() == Some(table))?;
    // Snowflake writes placeholders like `<invalid>` where a field doesn't apply (a table whose
    // external catalog vends the storage access has no volume): treat them as absent.
    let real = |c: &str| r.text(c).filter(|v| !v.trim().is_empty() && !(v.starts_with('<') && v.ends_with('>')));
    Some(IcebergInfo {
        catalog: real("catalog_name"),
        external_volume: real("external_volume_name"),
        base_location: real("base_location"),
        managed: real("iceberg_table_type").map(|t| t.eq_ignore_ascii_case("MANAGED")),
    })
}

/// Refresh status of a dynamic table (`INFORMATION_SCHEMA.DYNAMIC_TABLES()`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DynamicInfo {
    /// None when the lag is `DOWNSTREAM` (refreshed only when something downstream needs it).
    pub target_lag_sec: Option<u64>,
    pub mean_lag_sec: Option<u64>,
    pub max_lag_sec: Option<u64>,
    /// Share of time the data was within the target lag, 0..1.
    pub within_target: Option<f64>,
    /// When the data was last made current (timestamp text).
    pub latest_data: Option<String>,
    pub last_state: Option<String>,
    pub last_message: Option<String>,
    /// `ACTIVE`, `SUSPENDED`…
    pub scheduling: Option<String>,
    /// Snowflake's advice on how to refresh it cheaper (texts).
    pub recommendations: Vec<String>,
}

pub fn dynamic_sql(db: &str, schema: &str, table: &str) -> String {
    format!("select * from table({}.information_schema.dynamic_tables(name => {}))", quote_always(db), sql_str(&qualify(&[db, schema, table])))
}

pub fn parse_dynamic(names: &[String], rows: &Rows) -> Option<DynamicInfo> {
    let r = named(names, rows).next()?;
    let secs = |c: &str| r.text(c).and_then(|v| v.parse::<f64>().ok()).map(|v| v as u64);
    let json = |c: &str| r.text(c).and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok());
    Some(DynamicInfo {
        target_lag_sec: (r.text("target_lag_type").as_deref() != Some("DOWNSTREAM")).then(|| secs("target_lag_sec")).flatten(),
        mean_lag_sec: secs("mean_lag_sec"),
        max_lag_sec: secs("maximum_lag_sec"),
        within_target: r.text("time_within_target_lag_ratio").and_then(|v| v.parse().ok()),
        latest_data: r.text("latest_data_timestamp"),
        last_state: r.text("last_completed_refresh_state"),
        last_message: r.text("last_completed_refresh_state_message"),
        // `{"state": "ACTIVE"}` (JSON) or plain text.
        scheduling: json("scheduling_state").and_then(|v| v["state"].as_str().map(String::from)).or_else(|| r.text("scheduling_state")),
        recommendations: json("recommendations").and_then(|v| v["recommendations"].as_array().map(|a| a.iter().filter_map(|x| x["info"].as_str().map(String::from)).collect())).unwrap_or_default(),
    })
}

/// How a dynamic table is doing, for the badge colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynState {
    OnTime,
    /// The data is older than the target lag.
    Behind,
    Failed,
    Suspended,
}

/// `now` and `latest` in Unix seconds.
pub fn dynamic_state(d: &DynamicInfo, now: i64, latest: Option<i64>) -> DynState {
    if matches!(d.last_state.as_deref(), Some("FAILED" | "UPSTREAM_FAILED")) {
        return DynState::Failed;
    }
    if d.scheduling.as_deref().is_some_and(|s| s != "ACTIVE") {
        return DynState::Suspended;
    }
    match (d.target_lag_sec, latest) {
        (Some(target), Some(at)) if now - at > target as i64 => DynState::Behind,
        _ => DynState::OnTime,
    }
}

pub fn columns_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select ordinal_position, column_name, data_type, is_nullable, column_default, comment from {}.information_schema.columns where table_schema = {} and table_name = {} order by ordinal_position",
        quote_always(db),
        sql_str(schema),
        sql_str(table)
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColInfo {
    /// `ordinal_position`, 1..n.
    pub position: usize,
    pub name: String,
    pub kind: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

pub fn parse_columns(names: &[String], rows: &Rows) -> Vec<ColInfo> {
    named(names, rows)
        .filter_map(|r| {
            Some(ColInfo {
                position: r.get("ordinal_position").and_then(|v| v.parse().ok()).unwrap_or_default(),
                name: r.text("column_name")?,
                kind: r.text("data_type").unwrap_or_default(),
                nullable: r.get("is_nullable").is_some_and(|v| v.eq_ignore_ascii_case("YES")),
                default: r.text("column_default"),
                comment: r.text("comment"),
            })
        })
        .collect()
}

/// Column tags, live (INFORMATION_SCHEMA function; ACCOUNT_USAGE lags hours).
pub fn tags_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        // `*`: apply_method only exists on newer accounts; missing = no "assigned by".
        "select * from table({}.information_schema.tag_references_all_columns({}, 'table'))",
        quote_always(db),
        sql_str(&fq(db, schema, table))
    )
}

/// The row belongs to the configured classification tag (all 3 parts).
fn is_class(org: &OrgConfig, r: Named) -> bool {
    match (r.get("tag_database"), r.get("tag_schema"), r.get("tag_name")) {
        (Some(d), Some(s), Some(n)) => org.is_class_tag(d, s, n),
        _ => false,
    }
}

/// Column → (C level, tag) — only the configured classification tag.
pub fn parse_class_tags(org: &OrgConfig, names: &[String], rows: &Rows) -> HashMap<String, (u8, String)> {
    named(names, rows)
        .filter(|r| is_class(org, *r))
        .filter_map(|r| {
            let level = org.tag_level(r.get("tag_value")?)?;
            let tag = format!("{}.{}.{}", r.text("tag_database")?, r.text("tag_schema")?, r.text("tag_name")?);
            Some((r.text("column_name")?, (level, tag)))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColTag {
    pub name: String,
    pub value: String,
    pub database: String,
    pub schema: String,
    /// MANUAL, PROPAGATED, CLASSIFIED, INHERITED… (`apply_method`), when the account has it.
    pub method: Option<String>,
    /// Where it is set: COLUMN, or TABLE/SCHEMA/DATABASE when inherited (`level`).
    pub level: Option<String>,
}

impl ColTag {
    /// Set on the column itself (by hand, propagation or classification): what Snowsight lists.
    pub fn on_column(&self) -> bool {
        self.level.as_deref().is_none_or(|l| l.eq_ignore_ascii_case("COLUMN"))
    }
}

/// Every tag of each column, sorted by name like Snowsight's Tags column.
pub fn parse_all_tags(names: &[String], rows: &Rows) -> HashMap<String, Vec<ColTag>> {
    let mut m: HashMap<String, Vec<ColTag>> = HashMap::new();
    for r in named(names, rows) {
        let (Some(col), Some(name)) = (r.text("column_name"), r.text("tag_name")) else { continue };
        let s = |k| r.text(k).unwrap_or_default();
        m.entry(col).or_default().push(ColTag { name, value: s("tag_value"), database: s("tag_database"), schema: s("tag_schema"), method: r.text("apply_method"), level: r.text("level") });
    }
    m.values_mut().for_each(|v| v.sort());
    m
}

pub fn policies_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select ref_column_name, policy_name, policy_db, policy_schema, policy_kind from table({}.information_schema.policy_references(ref_entity_name => {}, ref_entity_domain => 'table'))",
        quote_always(db),
        sql_str(&fq(db, schema, table))
    )
}

/// Column → masking policy (including those attached by tag).
pub fn parse_masking(names: &[String], rows: &Rows) -> HashMap<String, String> {
    named(names, rows)
        .filter(|r| r.get("policy_kind") == Some("MASKING_POLICY"))
        .filter_map(|r| Some((r.text("ref_column_name")?, format!("{}.{}.{}", r.text("policy_db")?, r.text("policy_schema")?, r.text("policy_name")?))))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescSource {
    Dbt,
    Comment,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DefRow {
    pub col: ColInfo,
    pub desc: Option<String>,
    pub source: DescSource,
    pub level: Option<u8>,
    pub tag: Option<String>,
    pub masked_by: Option<String>,
    pub tests: Vec<String>,
}

/// Snowflake columns rule; dbt comes in through the description (preferred) and the tests.
pub fn merge(cols: &[ColInfo], dbt_cols: &[(String, String)], tags: &HashMap<String, (u8, String)>, masking: &HashMap<String, String>, tests: &HashMap<String, Vec<String>>) -> Vec<DefRow> {
    let up = |m: &HashMap<String, Vec<String>>| m.iter().map(|(k, v)| (k.to_uppercase(), v.clone())).collect::<HashMap<_, _>>();
    let tests = up(tests);
    cols.iter()
        .map(|c| {
            let key = c.name.to_uppercase();
            let dbt = dbt_cols.iter().find(|(n, _)| n.eq_ignore_ascii_case(&c.name)).map(|(_, d)| d.trim()).filter(|d| !d.is_empty());
            let (desc, source) = match (dbt, &c.comment) {
                (Some(d), _) => (Some(d.to_string()), DescSource::Dbt),
                (None, Some(cm)) => (Some(cm.clone()), DescSource::Comment),
                (None, None) => (None, DescSource::None),
            };
            let tag = tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(&c.name)).map(|(_, v)| v.clone());
            DefRow {
                col: c.clone(),
                desc,
                source,
                level: tag.as_ref().map(|t| t.0),
                tag: tag.map(|t| t.1),
                masked_by: masking.iter().find(|(k, _)| k.eq_ignore_ascii_case(&c.name)).map(|(_, v)| v.clone()),
                tests: tests.get(&key).cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// Readable test name: drops the package and the "expect_column_values_to_be_" style prefixes,
/// `_` becomes a space ("dbt_expectations.expect_column_values_to_be_unique" → "unique").
pub fn short_test_name(kind: &str) -> String {
    let base = kind.rsplit('.').next().unwrap_or(kind);
    let mut s = base;
    for p in ["expect_column_values_to_be_", "expect_column_values_to_", "expect_column_value_", "expect_column_to_", "expect_column_", "expect_"] {
        if let Some(rest) = s.strip_prefix(p).filter(|r| !r.is_empty()) {
            s = rest;
            break;
        }
    }
    s.replace('_', " ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefSort {
    #[default]
    Position,
    Name,
    Type,
    Description,
    Tags,
    Policy,
}

/// Definition rows to show: `query` matches (any word, case-insensitive) anything on screen or in the
/// panel (name, type, description, comment, tags, policy, tests); then sorted, empty values last.
pub fn def_view(rows: Vec<(DefRow, Vec<ColTag>)>, query: &str, sort: DefSort, asc: bool) -> Vec<(DefRow, Vec<ColTag>)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut out: Vec<_> = rows
        .into_iter()
        .filter(|(r, tags)| {
            let mut hay =
                format!("{} {} {} {} {} {}", r.col.name, r.col.kind, r.desc.as_deref().unwrap_or(""), r.col.comment.as_deref().unwrap_or(""), r.masked_by.as_deref().unwrap_or(""), r.tests.join(" "));
            for t in tags {
                hay.push_str(&format!(" {} {}", t.name, t.value));
            }
            let hay = hay.to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect();
    let key = |(r, tags): &(DefRow, Vec<ColTag>)| -> Option<String> {
        match sort {
            DefSort::Position => Some(format!("{:09}", r.col.position)),
            DefSort::Name => Some(r.col.name.to_lowercase()),
            DefSort::Type => Some(r.col.kind.to_lowercase()),
            DefSort::Description => r.desc.as_ref().map(|d| d.to_lowercase()),
            DefSort::Tags => tags.iter().find(|t| t.on_column()).map(|t| format!("{} {}", t.name, t.value).to_lowercase()),
            DefSort::Policy => r.masked_by.as_ref().map(|p| p.rsplit('.').next().unwrap_or(p).to_lowercase()),
        }
    };
    out.sort_by(|a, b| match (key(a), key(b)) {
        (Some(x), Some(y)) if asc => x.cmp(&y),
        (Some(x), Some(y)) => y.cmp(&x),
        (x, y) => x.is_none().cmp(&y.is_none()),
    });
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub columns: usize,
    pub classified: usize,
    pub tests: usize,
    pub undocumented: usize,
}

pub fn summary(rows: &[DefRow]) -> Summary {
    Summary {
        columns: rows.len(),
        classified: rows.iter().filter(|r| r.level.is_some_and(|l| l > 1)).count(),
        tests: rows.iter().map(|r| r.tests.len()).sum(),
        undocumented: rows.iter().filter(|r| r.source == DescSource::None).count(),
    }
}

pub fn column_lineage_sql(db: &str, schema: &str, table: &str, column: &str) -> String {
    format!("select * from table(snowflake.core.get_lineage({}, 'COLUMN', 'UPSTREAM', 5))", sql_str(&format!("{}.{}", fq(db, schema, table), quote_always(column))))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinNode {
    pub db: String,
    pub schema: String,
    pub object: String,
    pub column: String,
    pub distance: u32,
}

/// From the farthest source to the nearest; no repeats and no edge from the column to itself
/// (MERGE incremental).
pub fn parse_column_lineage(names: &[String], rows: &Rows) -> Vec<LinNode> {
    let mut out: Vec<LinNode> = Vec::new();
    for r in named(names, rows) {
        let (Some(object), Some(column)) = (r.text("source_object_name"), r.text("source_column_name")) else { continue };
        let (db, schema) = (r.text("source_object_database").unwrap_or_default(), r.text("source_object_schema").unwrap_or_default());
        // Self-edge only if it is truly the same object (database and schema too).
        let same = |c: &str, v: &str| r.text(c).is_none_or(|x| x == v);
        if same("target_object_database", &db)
            && same("target_object_schema", &schema)
            && Some(&object) == r.text("target_object_name").as_ref()
            && Some(&column) == r.text("target_column_name").as_ref()
        {
            continue;
        }
        let distance = r.text("distance").and_then(|d| d.parse().ok()).unwrap_or(0);
        if out.iter().any(|n| n.db == db && n.schema == schema && n.object == object && n.column == column) {
            continue;
        }
        out.push(LinNode { db, schema, object, column, distance });
    }
    out.sort_by_key(|n| std::cmp::Reverse(n.distance));
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn routines_get_ddl_and_a_call_template_from_their_signature() {
        assert_eq!(routine_ddl_sql(Routine::Procedure, "DB", "S", "LOAD(VARCHAR, NUMBER)"), r#"select get_ddl('procedure', '"DB"."S"."LOAD"(VARCHAR, NUMBER)')"#);
        assert_eq!(routine_ddl_sql(Routine::Function, "DB", "S", "F()"), r#"select get_ddl('function', '"DB"."S"."F"()')"#);
        assert_eq!(call_template("DB", "S", "LOAD(VARCHAR, NUMBER)"), r#"-- CALL "DB"."S"."LOAD"(/* VARCHAR */, /* NUMBER */);"#);
        assert_eq!(call_template("DB", "S", "NOW()"), r#"-- CALL "DB"."S"."NOW"();"#);
    }

    #[test]
    fn dynamic_tables_read_lag_state_and_advice() {
        let (n, r) = t(
            &[
                "TARGET_LAG_SEC",
                "TARGET_LAG_TYPE",
                "MEAN_LAG_SEC",
                "MAXIMUM_LAG_SEC",
                "TIME_WITHIN_TARGET_LAG_RATIO",
                "LATEST_DATA_TIMESTAMP",
                "LAST_COMPLETED_REFRESH_STATE",
                "SCHEDULING_STATE",
                "RECOMMENDATIONS",
            ],
            &[&[
                Some("3600"),
                Some("USER_DEFINED"),
                Some("1648"),
                Some("4303"),
                Some("0.998"),
                Some("2026-10-21 15:54:10.755000+00:00"),
                Some("SUCCEEDED"),
                Some("{\n  \"state\": \"ACTIVE\"\n}"),
                Some(r#"{"recommendations":[{"code":"X","info":"Do Y"}]}"#),
            ]],
        );
        let d = parse_dynamic(&n, &r).unwrap();
        assert_eq!((d.target_lag_sec, d.mean_lag_sec, d.scheduling.as_deref(), d.recommendations.as_slice()), (Some(3600), Some(1648), Some("ACTIVE"), &["Do Y".to_string()][..]));
        assert_eq!(dynamic_state(&d, 10_000, Some(9_000)), DynState::OnTime);
        assert_eq!(dynamic_state(&d, 10_000, Some(5_000)), DynState::Behind, "data older than the 1 h target");
        let failed = DynamicInfo { last_state: Some("UPSTREAM_FAILED".into()), ..d.clone() };
        assert_eq!(dynamic_state(&failed, 10_000, Some(9_000)), DynState::Failed);
        let paused = DynamicInfo { scheduling: Some("SUSPENDED".into()), ..d.clone() };
        assert_eq!(dynamic_state(&paused, 10_000, Some(9_000)), DynState::Suspended);
        let (n, r) = t(&["TARGET_LAG_SEC", "TARGET_LAG_TYPE"], &[&[Some("0"), Some("DOWNSTREAM")]]);
        assert_eq!(parse_dynamic(&n, &r).unwrap().target_lag_sec, None, "DOWNSTREAM has no lag to be behind");
    }

    #[test]
    fn iceberg_keeps_only_the_exact_name_of_the_like_pattern() {
        let (n, r) = t(
            &["name", "catalog_name", "external_volume_name", "base_location", "iceberg_table_type"],
            &[&[Some("AXB"), Some("C1"), None, None, None], &[Some("A_B"), Some("SNOWFLAKE"), Some("VOL"), Some("raw/a_b"), Some("MANAGED")]],
        );
        let i = parse_iceberg(&n, &r, "A_B").unwrap();
        assert_eq!((i.catalog.as_deref(), i.external_volume.as_deref(), i.base_location.as_deref(), i.managed), (Some("SNOWFLAKE"), Some("VOL"), Some("raw/a_b"), Some(true)));
        // An external catalog's table: Snowflake's `<invalid>` placeholder is no volume at all.
        let (n, r) = t(&["name", "catalog_name", "external_volume_name", "iceberg_table_type"], &[&[Some("S"), Some("ATLAN_CATALOG_INTEGRATION"), Some("<invalid>"), Some("UNMANAGED")]]);
        let ext = parse_iceberg(&n, &r, "S").unwrap();
        assert_eq!((ext.external_volume, ext.managed), (None, Some(false)));
        assert!(iceberg_sql("RAW", "ATLAN ICEBERG", "o'b").ends_with("like 'o''b' in schema \"RAW\".\"ATLAN ICEBERG\""), "{}", iceberg_sql("RAW", "ATLAN ICEBERG", "o'b"));
    }

    use super::*;
    use std::collections::HashMap;

    fn t(names: &[&str], rows: &[&[Option<&str>]]) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        (names.iter().map(|s| s.to_string()).collect(), rows.iter().map(|r| r.iter().map(|v| v.map(String::from)).collect()).collect())
    }

    #[test]
    fn sql_targets_the_tables_own_database_and_escapes() {
        let f = facts_sql("My Db", "S", "o'b");
        assert!(f.contains("\"My Db\".information_schema.tables") && f.contains("table_name = 'o''b'"), "{f}");
        assert!(columns_sql("D", "S", "T").contains("order by ordinal_position"));
        let tg = tags_sql("D", "S", "T");
        assert!(tg.contains("\"D\".information_schema.tag_references_all_columns('\"D\".\"S\".\"T\"', 'table')"), "{tg}");
        assert!(policies_sql("D", "S", "T").contains("ref_entity_domain => 'table'"));
        assert!(column_lineage_sql("D", "S", "T", "C").contains("'COLUMN', 'UPSTREAM', 5"));
    }

    #[test]
    fn facts_parse() {
        let (n, r) = t(&["TABLE_TYPE", "ROW_COUNT", "BYTES", "LAST_ALTERED", "COMMENT"], &[&[Some("BASE TABLE"), Some("1240000"), Some("84200000"), Some("2026-10-16 08:00:00"), Some("Daily cost")]]);
        let f = parse_facts(&n, &r).unwrap();
        assert_eq!((f.kind.as_str(), f.rows, f.bytes, f.comment.as_deref()), ("BASE TABLE", Some(1_240_000), Some(84_200_000), Some("Daily cost")));
    }

    fn cols() -> Vec<ColInfo> {
        let c = |n: &str, comment: Option<&str>| ColInfo { position: 0, name: n.into(), kind: "VARCHAR".into(), nullable: true, default: None, comment: comment.map(String::from) };
        vec![c("USAGE_DATE", None), c("REGION", Some("Region code")), c("ACCOUNT_ID", Some("acct")), c("_LOADED_AT", None)]
    }

    #[test]
    fn merge_prefers_dbt_then_comment_and_ignores_dbt_only_columns() {
        let dbt = vec![("usage_date".to_string(), "Day of cost".to_string()), ("account_id".to_string(), "".to_string()), ("ghost".to_string(), "x".to_string())];
        let mut tags = HashMap::new();
        tags.insert("ACCOUNT_ID".to_string(), (3u8, "T".to_string()));
        let mut masks = HashMap::new();
        masks.insert("ACCOUNT_ID".to_string(), "P.MASK".to_string());
        let mut tests = HashMap::new();
        tests.insert("USAGE_DATE".to_string(), vec!["not_null".to_string()]);
        let rows = merge(&cols(), &dbt, &tags, &masks, &tests);
        assert_eq!(rows.len(), 4, "dbt-only column is left out");
        assert_eq!((rows[0].desc.as_deref(), rows[0].source), (Some("Day of cost"), DescSource::Dbt));
        assert_eq!((rows[1].desc.as_deref(), rows[1].source), (Some("Region code"), DescSource::Comment));
        assert_eq!((rows[2].desc.as_deref(), rows[2].source), (Some("acct"), DescSource::Comment), "empty dbt description falls back to the comment");
        assert_eq!((rows[2].level, rows[2].masked_by.as_deref()), (Some(3), Some("P.MASK")));
        assert_eq!(rows[3].source, DescSource::None);
        assert_eq!(rows[0].tests, vec!["not_null"]);
        assert_eq!(summary(&rows), Summary { columns: 4, classified: 1, tests: 1, undocumented: 1 });
    }

    #[test]
    fn only_the_classification_tag_and_masking_policies_count() {
        let (n, r) = t(
            &["COLUMN_NAME", "TAG_NAME", "TAG_VALUE", "TAG_DATABASE", "TAG_SCHEMA"],
            &[
                &[Some("A"), Some("DATA_CLASSIFICATION"), Some("C-3 Confidential"), Some("GOVERNANCE"), Some("TAGS")],
                &[Some("B"), Some("DATA_CLASSIFICATION"), Some("C-3 Confidential"), Some("OTHER"), Some("TAGS")],
            ],
        );
        let tg = parse_class_tags(&crate::org_config::acme(), &n, &r);
        assert_eq!(tg.len(), 1, "same name in another database is not the org's tag");
        assert_eq!(tg["A"], (3, "GOVERNANCE.TAGS.DATA_CLASSIFICATION".to_string()));
        assert!(parse_class_tags(&crate::org_config::OrgConfig::none(), &n, &r).is_empty());
        let (n, r) = t(
            &["REF_COLUMN_NAME", "POLICY_NAME", "POLICY_DB", "POLICY_SCHEMA", "POLICY_KIND"],
            &[&[Some("A"), Some("EMAIL_MASK"), Some("G"), Some("P"), Some("MASKING_POLICY")], &[None, Some("RAP"), Some("G"), Some("P"), Some("ROW_ACCESS_POLICY")]],
        );
        let m = parse_masking(&n, &r);
        assert_eq!(m.len(), 1);
        assert_eq!(m["A"], "G.P.EMAIL_MASK");
    }

    #[test]
    fn column_lineage_from_farthest_to_nearest_without_self_edges() {
        let (n, r) = t(
            &["SOURCE_OBJECT_DATABASE", "SOURCE_OBJECT_SCHEMA", "SOURCE_OBJECT_NAME", "SOURCE_COLUMN_NAME", "TARGET_OBJECT_NAME", "TARGET_COLUMN_NAME", "DISTANCE"],
            &[
                &[Some("STG"), Some("S"), Some("STG_A"), Some("EMAIL"), Some("INT_A"), Some("EMAIL"), Some("2")],
                &[Some("INT"), Some("I"), Some("INT_A"), Some("EMAIL"), Some("T"), Some("EMAIL"), Some("1")],
                &[Some("RAW"), Some("R"), Some("ACCOUNTS"), Some("OWNER_EMAIL"), Some("STG_A"), Some("EMAIL"), Some("3")],
                &[Some("M"), Some("A"), Some("T"), Some("EMAIL"), Some("T"), Some("EMAIL"), Some("1")],
                &[Some("INT"), Some("I"), Some("INT_A"), Some("EMAIL"), Some("T"), Some("EMAIL"), Some("1")],
            ],
        );
        let l = parse_column_lineage(&n, &r);
        let got: Vec<String> = l.iter().map(|x| format!("{}.{}", x.object, x.column)).collect();
        assert_eq!(got, vec!["ACCOUNTS.OWNER_EMAIL", "STG_A.EMAIL", "INT_A.EMAIL"]);
    }

    #[test]
    fn same_named_objects_in_other_databases_are_distinct() {
        let (n, r) = t(
            &[
                "SOURCE_OBJECT_DATABASE",
                "SOURCE_OBJECT_SCHEMA",
                "SOURCE_OBJECT_NAME",
                "SOURCE_COLUMN_NAME",
                "TARGET_OBJECT_DATABASE",
                "TARGET_OBJECT_SCHEMA",
                "TARGET_OBJECT_NAME",
                "TARGET_COLUMN_NAME",
                "DISTANCE",
            ],
            &[
                &[Some("RAW"), Some("S"), Some("ORDERS"), Some("ID"), Some("STG"), Some("S"), Some("ORDERS"), Some("ID"), Some("2")],
                &[Some("STG"), Some("S"), Some("ORDERS"), Some("ID"), Some("M"), Some("S"), Some("ORDERS"), Some("ID"), Some("1")],
            ],
        );
        let l = parse_column_lineage(&n, &r);
        assert_eq!(l.iter().map(|x| x.db.as_str()).collect::<Vec<_>>(), vec!["RAW", "STG"], "same name in another database is neither a self-edge nor a duplicate");
    }

    #[test]
    fn all_column_tags_are_kept_sorted_by_name() {
        let (n, r) = t(
            &["COLUMN_NAME", "TAG_NAME", "TAG_VALUE", "TAG_DATABASE", "TAG_SCHEMA"],
            &[
                &[Some("A"), Some("OWNER_TEAM"), Some("SALES"), Some("GOVERNANCE"), Some("TAGS")],
                &[Some("A"), Some("PII"), Some("False"), Some("GOVERNANCE"), Some("TAGS")],
                &[Some("B"), Some("DATA_CLASSIFICATION"), Some("C-2 Internal"), Some("GOVERNANCE"), Some("TAGS")],
            ],
        );
        let a = crate::org_config::acme();
        let all = parse_all_tags(&n, &r);
        let nv = |c: &str| all[c].iter().map(|t| (t.name.as_str(), t.value.as_str())).collect::<Vec<_>>();
        assert_eq!(nv("A"), vec![("OWNER_TEAM", "SALES"), ("PII", "False")]);
        assert_eq!(nv("B"), vec![("DATA_CLASSIFICATION", "C-2 Internal")], "classification is a tag like any other");
        assert_eq!((all["B"][0].database.as_str(), all["B"][0].method.as_deref()), ("GOVERNANCE", None), "no apply_method column = None");
        assert!(all["B"][0].on_column(), "no level column = counts as the column's own");
        let (n, r) = t(
            &["COLUMN_NAME", "TAG_NAME", "TAG_VALUE", "LEVEL"],
            &[&[Some("A"), Some("PWI"), Some("False"), Some("SCHEMA")], &[Some("A"), Some("SECURITY_FIELD_CATEGORY"), Some("CONFLICT"), Some("COLUMN")]],
        );
        let own: Vec<_> = parse_all_tags(&n, &r)["A"].iter().filter(|t| t.on_column()).map(|t| t.name.clone()).collect();
        assert_eq!(own, vec!["SECURITY_FIELD_CATEGORY"], "inherited from schema/table is not the column's");
        assert!(!parse_class_tags(&a, &n, &r).contains_key("A"), "the other tags are not classification");
    }

    #[test]
    fn def_view_searches_everything_and_sorts_empty_last() {
        let row = |pos, name: &str, desc: Option<&str>, policy: Option<&str>| DefRow {
            col: ColInfo { position: pos, name: name.into(), kind: "TEXT".into(), nullable: true, default: None, comment: None },
            desc: desc.map(String::from),
            source: if desc.is_some() { DescSource::Dbt } else { DescSource::None },
            level: None,
            tag: None,
            masked_by: policy.map(String::from),
            tests: vec![],
        };
        let tag = |n: &str, v: &str| ColTag { name: n.into(), value: v.into(), database: "D".into(), schema: "S".into(), method: None, level: Some("COLUMN".into()) };
        let rows = vec![
            (row(2, "B_COL", Some("billing city"), Some("DB.SC.MASK_X")), vec![tag("SECURITY_FIELD_CATEGORY", "CONFLICT")]),
            (row(1, "A_COL", None, None), vec![]),
            (row(3, "C_COL", Some("country"), None), vec![]),
        ];
        let names = |v: Vec<(DefRow, Vec<ColTag>)>| v.into_iter().map(|(r, _)| r.col.name).collect::<Vec<_>>();
        assert_eq!(names(def_view(rows.clone(), "", DefSort::Position, true)), ["A_COL", "B_COL", "C_COL"]);
        assert_eq!(names(def_view(rows.clone(), "", DefSort::Position, false)), ["C_COL", "B_COL", "A_COL"]);
        assert_eq!(names(def_view(rows.clone(), "conflict", DefSort::Position, true)), ["B_COL"], "tag value");
        assert_eq!(names(def_view(rows.clone(), "mask_x", DefSort::Position, true)), ["B_COL"], "policy");
        assert_eq!(names(def_view(rows.clone(), "COUNTRY c_col", DefSort::Position, true)), ["C_COL"], "all words, any case");
        assert_eq!(names(def_view(rows.clone(), "", DefSort::Description, true)), ["B_COL", "C_COL", "A_COL"], "no description last");
        assert_eq!(names(def_view(rows.clone(), "", DefSort::Description, false)), ["C_COL", "B_COL", "A_COL"], "still last when descending");
        assert_eq!(names(def_view(rows, "", DefSort::Policy, true))[0], "B_COL");
    }

    #[test]
    fn test_names_are_shortened() {
        assert_eq!(short_test_name("dbt_expectations.expect_column_values_to_be_unique"), "unique");
        assert_eq!(short_test_name("expect_column_values_to_match_regex"), "match regex");
        assert_eq!(short_test_name("not_null"), "not null");
        assert_eq!(short_test_name("accepted_values"), "accepted values");
        assert_eq!(short_test_name("expect_"), "expect ", "nothing left after the prefix: keep it");
    }
}
