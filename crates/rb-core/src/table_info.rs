//! Table definition (Snowflake): header facts, columns, classification, masking and
//! column lineage, joined with dbt descriptions and tests.

use std::collections::HashMap;

use crate::admin::{quote_ident, sql_str};
use crate::org_config::OrgConfig;
use crate::snowflake::field;

type Rows = [Vec<Option<String>>];

fn opt(names: &[String], r: &[Option<String>], col: &str) -> Option<String> {
    field(names, r, col).filter(|v| !v.is_empty()).map(String::from)
}

fn fq(db: &str, schema: &str, table: &str) -> String {
    format!("{}.{}.{}", quote_ident(db), quote_ident(schema), quote_ident(table))
}

/// Always the INFORMATION_SCHEMA of the table's database (may not be the session's).
pub fn facts_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select table_type, row_count, bytes, last_altered, comment from {}.information_schema.tables where table_schema = {} and table_name = {}",
        quote_ident(db),
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
    let r = rows.first()?;
    Some(Facts {
        kind: opt(names, r, "table_type").unwrap_or_default(),
        rows: opt(names, r, "row_count").and_then(|v| v.parse().ok()),
        bytes: opt(names, r, "bytes").and_then(|v| v.parse().ok()),
        last_altered: opt(names, r, "last_altered"),
        comment: opt(names, r, "comment"),
    })
}

pub fn columns_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select ordinal_position, column_name, data_type, is_nullable, column_default, comment from {}.information_schema.columns where table_schema = {} and table_name = {} order by ordinal_position",
        quote_ident(db),
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
    rows.iter()
        .filter_map(|r| {
            Some(ColInfo {
                position: field(names, r, "ordinal_position").and_then(|v| v.parse().ok()).unwrap_or_default(),
                name: opt(names, r, "column_name")?,
                kind: opt(names, r, "data_type").unwrap_or_default(),
                nullable: field(names, r, "is_nullable").is_some_and(|v| v.eq_ignore_ascii_case("YES")),
                default: opt(names, r, "column_default"),
                comment: opt(names, r, "comment"),
            })
        })
        .collect()
}

/// Column tags, live (INFORMATION_SCHEMA function; ACCOUNT_USAGE lags hours).
pub fn tags_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        // `*`: apply_method only exists on newer accounts; missing = no "assigned by".
        "select * from table({}.information_schema.tag_references_all_columns({}, 'table'))",
        quote_ident(db),
        sql_str(&fq(db, schema, table))
    )
}

/// The row belongs to the configured classification tag (all 3 parts).
fn is_class(org: &OrgConfig, names: &[String], r: &[Option<String>]) -> bool {
    match (field(names, r, "tag_database"), field(names, r, "tag_schema"), field(names, r, "tag_name")) {
        (Some(d), Some(s), Some(n)) => org.is_class_tag(d, s, n),
        _ => false,
    }
}

/// Column → (C level, tag) — only the configured classification tag.
pub fn parse_class_tags(org: &OrgConfig, names: &[String], rows: &Rows) -> HashMap<String, (u8, String)> {
    rows.iter()
        .filter(|r| is_class(org, names, r))
        .filter_map(|r| {
            let level = org.tag_level(field(names, r, "tag_value")?)?;
            let tag = format!("{}.{}.{}", opt(names, r, "tag_database")?, opt(names, r, "tag_schema")?, opt(names, r, "tag_name")?);
            Some((opt(names, r, "column_name")?, (level, tag)))
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
    for r in rows {
        let (Some(col), Some(name)) = (opt(names, r, "column_name"), opt(names, r, "tag_name")) else { continue };
        let s = |k| opt(names, r, k).unwrap_or_default();
        m.entry(col).or_default().push(ColTag {
            name,
            value: s("tag_value"),
            database: s("tag_database"),
            schema: s("tag_schema"),
            method: opt(names, r, "apply_method"),
            level: opt(names, r, "level"),
        });
    }
    m.values_mut().for_each(|v| v.sort());
    m
}

pub fn policies_sql(db: &str, schema: &str, table: &str) -> String {
    format!(
        "select ref_column_name, policy_name, policy_db, policy_schema, policy_kind from table({}.information_schema.policy_references(ref_entity_name => {}, ref_entity_domain => 'table'))",
        quote_ident(db),
        sql_str(&fq(db, schema, table))
    )
}

/// Column → masking policy (including those attached by tag).
pub fn parse_masking(names: &[String], rows: &Rows) -> HashMap<String, String> {
    rows.iter()
        .filter(|r| field(names, r, "policy_kind") == Some("MASKING_POLICY"))
        .filter_map(|r| Some((opt(names, r, "ref_column_name")?, format!("{}.{}.{}", opt(names, r, "policy_db")?, opt(names, r, "policy_schema")?, opt(names, r, "policy_name")?))))
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
    format!("select * from table(snowflake.core.get_lineage({}, 'COLUMN', 'UPSTREAM', 5))", sql_str(&format!("{}.{}", fq(db, schema, table), quote_ident(column))))
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
    for r in rows {
        let (Some(object), Some(column)) = (opt(names, r, "source_object_name"), opt(names, r, "source_column_name")) else { continue };
        let (db, schema) = (opt(names, r, "source_object_database").unwrap_or_default(), opt(names, r, "source_object_schema").unwrap_or_default());
        // Self-edge only if it is truly the same object (database and schema too).
        let same = |c: &str, v: &str| opt(names, r, c).is_none_or(|x| x == v);
        if same("target_object_database", &db)
            && same("target_object_schema", &schema)
            && Some(&object) == opt(names, r, "target_object_name").as_ref()
            && Some(&column) == opt(names, r, "target_column_name").as_ref()
        {
            continue;
        }
        let distance = opt(names, r, "distance").and_then(|d| d.parse().ok()).unwrap_or(0);
        if out.iter().any(|n| n.db == db && n.schema == schema && n.object == object && n.column == column) {
            continue;
        }
        out.push(LinNode { db, schema, object, column, distance });
    }
    out.sort_by(|a, b| b.distance.cmp(&a.distance));
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
