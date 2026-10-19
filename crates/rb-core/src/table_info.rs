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
        "select table_type, row_count, bytes, last_altered, comment from {}.information_schema.tables where table_schema = {} and table_name = {}",
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
    pub last_altered: Option<String>,
    pub comment: Option<String>,
}

pub fn parse_facts(names: &[String], rows: &Rows) -> Option<Facts> {
    let r = named(names, rows).next()?;
    Some(Facts {
        kind: r.text("table_type").unwrap_or_default(),
        rows: r.text("row_count").and_then(|v| v.parse().ok()),
        bytes: r.text("bytes").and_then(|v| v.parse().ok()),
        last_altered: r.text("last_altered"),
        comment: r.text("comment"),
    })
}

pub fn columns_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select column_name, data_type, is_nullable, column_default, comment from {}.information_schema.columns where table_schema = {} and table_name = {} order by ordinal_position",
        quote_always(db),
        sql_str(schema),
        sql_str(table)
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColInfo {
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
        "select column_name, tag_name, tag_value, tag_database, tag_schema from table({}.information_schema.tag_references_all_columns({}, 'table'))",
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

/// The column's other tags, for the panel; the classification one becomes the chip.
pub fn parse_all_tags(org: &OrgConfig, names: &[String], rows: &Rows) -> HashMap<String, Vec<(String, String)>> {
    let mut m: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for r in named(names, rows) {
        let (Some(col), Some(tag)) = (r.text("column_name"), r.text("tag_name")) else { continue };
        // A value outside `values` does not become a chip: it stays here so it doesn't vanish.
        if is_class(org, r) && r.get("tag_value").and_then(|v| org.tag_level(v)).is_some() {
            continue;
        }
        m.entry(col).or_default().push((tag, r.text("tag_value").unwrap_or_default()));
    }
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
        let c = |n: &str, comment: Option<&str>| ColInfo { name: n.into(), kind: "VARCHAR".into(), nullable: true, default: None, comment: comment.map(String::from) };
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
    fn all_column_tags_are_kept_for_the_panel() {
        let (n, r) = t(
            &["COLUMN_NAME", "TAG_NAME", "TAG_VALUE", "TAG_DATABASE", "TAG_SCHEMA"],
            &[
                &[Some("A"), Some("OWNER_TEAM"), Some("SALES"), Some("GOVERNANCE"), Some("TAGS")],
                &[Some("A"), Some("PII"), Some("False"), Some("GOVERNANCE"), Some("TAGS")],
                &[Some("B"), Some("DATA_CLASSIFICATION"), Some("C-2 Internal"), Some("GOVERNANCE"), Some("TAGS")],
            ],
        );
        let a = crate::org_config::acme();
        let all = parse_all_tags(&a, &n, &r);
        assert_eq!(all["A"], vec![("OWNER_TEAM".to_string(), "SALES".to_string()), ("PII".to_string(), "False".to_string())]);
        assert!(!all.contains_key("B"), "the classification one already shows in the chip");
        assert!(!parse_class_tags(&a, &n, &r).contains_key("A"), "the other tags are not classification");
        assert!(parse_all_tags(&crate::org_config::OrgConfig::none(), &n, &r).contains_key("B"), "without an org every tag is just a tag");
        let (n, r) = t(&["COLUMN_NAME", "TAG_NAME", "TAG_VALUE", "TAG_DATABASE", "TAG_SCHEMA"], &[&[Some("C"), Some("DATA_CLASSIFICATION"), Some("C-9 Secret"), Some("GOVERNANCE"), Some("TAGS")]]);
        assert_eq!(parse_all_tags(&a, &n, &r)["C"], vec![("DATA_CLASSIFICATION".to_string(), "C-9 Secret".to_string())], "unknown value stays visible");
    }
}
