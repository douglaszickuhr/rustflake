//! Quick search (⌘K): results, ordering, tree path and the Snowflake index.

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use crate::DbError;
use crate::lexer::quote_always;
use crate::metadata::{MetaCache, MetaRequest};
use crate::session::Session;
use crate::snowflake::{Rows, named};

const MAX_HITS: usize = 50;
/// Sessions listing databases at the same time when the account is too big for one `SHOW`.
const LANES: usize = 8;
/// Row cap of a `SHOW`; when it is hit, the listing may be truncated.
const SHOW_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum HitKind {
    Database,
    Schema,
    Table,
    View,
}

/// A found object. `matview` only applies with `View`: decides the tree group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub kind: HitKind,
    pub matview: bool,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
}

fn key(db: Option<&str>, schema: &str) -> String {
    match db {
        Some(db) => format!("{db}.{schema}"),
        None => schema.to_string(),
    }
}

impl Hit {
    /// Cache schema key: `DB.SCHEMA` on Snowflake, `SCHEMA` on Postgres.
    pub fn schema_key(&self) -> Option<String> {
        match self.kind {
            HitKind::Database => None,
            HitKind::Schema => Some(key(self.database.as_deref(), &self.name)),
            _ => self.schema.as_deref().map(|s| key(self.database.as_deref(), s)),
        }
    }

    /// Tree ids (each one in parts) from the root to the target, in the format of `schema_tree.rs`.
    pub fn tree_path(&self) -> Vec<Vec<String>> {
        let v = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if self.kind == HitKind::Database {
            return vec![v(&["d", &self.name])];
        }
        let mut path: Vec<Vec<String>> = self.database.iter().map(|db| v(&["d", db])).collect();
        let Some(s) = self.schema_key() else { return path };
        path.push(v(&["s", &s]));
        if self.kind != HitKind::Schema {
            let group = match (self.kind, self.matview) {
                (HitKind::Table, _) => "tables",
                (_, true) => "matviews",
                _ => "views",
            };
            path.push(v(&["g", &s, group]));
            path.push(v(&["r", &s, &self.name]));
        }
        path
    }
}

