use std::collections::HashMap;

use crate::DbError;
use crate::session::Session;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaObjects {
    pub tables: Vec<String>,
    pub views: Vec<String>,
    pub matviews: Vec<String>,
    /// `nome(argumentos)`.
    pub functions: Vec<String>,
}

impl SchemaObjects {
    pub fn contains_relation(&self, name: &str) -> bool {
        self.tables.iter().chain(&self.views).chain(&self.matviews).any(|t| t == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// Saída de `format_type`, pronta para cast (`character varying(20)`, `"Sales".mood`).
    pub type_name: String,
    /// Tipo sem typmod (`character varying`), usado nos casts de edição: o cast explícito para
    /// `varchar(5)` truncaria em silêncio; sem typmod, a atribuição à coluna valida o tamanho.
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
}

#[derive(Debug, Default)]
pub struct MetaCache {
    pub search_path: Vec<String>,
    pub schemas: Vec<String>,
    pub objects: HashMap<String, SchemaObjects>,
    pub columns: HashMap<(String, String), Vec<ColumnMeta>>,
    pub indexes: HashMap<(String, String), Vec<IndexMeta>>,
}

impl MetaCache {
    pub fn has(&self, req: &MetaRequest) -> bool {
        match req {
            MetaRequest::Objects(s) => self.objects.contains_key(s),
            MetaRequest::Columns(s, t) => self.columns.contains_key(&(s.clone(), t.clone())),
            MetaRequest::Indexes(s, t) => self.indexes.contains_key(&(s.clone(), t.clone())),
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

pub async fn schemas(s: &Session) -> Result<Vec<String>, DbError> {
    Ok(first_column(s.query_text(SCHEMAS_SQL, &[]).await?))
}

pub async fn search_path(s: &Session) -> Result<Vec<String>, DbError> {
    Ok(first_column(s.query_text(SEARCH_PATH_SQL, &[]).await?))
}

pub async fn objects(s: &Session, schema: &str) -> Result<SchemaObjects, DbError> {
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
    Ok(s.query_text(INDEXES_SQL, &[schema, table])
        .await?
        .into_iter()
        .map(|r| IndexMeta { name: text(&r, 0), definition: text(&r, 1) })
        .collect())
}
