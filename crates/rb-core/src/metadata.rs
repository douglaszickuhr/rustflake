use std::collections::{HashMap, HashSet};

use crate::DbError;
use crate::dialect::Dialect;
use crate::lexer::quote_always;
use crate::session::Session;
use crate::snowflake::{first_named, parse_describe, parse_functions, parse_objects};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaObjects {
    pub tables: Vec<String>,
    pub views: Vec<String>,
    pub matviews: Vec<String>,
    /// `name(arguments)`.
    pub functions: Vec<String>,
    /// Stored procedures, `name(arguments)` (Snowflake).
    pub procedures: Vec<String>,
    /// Tasks: (name, `started` / `suspended`) (Snowflake).
    pub tasks: Vec<(String, String)>,
}

impl SchemaObjects {
    pub fn contains_relation(&self, name: &str) -> bool {
        self.tables.iter().chain(&self.views).chain(&self.matviews).any(|t| t == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Output of `format_type`, ready for cast (`character varying(20)`, `"Sales".mood`).
    pub type_name: String,
    /// Type without typmod (`character varying`), used in edit casts: an explicit cast to
    /// `varchar(5)` would truncate silently; without typmod, the assignment to the column validates the size.
    pub cast_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_pk: bool,
    pub generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub name: String,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MetaRequest {
    Objects(String),
    Columns(String, String),
    Indexes(String, String),
    /// Schemas of a Snowflake database.
    Schemas(String),
}

#[derive(Debug, Default)]
pub struct MetaCache {
    pub search_path: Vec<String>,
    pub schemas: Vec<String>,
    pub objects: HashMap<String, SchemaObjects>,
    pub columns: HashMap<(String, String), Vec<ColumnMeta>>,
    pub indexes: HashMap<(String, String), Vec<IndexMeta>>,
    pub dialect: Dialect,
    /// Snowflake: account databases and which ones already had their schemas loaded.
    pub databases: Vec<String>,
    pub loaded_dbs: HashSet<String>,
    /// Connection patterns (`SnowflakeConfig::db_filter`); empty = all databases.
    pub db_filter: Vec<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
}

impl MetaCache {
    /// Databases that pass the connection filter (tree and autocomplete): they match some include
    /// pattern (with none, all match) and no `!pattern` exclusion.
    pub fn visible_databases(&self) -> impl Iterator<Item = &String> {
        let (exclude, include): (Vec<_>, Vec<_>) = self.db_filter.iter().partition(|p| p.starts_with('!'));
        self.databases.iter().filter(move |d| (include.is_empty() || include.iter().any(|p| glob_match(p, d))) && !exclude.iter().any(|p| glob_match(&p[1..], d)))
    }

    pub fn has(&self, req: &MetaRequest) -> bool {
        match req {
            MetaRequest::Objects(s) => self.objects.contains_key(s),
            MetaRequest::Columns(s, t) => self.columns.contains_key(&(s.clone(), t.clone())),
            MetaRequest::Indexes(s, t) => self.indexes.contains_key(&(s.clone(), t.clone())),
            MetaRequest::Schemas(db) => self.loaded_dbs.contains(db),
        }
    }

    /// Current database on Snowflake (start of the `DB.SCHEMA` search_path).
    pub fn current_db(&self) -> Option<&str> {
        self.search_path.first().and_then(|s| s.split_once('.')).map(|(db, _)| db)
    }

    /// Schema key as it is in the cache: on Snowflake, `SCHEMA` becomes `CURRENT_DB.SCHEMA`.
    pub fn schema_key(&self, s: &str) -> String {
        match (self.dialect, self.current_db()) {
            (Dialect::Snowflake, Some(db)) if !s.contains('.') => format!("{db}.{s}"),
            _ => s.to_string(),
        }
    }

    pub fn invalidate_schema(&mut self, schema: &str) {
        self.objects.remove(schema);
        self.columns.retain(|(s, _), _| s != schema);
        self.indexes.retain(|(s, _), _| s != schema);
    }

    pub fn invalidate_table(&mut self, schema: &str, table: &str) {
        let key = (schema.to_string(), table.to_string());
        self.columns.remove(&key);
        self.indexes.remove(&key);
    }
}

const SCHEMAS_SQL: &str = "select nspname::text from pg_namespace \
     where nspname !~ '^pg_' and nspname <> 'information_schema' order by 1";

const SEARCH_PATH_SQL: &str = "select unnest(current_schemas(false))::text";

const OBJECTS_SQL: &str = "select c.relname::text, c.relkind::text from pg_class c \
     join pg_namespace n on n.oid = c.relnamespace \
     where n.nspname = $1::text and c.relkind in ('r', 'p', 'f', 'v', 'm') and not c.relispartition \
     order by 1";

const FUNCTIONS_SQL: &str = "select (p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')')::text \
     from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
     where n.nspname = $1::text and p.prokind in ('f', 'p') order by 1";

const COLUMNS_SQL: &str = "select a.attname::text, format_type(a.atttypid, a.atttypmod)::text, \
       (not a.attnotnull)::text, pg_get_expr(d.adbin, d.adrelid)::text, \
       (exists (select 1 from pg_index i where i.indrelid = a.attrelid and i.indisprimary and a.attnum = any(i.indkey)))::text, \
       (a.attgenerated <> '')::text, format_type(a.atttypid, NULL)::text \
     from pg_attribute a \
     join pg_class c on c.oid = a.attrelid \
     join pg_namespace n on n.oid = c.relnamespace \
     left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
     where n.nspname = $1::text and c.relname = $2::text and a.attnum > 0 and not a.attisdropped \
     order by a.attnum";

const INDEXES_SQL: &str = "select i.relname::text, pg_get_indexdef(i.oid)::text from pg_index x \
     join pg_class i on i.oid = x.indexrelid \
     join pg_class c on c.oid = x.indrelid \
     join pg_namespace n on n.oid = c.relnamespace \
     where n.nspname = $1::text and c.relname = $2::text order by 1";

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn first_column(rows: Vec<Vec<Option<String>>>) -> Vec<String> {
    rows.iter().map(|r| text(r, 0)).collect()
}

pub struct Context {
    pub search_path: Vec<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
}

/// `*` matches any sequence, `?` one character; the rest is literal, case-insensitive. `DEV_*` = starts with DEV_.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    wildcard(pattern.to_uppercase().as_bytes(), name.to_uppercase().as_bytes())
}

/// Case-sensitive `*`/`?` matching on bytes (greedy with backtracking to the last `*`: linear-ish, no recursion).
pub fn wildcard(p: &[u8], n: &[u8]) -> bool {
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// `"DEV_*, !*_TMP"` → `["DEV_*", "!*_TMP"]`; commas and spaces separate, empty ones vanish.
pub fn parse_db_filter(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n']).map(str::trim).filter(|p| !p.is_empty() && *p != "!").map(String::from).collect()
}

pub async fn databases(s: &Session) -> Result<Vec<String>, DbError> {
    let (names, rows) = s.query_named("show databases").await?;
    Ok(first_named(&names, &rows, "name"))
}

pub async fn db_schemas(s: &Session, db: &str) -> Result<Vec<String>, DbError> {
    let (names, rows) = s.query_named(&format!("show schemas in database {}", quote_always(db))).await?;
    Ok(first_named(&names, &rows, "name").into_iter().map(|n| format!("{db}.{n}")).collect())
}

pub async fn context(s: &Session) -> Result<Context, DbError> {
    match s.dialect() {
        Dialect::Postgres => Ok(Context { search_path: search_path(s).await?, warehouse: None, role: None }),
        Dialect::Snowflake => {
            let (_, rows) = s.query_named("select current_database(), current_schema(), current_warehouse(), current_role()").await?;
            let r = rows.into_iter().next().unwrap_or_default();
            let get = |i: usize| r.get(i).cloned().flatten();
            let search_path = match (get(0), get(1)) {
                (Some(db), Some(sc)) => vec![format!("{db}.{sc}")],
                _ => Vec::new(),
            };
            Ok(Context { search_path, warehouse: get(2), role: get(3) })
        }
    }
}

pub async fn schemas(s: &Session) -> Result<Vec<String>, DbError> {
    // On Snowflake the tree starts from the databases.
    if s.dialect() == Dialect::Snowflake {
        return Ok(Vec::new());
    }
    Ok(first_column(s.query_text(SCHEMAS_SQL, &[]).await?))
}

pub async fn search_path(s: &Session) -> Result<Vec<String>, DbError> {
    Ok(first_column(s.query_text(SEARCH_PATH_SQL, &[]).await?))
}

pub async fn objects(s: &Session, schema: &str) -> Result<SchemaObjects, DbError> {
    if s.dialect() == Dialect::Snowflake {
        let q = Dialect::Snowflake.qualified_schema(schema);
        let (s2, s3, s4) = (s.sibling(), s.sibling(), s.sibling());
        let (q_objs, q_funcs, q_procs, q_tasks) =
            (format!("show objects in schema {q}"), format!("show user functions in schema {q}"), format!("show user procedures in schema {q}"), format!("show tasks in schema {q}"));
        let (objs, funcs, procs, tasks) =
            tokio::join!(s.query_named(&q_objs), s2.as_ref().unwrap_or(s).query_named(&q_funcs), s3.as_ref().unwrap_or(s).query_named(&q_procs), s4.as_ref().unwrap_or(s).query_named(&q_tasks));
        let ((n, rows), (fnames, frows)) = (objs?, funcs?);
        let mut o = parse_objects(&n, &rows);
        o.functions = parse_functions(&fnames, &frows);
        // Same `arguments` shape as functions; a role that can't list them just gets none.
        o.procedures = procs.map(|(n, r)| parse_functions(&n, &r)).unwrap_or_default();
        o.tasks = tasks.map(|(n, r)| crate::task::parse_tasks(&n, &r).into_iter().map(|t| (t.name, t.state)).collect()).unwrap_or_default();
        return Ok(o);
    }
    let mut o = SchemaObjects::default();
    for r in s.query_text(OBJECTS_SQL, &[schema]).await? {
        let name = text(&r, 0);
        match text(&r, 1).as_str() {
            "v" => o.views.push(name),
            "m" => o.matviews.push(name),
            _ => o.tables.push(name),
        }
    }
    o.functions = first_column(s.query_text(FUNCTIONS_SQL, &[schema]).await?);
    Ok(o)
}

pub async fn columns(s: &Session, schema: &str, table: &str) -> Result<Vec<ColumnMeta>, DbError> {
    if s.dialect() == Dialect::Snowflake {
        let name = Dialect::Snowflake.qualified(schema, table);
        let (n, rows) = match s.query_named(&format!("describe table {name}")).await {
            Ok(r) => r,
            // DESCRIBE TABLE rejects views; the view has its own DESCRIBE.
            Err(_) => s.query_named(&format!("describe view {name}")).await?,
        };
        return Ok(parse_describe(&n, &rows));
    }
    Ok(s.query_text(COLUMNS_SQL, &[schema, table])
        .await?
        .into_iter()
        .map(|r| ColumnMeta {
            name: text(&r, 0),
            type_name: text(&r, 1),
            nullable: text(&r, 2) == "true",
            default: r.get(3).cloned().flatten(),
            is_pk: text(&r, 4) == "true",
            generated: text(&r, 5) == "true",
            cast_type: text(&r, 6),
        })
        .collect())
}

pub async fn indexes(s: &Session, schema: &str, table: &str) -> Result<Vec<IndexMeta>, DbError> {
    if s.dialect() == Dialect::Snowflake {
        return Ok(Vec::new());
    }
    Ok(s.query_text(INDEXES_SQL, &[schema, table]).await?.into_iter().map(|r| IndexMeta { name: text(&r, 0), definition: text(&r, 1) }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_prefix_suffix_middle_and_exact() {
        assert!(glob_match("DEV_*", "DEV_SALES") && glob_match("dev_*", "Dev_x") && glob_match("DEV_*", "DEV_"));
        assert!(!glob_match("DEV_*", "DEVX_SALES") && !glob_match("DEV_*", "AI_DEV_X"));
        assert!(glob_match("*_PROD", "AI_PROD") && !glob_match("*_PROD", "AI_PROD_OLD"));
        assert!(glob_match("*DBT*", "AI_DBT_TEMP") && glob_match("A*B*C", "AXXBYYC") && !glob_match("A*B*C", "ACB"));
        assert!(glob_match("AB*BA", "ABBA") && !glob_match("AB*BA", "ABA"));
        assert!(glob_match("AI_MARTS", "ai_marts") && !glob_match("AI_MARTS", "AI_MARTS2"));
        assert!(glob_match("*", "ANYTHING") && glob_match("DB?", "db1") && !glob_match("DB?", "DB12"));
        assert!(wildcard(b"*.conf", b"x.conf") && !wildcard(b"*.conf", b"X.CONF.bak") && wildcard(b"a*b*", b"ab"));
    }

    #[test]
    fn filter_text_splits_on_commas_and_spaces() {
        assert_eq!(parse_db_filter(" DEV_*, AI_*  ,,X "), vec!["DEV_*", "AI_*", "X"]);
        assert!(parse_db_filter("  ! ").is_empty());
        assert_eq!(parse_db_filter("!X_*"), vec!["!X_*"]);
    }

    #[test]
    fn visible_databases_follow_filter() {
        let mut c = MetaCache { databases: vec!["AI_PROD".into(), "DEV_A".into(), "CX_OPS".into()], ..Default::default() };
        assert_eq!(c.visible_databases().count(), 3);
        c.db_filter = vec!["DEV_*".into(), "*_PROD".into()];
        assert_eq!(c.visible_databases().collect::<Vec<_>>(), ["AI_PROD", "DEV_A"]);
        // `!` excludes; only exclusions = all but those.
        c.db_filter = vec!["!dev_*".into()];
        assert_eq!(c.visible_databases().collect::<Vec<_>>(), ["AI_PROD", "CX_OPS"]);
        c.db_filter = vec!["*_*".into(), "!*_PROD".into()];
        assert_eq!(c.visible_databases().collect::<Vec<_>>(), ["DEV_A", "CX_OPS"]);
    }
}