/// `%text%` for `LIKE`/`ILIKE` with `\` as escape: typed `_` and `%` count literally.
pub fn like_pattern(text: &str) -> String {
    format!("%{}%", text.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_"))
}

/// `DB.SCHEMA.NAME` (the parts that exist), as typed in SQL.
fn full_name(h: &Hit) -> String {
    match h.kind {
        HitKind::Database => h.name.clone(),
        HitKind::Schema => h.database.iter().chain([&h.name]).cloned().collect::<Vec<_>>().join("."),
        _ => h.database.iter().chain(&h.schema).chain([&h.name]).cloned().collect::<Vec<_>>().join("."),
    }
}

/// The hit is exactly what was typed (its name, or with dots its full or trailing path).
pub fn is_exact(h: &Hit, text: &str) -> bool {
    let q = text.replace('"', "").to_uppercase();
    let n = if q.contains('.') { full_name(h) } else { h.name.clone() }.to_uppercase();
    n == q || (q.contains('.') && n.ends_with(&format!(".{q}")))
}

/// Only those containing the text (case-insensitive); exact, prefix, contains; shorter first.
/// Text with dots (`RAW.ATLAN_ICEBERG.SCHEMA`, quotes allowed) is matched against the full path, and
/// a whole trailing path (`ATLAN_ICEBERG.SCHEMA`) counts as exact.
pub fn rank(hits: Vec<Hit>, text: &str) -> Vec<Hit> {
    let q = text.replace('"', "").to_uppercase();
    let dotted = q.contains('.');
    let mut scored: Vec<(u8, usize, Hit)> = hits
        .into_iter()
        .filter_map(|h| {
            let n = if dotted { full_name(&h) } else { h.name.clone() }.to_uppercase();
            let tier = if n == q || (dotted && n.ends_with(&format!(".{q}"))) {
                0
            } else if n.starts_with(&q) || (dotted && n.contains(&format!(".{q}"))) {
                1
            } else if n.contains(&q) {
                2
            } else {
                return None;
            };
            Some((tier, n.len(), h))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(MAX_HITS).map(|(_, _, h)| h).collect()
}

/// `SHOW TERSE OBJECTS`: tables and views (matview flagged).
pub fn parse_sf_objects(names: &[String], rows: &Rows) -> Vec<Hit> {
    named(names, rows)
        .filter_map(|r| {
            let kind = r.get("kind").unwrap_or_default();
            Some(Hit {
                kind: if kind.contains("VIEW") { HitKind::View } else { HitKind::Table },
                matview: kind.contains("MATERIALIZED"),
                database: Some(r.text("database_name")?),
                schema: Some(r.text("schema_name")?),
                name: r.text("name")?,
            })
        })
        .collect()
}

/// `SHOW TERSE SCHEMAS`.
pub fn parse_sf_schemas(names: &[String], rows: &Rows) -> Vec<Hit> {
    named(names, rows).filter_map(|r| Some(Hit { kind: HitKind::Schema, matview: false, database: Some(r.text("database_name")?), schema: None, name: r.text("name")? })).collect()
}

/// Tables, views and schemas of the account, as the current role sees them.
#[derive(Debug, Clone, Default)]
pub struct SfIndex {
    pub objects: Vec<Hit>,
    pub schemas: Vec<Hit>,
    pub refreshed: Option<Instant>,
    /// Per-database listing (the account-wide one hit the cap): which ones were included. Indexes of different
    /// scopes are not comparable.
    pub fallback_dbs: Option<Vec<String>>,
}

/// What changed between two indexes: schemas with new/removed/type-changed objects and
/// databases with new/removed schemas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    pub changed_schemas: BTreeSet<String>,
    pub changed_dbs: BTreeSet<String>,
    /// Schemas that no longer exist: they leave the cache instead of being reloaded.
    pub removed_schemas: BTreeSet<String>,
}

pub fn diff(old: &SfIndex, new: &SfIndex) -> IndexDiff {
    if old.fallback_dbs != new.fallback_dbs {
        return IndexDiff::default();
    }
    let set = |v: &[Hit]| v.iter().cloned().collect::<HashSet<_>>();
    let (oo, no, os, ns) = (set(&old.objects), set(&new.objects), set(&old.schemas), set(&new.schemas));
    IndexDiff {
        changed_schemas: oo.symmetric_difference(&no).filter_map(Hit::schema_key).collect(),
        changed_dbs: os.symmetric_difference(&ns).filter_map(|h| h.database.clone()).collect(),
        removed_schemas: os.difference(&ns).filter_map(Hit::schema_key).collect(),
    }
}

/// What to do to the tree cache with a diff: only touches what it already loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshPlan {
    /// Requests to redo on top of the cache (without clearing first: columns and the tree stay).
    pub reload: Vec<MetaRequest>,
    pub drop_schemas: Vec<String>,
    /// Database created or removed: the root list (`show databases`) changes.
    pub reload_roots: bool,
}

pub fn refresh_plan(d: &IndexDiff, cache: &MetaCache) -> RefreshPlan {
    let loaded = |s: &&String| cache.objects.contains_key(*s);
    let dbs = d.changed_dbs.iter().filter(|db| cache.loaded_dbs.contains(*db)).map(|db| MetaRequest::Schemas(db.clone()));
    let schemas = d.changed_schemas.iter().filter(loaded).filter(|s| !d.removed_schemas.contains(*s)).map(|s| MetaRequest::Objects(s.clone()));
    RefreshPlan { reload: dbs.chain(schemas).collect(), drop_schemas: d.removed_schemas.iter().filter(loaded).cloned().collect(), reload_roots: !d.changed_dbs.is_empty() }
}

/// Local search: visible databases (from the cache), schemas and index objects in the visible databases.
pub fn search_index(idx: &SfIndex, cache: &MetaCache, text: &str) -> Vec<Hit> {
    let visible: HashSet<&str> = cache.visible_databases().map(String::as_str).collect();
    let dbs = visible.iter().map(|d| Hit { kind: HitKind::Database, matview: false, database: None, schema: None, name: d.to_string() });
    let rest = idx.schemas.iter().chain(&idx.objects).filter(|h| h.database.as_deref().is_some_and(|d| visible.contains(d))).cloned();
    rank(dbs.chain(rest).collect(), text)
}

// Shorter first before the cut: the exact match and prefixes are not left out in a large catalog.
const PG_SEARCH: &str = "\
select * from (
select 'schema'::text, nspname::text, null::text from pg_namespace
 where nspname ilike $1 and nspname not like 'pg\\_%' and nspname <> 'information_schema'
union all
select c.relkind::text, n.nspname::text, c.relname::text from pg_class c join pg_namespace n on n.oid = c.relnamespace
 where c.relkind in ('r','p','v','m','f') and c.relname ilike $1
   and n.nspname not like 'pg\\_%' and n.nspname <> 'information_schema'
) hit(kind, nsp, rel)
order by length(coalesce(rel, nsp))
limit 200";

