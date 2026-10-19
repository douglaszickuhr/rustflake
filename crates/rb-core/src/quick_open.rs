//! Busca rápida (⌘K): resultados, ordem, caminho na árvore e o índice do Snowflake.

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use crate::DbError;
use crate::lexer::quote_always;
use crate::metadata::{MetaCache, MetaRequest};
use crate::session::Session;

const MAX_HITS: usize = 50;
/// Teto de linhas de um `SHOW`; batendo nele, a listagem pode estar truncada.
const SHOW_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HitKind {
    Database,
    Schema,
    Table,
    View,
}

/// Um objeto achado. `matview` só vale com `View`: decide o grupo da árvore.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Chave de schema do cache: `DB.SCHEMA` no Snowflake, `SCHEMA` no Postgres.
    pub fn schema_key(&self) -> Option<String> {
        match self.kind {
            HitKind::Database => None,
            HitKind::Schema => Some(key(self.database.as_deref(), &self.name)),
            _ => self.schema.as_deref().map(|s| key(self.database.as_deref(), s)),
        }
    }

    /// Ids da árvore (cada um em partes) da raiz até o alvo, no formato de `schema_tree.rs`.
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

/// `%texto%` para `LIKE`/`ILIKE` com `\` como escape: `_` e `%` digitados valem literalmente.
pub fn like_pattern(text: &str) -> String {
    format!("%{}%", text.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_"))
}

/// Só quem contém o texto (sem diferenciar caixa); exato, prefixo, contém; mais curto primeiro.
pub fn rank(hits: Vec<Hit>, text: &str) -> Vec<Hit> {
    let q = text.to_uppercase();
    let mut scored: Vec<(u8, usize, Hit)> = hits
        .into_iter()
        .filter_map(|h| {
            let n = h.name.to_uppercase();
            let tier = if n == q { 0 } else if n.starts_with(&q) { 1 } else if n.contains(&q) { 2 } else { return None };
            Some((tier, n.len(), h))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(MAX_HITS).map(|(_, _, h)| h).collect()
}

fn col(names: &[String], row: &[Option<String>], c: &str) -> Option<String> {
    names.iter().position(|n| n.eq_ignore_ascii_case(c)).and_then(|i| row.get(i).cloned().flatten())
}

/// `SHOW TERSE OBJECTS`: tabelas e views (matview marcada).
pub fn parse_sf_objects(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<Hit> {
    rows.iter()
        .filter_map(|r| {
            let kind = col(names, r, "kind").unwrap_or_default();
            let is_view = kind.contains("VIEW");
            Some(Hit {
                kind: if is_view { HitKind::View } else { HitKind::Table },
                matview: kind.contains("MATERIALIZED"),
                database: Some(col(names, r, "database_name")?),
                schema: Some(col(names, r, "schema_name")?),
                name: col(names, r, "name")?,
            })
        })
        .collect()
}

/// `SHOW TERSE SCHEMAS`.
pub fn parse_sf_schemas(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<Hit> {
    rows.iter()
        .filter_map(|r| Some(Hit { kind: HitKind::Schema, matview: false, database: Some(col(names, r, "database_name")?), schema: None, name: col(names, r, "name")? }))
        .collect()
}

/// Tabelas, views e schemas da conta, como o papel atual enxerga.
#[derive(Debug, Clone, Default)]
pub struct SfIndex {
    pub objects: Vec<Hit>,
    pub schemas: Vec<Hit>,
    pub refreshed: Option<Instant>,
    /// Listagem por database (a da conta bateu no teto): quais entraram. Índices de escopos
    /// diferentes não se comparam.
    pub fallback_dbs: Option<Vec<String>>,
}

/// O que mudou entre dois índices: schemas com objetos novos/removidos/trocados de tipo e
/// databases com schemas novos/removidos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    pub changed_schemas: BTreeSet<String>,
    pub changed_dbs: BTreeSet<String>,
    /// Schemas que deixaram de existir: saem do cache em vez de serem recarregados.
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

/// O que fazer no cache da árvore com um diff: só mexe no que ela já carregou.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshPlan {
    /// Pedidos a refazer por cima do cache (sem apagar antes: colunas e a árvore continuam).
    pub reload: Vec<MetaRequest>,
    pub drop_schemas: Vec<String>,
    /// Database criada ou removida: a lista da raiz (`show databases`) muda.
    pub reload_roots: bool,
}

pub fn refresh_plan(d: &IndexDiff, cache: &MetaCache) -> RefreshPlan {
    let loaded = |s: &&String| cache.objects.contains_key(*s);
    let dbs = d.changed_dbs.iter().filter(|db| cache.loaded_dbs.contains(*db)).map(|db| MetaRequest::Schemas(db.clone()));
    let schemas = d.changed_schemas.iter().filter(loaded).filter(|s| !d.removed_schemas.contains(*s)).map(|s| MetaRequest::Objects(s.clone()));
    RefreshPlan {
        reload: dbs.chain(schemas).collect(),
        drop_schemas: d.removed_schemas.iter().filter(loaded).cloned().collect(),
        reload_roots: !d.changed_dbs.is_empty(),
    }
}

/// Busca local: databases visíveis (do cache), schemas e objetos do índice nas databases visíveis.
pub fn search_index(idx: &SfIndex, cache: &MetaCache, text: &str) -> Vec<Hit> {
    let visible: HashSet<&str> = cache.visible_databases().map(String::as_str).collect();
    let dbs = visible.iter().map(|d| Hit { kind: HitKind::Database, matview: false, database: None, schema: None, name: d.to_string() });
    let rest = idx.schemas.iter().chain(&idx.objects).filter(|h| h.database.as_deref().is_some_and(|d| visible.contains(d))).cloned();
    rank(dbs.chain(rest).collect(), text)
}

// Mais curtos primeiro antes do corte: o exato e os prefixos não ficam de fora num catálogo grande.
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

/// Postgres: busca ao vivo no catálogo (local, milissegundos).
pub async fn pg_search(s: &Session, text: &str) -> Result<Vec<Hit>, DbError> {
    let rows = s.query_text(PG_SEARCH, &[&like_pattern(text)]).await?;
    let hits = rows
        .into_iter()
        .filter_map(|r| {
            let (kind, schema, rel) = (r.first()?.clone()?, r.get(1)?.clone()?, r.get(2).cloned().flatten());
            Some(match (kind.as_str(), rel) {
                ("schema", _) => Hit { kind: HitKind::Schema, matview: false, database: None, schema: None, name: schema },
                (k, Some(name)) => Hit {
                    kind: if matches!(k, "v" | "m") { HitKind::View } else { HitKind::Table },
                    matview: k == "m",
                    database: None,
                    schema: Some(schema),
                    name,
                },
                _ => return None,
            })
        })
        .collect();
    Ok(rank(hits, text))
}

/// Snowflake: schemas e objetos da conta. Lista truncada no teto do `SHOW` → refaz por database.
pub async fn sf_snapshot(s: &Session, dbs: &[String]) -> Result<SfIndex, DbError> {
    let (n, rows) = s.query_named("show terse schemas in account").await?;
    let schemas = parse_sf_schemas(&n, &rows);
    let (n, rows) = s.query_named("show terse objects in account").await?;
    if rows.len() < SHOW_LIMIT {
        return Ok(SfIndex { objects: parse_sf_objects(&n, &rows), schemas, refreshed: Some(Instant::now()), fallback_dbs: None });
    }
    // ponytail: sem paginação `FROM`; uma database com mais de 10k objetos fica truncada.
    let mut objects = Vec::new();
    for db in dbs {
        // Uma database que falha (removida no meio, share sem acesso) não derruba o índice todo.
        match s.query_named(&format!("show terse objects in database {}", quote_always(db))).await {
            Ok((n, rows)) => {
                if rows.len() >= SHOW_LIMIT {
                    tracing::warn!(db, "more than 10k objects: index truncated");
                }
                objects.extend(parse_sf_objects(&n, &rows));
            }
            Err(e) => tracing::warn!(db, "quick-open index: {e}"),
        }
    }
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
        // Schema removido aparece à parte: some do cache em vez de ser recarregado.
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