/// Postgres: live catalog search (local, milliseconds).
pub async fn pg_search(s: &Session, text: &str) -> Result<Vec<Hit>, DbError> {
    // `schema.table`: the catalog only knows names, so it looks for the last part and `rank` checks the path.
    let name = text.replace('"', "");
    let name = name.rsplit('.').next().unwrap_or_default();
    let rows = s.query_text(PG_SEARCH, &[&like_pattern(name)]).await?;
    let hits = rows
        .into_iter()
        .filter_map(|r| {
            let (kind, schema, rel) = (r.first()?.clone()?, r.get(1)?.clone()?, r.get(2).cloned().flatten());
            Some(match (kind.as_str(), rel) {
                ("schema", _) => Hit { kind: HitKind::Schema, matview: false, database: None, schema: None, name: schema },
                (k, Some(name)) => Hit { kind: if matches!(k, "v" | "m") { HitKind::View } else { HitKind::Table }, matview: k == "m", database: None, schema: Some(schema), name },
                _ => return None,
            })
        })
        .collect();
    Ok(rank(hits, text))
}

/// The index as saved between runs (one file per connection).
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedIndex {
    saved_at: i64,
    schemas: Vec<Hit>,
    objects: Vec<Hit>,
    fallback_dbs: Option<Vec<String>>,
}

pub fn index_path(conn_id: &str) -> std::path::PathBuf {
    crate::config::app_dir().join("index").join(format!("{conn_id}.json"))
}

/// Saves the index so the next launch can search right away (blocking: call it off the UI thread).
pub fn save_index(conn_id: &str, idx: &SfIndex) -> std::io::Result<()> {
    let saved = SavedIndex { saved_at: crate::user_view::now(), schemas: idx.schemas.clone(), objects: idx.objects.clone(), fallback_dbs: idx.fallback_dbs.clone() };
    let path = index_path(conn_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&saved).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

/// The index saved by a previous run, aged as old as it is (so it gets refreshed). None if absent or unreadable.
pub fn load_index(conn_id: &str) -> Option<SfIndex> {
    let saved: SavedIndex = serde_json::from_slice(&std::fs::read(index_path(conn_id)).ok()?).ok()?;
    let age = std::time::Duration::from_secs((crate::user_view::now() - saved.saved_at).max(0) as u64);
    Some(SfIndex { objects: saved.objects, schemas: saved.schemas, refreshed: Instant::now().checked_sub(age), fallback_dbs: saved.fallback_dbs })
}

/// `DB.SCHEMA[.NAME]` typed in full: asks Snowflake for just that (fast) instead of waiting for the
/// index. Other text gives an empty list. Unquoted parts match like Snowflake (case-insensitive).
pub async fn sf_lookup(s: &Session, text: &str) -> Result<Vec<Hit>, DbError> {
    let clean = text.replace('"', "");
    let parts: Vec<&str> = clean.split('.').map(str::trim).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Ok(Vec::new());
    }
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    match parts.as_slice() {
        [db, schema] => {
            let (n, rows) = s.query_named(&format!("show terse schemas like {} in database {}", crate::lexer::sql_str(schema), quote_always(&db.to_uppercase()))).await?;
            Ok(parse_sf_schemas(&n, &rows).into_iter().filter(|h| same(&h.name, schema)).collect())
        }
        [db, schema, name] => {
            let q = format!("show terse objects like {} in schema {}.{}", crate::lexer::sql_str(name), quote_always(&db.to_uppercase()), quote_always(&schema.to_uppercase()));
            let (n, rows) = s.query_named(&q).await?;
            Ok(parse_sf_objects(&n, &rows).into_iter().filter(|h| same(&h.name, name)).collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// Snowflake: schemas and objects of the account. List truncated at the `SHOW` cap → redo per database.
pub async fn sf_snapshot(s: &Session, dbs: &[String]) -> Result<SfIndex, DbError> {
    let started = Instant::now();
    let other = s.sibling();
    let (sch, objs) = tokio::join!(s.query_named("show terse schemas in account"), other.as_ref().unwrap_or(s).query_named("show terse objects in account"));
    let (n, rows) = sch?;
    let schemas = parse_sf_schemas(&n, &rows);
    let (n, rows) = objs?;
    tracing::info!("index: account-wide schemas + objects in {:?} ({} objects)", started.elapsed(), rows.len());
    if rows.len() < SHOW_LIMIT {
        return Ok(SfIndex { objects: parse_sf_objects(&n, &rows), schemas, refreshed: Some(Instant::now()), fallback_dbs: None });
    }
    // ponytail: no `FROM` pagination; a database with more than 10k objects ends up truncated.
    // Databases split over `LANES` sessions on the same login, each listing its share in turn.
    let lanes: Vec<Option<Session>> = (0..LANES).map(|i| if i == 0 { None } else { s.sibling() }).collect();
    let work = lanes.iter().enumerate().map(|(i, lane)| {
        let session = lane.as_ref().unwrap_or(s);
        let mine: Vec<&String> = dbs.iter().skip(i).step_by(LANES).collect();
        async move {
            let mut out = Vec::new();
            for db in mine {
                // A database that fails (removed midway, share without access) does not take down the whole index.
                match session.query_named(&format!("show terse objects in database {}", quote_always(db))).await {
                    Ok((n, rows)) => {
                        if rows.len() >= SHOW_LIMIT {
                            tracing::warn!(db, "more than 10k objects: index truncated");
                        }
                        out.extend(parse_sf_objects(&n, &rows));
                    }
                    Err(e) => tracing::warn!(db, "quick-open index: {e}"),
                }
            }
            out
        }
    });
    let objects: Vec<Hit> = futures::future::join_all(work).await.into_iter().flatten().collect();
    tracing::info!("index: {} databases over {LANES} sessions → {} objects, total {:?}", dbs.len(), objects.len(), started.elapsed());
    Ok(SfIndex { objects, schemas, refreshed: Some(Instant::now()), fallback_dbs: Some(dbs.to_vec()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(db: &str, s: &str, n: &str) -> Hit {
        Hit { kind: HitKind::Table, matview: false, database: Some(db.into()), schema: Some(s.into()), name: n.into() }
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("DEV_"), r"%DEV\_%");
        assert_eq!(like_pattern(r"a%b\c"), r"%a\%b\\c%");
    }

    #[test]
    fn rank_orders_exact_prefix_contains_then_length() {
        let hits = vec![t("D", "S", "XORDERS"), t("D", "S", "ORDERS_OLD"), t("D", "S", "ORDERS"), t("D", "S", "ORD"), t("D", "S", "CUSTOMERS")];
        let names: Vec<_> = rank(hits, "orders").into_iter().map(|h| h.name).collect();
        assert_eq!(names, ["ORDERS", "ORDERS_OLD", "XORDERS"]);
    }

    #[test]
    fn full_paths_find_the_exact_object_first() {
        let schema = Hit { kind: HitKind::Schema, matview: false, database: Some("RAW".into()), schema: None, name: "ATLAN_ICEBERG".into() };
        let hits = vec![t("RAW", "ATLAN_ICEBERG", "SCHEMA_HISTORY"), t("RAW", "OTHER", "SCHEMA"), t("RAW", "ATLAN_ICEBERG", "SCHEMA"), schema.clone()];
        let names = |q: &str| rank(hits.clone(), q).into_iter().map(|h| full_name(&h)).collect::<Vec<_>>();
        assert_eq!(names("RAW.ATLAN_ICEBERG.SCHEMA"), ["RAW.ATLAN_ICEBERG.SCHEMA", "RAW.ATLAN_ICEBERG.SCHEMA_HISTORY"]);
        assert_eq!(names(r#""raw"."atlan_iceberg"."schema""#)[0], "RAW.ATLAN_ICEBERG.SCHEMA", "quotes and case don't matter");
        assert_eq!(names("atlan_iceberg.schema")[0], "RAW.ATLAN_ICEBERG.SCHEMA", "a trailing path is exact too");
        assert_eq!(names("raw.atlan_iceberg")[0], "RAW.ATLAN_ICEBERG", "the schema itself");
        assert_eq!(names("schema").len(), 3, "without dots, only names count (the schema ATLAN_ICEBERG is out)");
        let best = rank(hits.clone(), "RAW.ATLAN_ICEBERG.SCHEMA");
        assert!(is_exact(&best[0], "RAW.ATLAN_ICEBERG.SCHEMA") && !is_exact(&best[1], "RAW.ATLAN_ICEBERG.SCHEMA"));
    }

    #[test]
    fn saved_index_round_trips_and_ages() {
        let idx = SfIndex { objects: vec![t("RAW", "S", "T")], schemas: vec![], refreshed: Some(Instant::now()), fallback_dbs: Some(vec!["RAW".into()]) };
        let saved = SavedIndex { saved_at: crate::user_view::now() - 600, schemas: idx.schemas.clone(), objects: idx.objects.clone(), fallback_dbs: idx.fallback_dbs.clone() };
        let back: SavedIndex = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!((back.objects, back.fallback_dbs), (idx.objects, idx.fallback_dbs));
    }

    #[test]
    fn rank_is_literal_and_capped() {
        let hits = vec![t("D", "S", "DEVX"), t("D", "S", "DEV_A")];
        assert_eq!(rank(hits, "dev_").len(), 1);
        let many = (0..80).map(|i| t("D", "S", &format!("T{i:02}"))).collect();
        assert_eq!(rank(many, "t").len(), 50);
    }

    #[test]
    fn tree_paths_follow_schema_tree_ids() {
        let sf = Hit { kind: HitKind::View, matview: true, ..t("DB", "S", "MV") };
        assert_eq!(sf.tree_path(), vec![vec!["d", "DB"], vec!["s", "DB.S"], vec!["g", "DB.S", "matviews"], vec!["r", "DB.S", "MV"]]);
        let pg = Hit { kind: HitKind::Table, matview: false, database: None, schema: Some("public".into()), name: "t".into() };
        assert_eq!(pg.tree_path(), vec![vec!["s", "public"], vec!["g", "public", "tables"], vec!["r", "public", "t"]]);
        let schema = Hit { kind: HitKind::Schema, matview: false, database: Some("DB".into()), schema: None, name: "S".into() };
        assert_eq!(schema.tree_path(), vec![vec!["d", "DB"], vec!["s", "DB.S"]]);
        let db = Hit { kind: HitKind::Database, matview: false, database: None, schema: None, name: "DB".into() };
        assert_eq!(db.tree_path(), vec![vec!["d", "DB"]]);
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn row(v: &[&str]) -> Vec<Option<String>> {
        v.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn parses_show_terse_rows() {
        let n = names(&["created_on", "name", "kind", "database_name", "schema_name"]);
        let o = parse_sf_objects(&n, &[row(&["x", "T", "TABLE", "DB", "S"]), row(&["x", "V", "VIEW", "DB", "S"]), row(&["x", "M", "MATERIALIZED_VIEW", "DB", "S"])]);
        assert_eq!(o.iter().map(|h| (h.kind, h.matview)).collect::<Vec<_>>(), [(HitKind::Table, false), (HitKind::View, false), (HitKind::View, true)]);
        let s = parse_sf_schemas(&n, &[row(&["x", "S", "", "DB", ""])]);
        assert_eq!((s[0].kind, s[0].schema_key().as_deref()), (HitKind::Schema, Some("DB.S")));
    }

    #[test]
    fn diff_reports_touched_schemas_and_databases() {
        let schema = |db: &str, n: &str| Hit { kind: HitKind::Schema, matview: false, database: Some(db.into()), schema: None, name: n.into() };
        let old = SfIndex { objects: vec![t("DB", "S", "A"), t("DB", "S", "B")], schemas: vec![schema("DB", "S")], ..Default::default() };
        assert_eq!(diff(&old, &old), IndexDiff::default());
        let mut new = SfIndex { objects: vec![t("DB", "S", "A"), t("DB", "S2", "C")], schemas: vec![schema("DB", "S"), schema("DB", "S2")], ..Default::default() };
        new.objects[0].kind = HitKind::View;
        let d = diff(&old, &new);
        assert_eq!(d.changed_schemas.into_iter().collect::<Vec<_>>(), ["DB.S", "DB.S2"]);
        assert_eq!(d.changed_dbs.into_iter().collect::<Vec<_>>(), ["DB"]);
        assert!(d.removed_schemas.is_empty());
        // A removed schema shows up separately: it leaves the cache instead of being reloaded.
        let gone = diff(&new, &old);
        assert_eq!(gone.removed_schemas.into_iter().collect::<Vec<_>>(), ["DB.S2"]);
    }

    #[test]
    fn diff_ignores_snapshots_over_different_fallback_scopes() {
        let a = SfIndex { objects: vec![t("DB", "S", "A")], fallback_dbs: Some(vec!["DB".into()]), ..Default::default() };
        let b = SfIndex { objects: vec![t("DB2", "S", "B")], fallback_dbs: Some(vec!["DB2".into()]), ..Default::default() };
        assert_eq!(diff(&a, &b), IndexDiff::default());
    }

    #[test]
    fn refresh_plan_reloads_only_what_the_tree_loaded_without_dropping_columns() {
        let mut c = MetaCache::default();
        c.objects.insert("DB.S".into(), Default::default());
        c.objects.insert("DB.GONE".into(), Default::default());
        c.loaded_dbs.insert("DB".into());
        let d = IndexDiff {
            changed_schemas: ["DB.S", "DB.OTHER", "DB.GONE"].map(String::from).into(),
            changed_dbs: ["DB", "DB9"].map(String::from).into(),
            removed_schemas: ["DB.GONE"].map(String::from).into(),
        };
        let p = refresh_plan(&d, &c);
        assert_eq!(p.reload, vec![MetaRequest::Schemas("DB".into()), MetaRequest::Objects("DB.S".into())]);
        assert_eq!(p.drop_schemas, vec!["DB.GONE".to_string()]);
        assert!(p.reload_roots);
        assert_eq!(refresh_plan(&IndexDiff::default(), &c), RefreshPlan::default());
    }

    #[test]
    fn search_index_respects_db_filter_and_includes_databases() {
        let idx = SfIndex { objects: vec![t("DEV_A", "S", "ORDERS"), t("PROD", "S", "ORDERS")], schemas: Vec::new(), ..Default::default() };
        let mut c = MetaCache { databases: vec!["DEV_A".into(), "PROD".into(), "ORDERS_DB".into()], ..Default::default() };
        c.db_filter = vec!["!PROD".into()];
        let hits = search_index(&idx, &c, "orders");
        assert_eq!(hits.iter().map(|h| (h.kind, h.database.as_deref().unwrap_or(&h.name))).collect::<Vec<_>>(), [(HitKind::Table, "DEV_A"), (HitKind::Database, "ORDERS_DB")]);
    }
}
