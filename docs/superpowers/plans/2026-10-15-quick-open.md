# Busca rápida (⌘K) — plano de implementação

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ⌘K abre um popup que acha database/schema/tabela/view pelo nome e revela o objeto na árvore (abrindo a aba de tabela quando for tabela/view).

**Architecture:** Lógica pura e queries em `rb-core/src/quick_open.rs`. No Snowflake, um `Indexer` por conexão (sessão própria, mesmo login) refaz `SHOW TERSE … IN ACCOUNT` a cada 30 s, calcula o diff e corrige o cache da árvore; a busca é local. No Postgres, uma query ao vivo em `pg_catalog`. O popup (`rb-app/src/quick_open.rs`) chama `Workspace::reveal_hit`, que usa `SchemaTree::reveal`.

**Tech Stack:** Rust 2024, gpui-kit 0.7.3 (gpui-component), tokio-postgres, snowflake-connector-rs 1.1.0.

**Spec:** `docs/superpowers/specs/2026-10-15-quick-open-design.md`

## Global Constraints

- Nenhuma dependência nova.
- Textos da interface em português (a i18n vem no plano seguinte e traduz tudo).
- Comentários em português, no estilo do código vizinho (curtos, dizem o porquê).
- Atalho `cmd-k` no contexto `Workspace`.
- Indexador: tick de 30 s; sem sobreposição; erro mantém o índice anterior.
- Até 50 resultados; ordem exato < prefixo < contém, depois nome mais curto, depois alfabético.
- Resultados respeitam `db_filter` (`MetaCache::visible_databases`).

## Review Focus

1. Texto com `_` ou `%` (`DEV_`): não pode virar curinga — `like_pattern` escapa (Postgres) e o índice compara literal (Task 1).
2. Objeto de database escondida pelo filtro não aparece no ⌘K (Task 1, `search_index`).
3. Revelar tabela num schema ainda não carregado: nó aparece quando o metadado chega, sem precisar de outro ⌘K (Task 3).
4. Tick de 30 s enquanto o snapshot anterior ainda roda: pula, não empilha queries (Task 4).
5. Desconectar com snapshot em andamento: nada é aplicado depois (a `Task` cai com o `Open`) (Task 4).

---

### Task 1: Núcleo puro — `Hit`, ordem, caminho na árvore, parse, diff, busca no índice

**Files:**
- Create: `crates/rb-core/src/quick_open.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod quick_open;`)

**Interfaces:**
- Produces:
  - `HitKind { Database, Schema, Table, View }`, `Hit { kind, matview: bool, database: Option<String>, schema: Option<String>, name: String }`
  - `Hit::schema_key(&self) -> Option<String>` (`DB.S` no Snowflake, `S` no Postgres)
  - `Hit::tree_path(&self) -> Vec<Vec<String>>` (partes de cada id, da raiz ao alvo; o app junta com `\u{1f}`)
  - `like_pattern(&str) -> String`, `rank(Vec<Hit>, &str) -> Vec<Hit>`
  - `parse_sf_objects(&[String], &[Vec<Option<String>>]) -> Vec<Hit>`, `parse_sf_schemas(..) -> Vec<Hit>`
  - `SfIndex { objects: Vec<Hit>, schemas: Vec<Hit>, refreshed: Option<Instant> }`
  - `IndexDiff { changed_schemas: BTreeSet<String>, changed_dbs: BTreeSet<String> }`, `diff(&SfIndex, &SfIndex) -> IndexDiff`
  - `search_index(&SfIndex, &MetaCache, &str) -> Vec<Hit>`

- [ ] **Step 1: Escrever os testes** (no fim de `quick_open.rs`, `#[cfg(test)] mod tests`)

```rust
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
    let old = SfIndex { objects: vec![t("DB", "S", "A"), t("DB", "S", "B")], schemas: vec![schema("DB", "S")], refreshed: None };
    assert_eq!(diff(&old, &old), IndexDiff::default());
    let mut new = SfIndex { objects: vec![t("DB", "S", "A"), t("DB", "S2", "C")], schemas: vec![schema("DB", "S"), schema("DB", "S2")], refreshed: None };
    new.objects[0].kind = HitKind::View;
    let d = diff(&old, &new);
    assert_eq!(d.changed_schemas.into_iter().collect::<Vec<_>>(), ["DB.S", "DB.S2"]);
    assert_eq!(d.changed_dbs.into_iter().collect::<Vec<_>>(), ["DB"]);
}

#[test]
fn search_index_respects_db_filter_and_includes_databases() {
    let idx = SfIndex { objects: vec![t("DEV_A", "S", "ORDERS"), t("PROD", "S", "ORDERS")], schemas: Vec::new(), refreshed: None };
    let mut c = MetaCache { databases: vec!["DEV_A".into(), "PROD".into(), "ORDERS_DB".into()], ..Default::default() };
    c.db_filter = vec!["!PROD".into()];
    let hits = search_index(&idx, &c, "orders");
    assert_eq!(hits.iter().map(|h| (h.kind, h.database.as_deref().unwrap_or(&h.name))).collect::<Vec<_>>(), [(HitKind::Table, "DEV_A"), (HitKind::Database, "ORDERS_DB")]);
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --lib quick_open`
Expected: FAIL (módulo/funções não existem).

- [ ] **Step 3: Implementar**

```rust
//! Busca rápida (⌘K): resultados, ordem, caminho na árvore e o índice do Snowflake.

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use crate::DbError;
use crate::lexer::quote_always;
use crate::metadata::MetaCache;
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
}

/// O que mudou entre dois índices: schemas com objetos novos/removidos/trocados de tipo e
/// databases com schemas novos/removidos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    pub changed_schemas: BTreeSet<String>,
    pub changed_dbs: BTreeSet<String>,
}

pub fn diff(old: &SfIndex, new: &SfIndex) -> IndexDiff {
    let set = |v: &[Hit]| v.iter().cloned().collect::<HashSet<_>>();
    let (oo, no, os, ns) = (set(&old.objects), set(&new.objects), set(&old.schemas), set(&new.schemas));
    IndexDiff {
        changed_schemas: oo.symmetric_difference(&no).filter_map(Hit::schema_key).collect(),
        changed_dbs: os.symmetric_difference(&ns).filter_map(|h| h.database.clone()).collect(),
    }
}

/// Busca local: databases visíveis (do cache), schemas e objetos do índice nas databases visíveis.
pub fn search_index(idx: &SfIndex, cache: &MetaCache, text: &str) -> Vec<Hit> {
    let visible: HashSet<&str> = cache.visible_databases().map(String::as_str).collect();
    let dbs = visible.iter().map(|d| Hit { kind: HitKind::Database, matview: false, database: None, schema: None, name: d.to_string() });
    let rest = idx.schemas.iter().chain(&idx.objects).filter(|h| h.database.as_deref().is_some_and(|d| visible.contains(d))).cloned();
    rank(dbs.chain(rest).collect(), text)
}
```

Note: `search_index` com duas databases visíveis em ordem de `HashSet` — `rank` ordena tudo, então a ordem final é determinística.

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core --lib quick_open`
Expected: 7 passed.

- [ ] **Step 5: Commit** — só se o usuário pedir (ele comita depois).

---

### Task 2: Queries — `sf_snapshot` e `pg_search`

**Files:**
- Modify: `crates/rb-core/src/quick_open.rs`
- Test: `crates/rb-core/tests/session.rs` (Postgres, mesmo padrão dos testes existentes com banco)

**Interfaces:**
- Consumes: `Session::query_named`, `Session::query_text`, funções da Task 1.
- Produces: `pub async fn sf_snapshot(s: &Session, dbs: &[String]) -> Result<SfIndex, DbError>`, `pub async fn pg_search(s: &Session, text: &str) -> Result<Vec<Hit>, DbError>`

- [ ] **Step 1: Teste de integração Postgres** (em `tests/session.rs`, usando o helper de conexão que o arquivo já usa; ver o topo do arquivo)

```rust
#[test]
fn quick_open_pg_search_finds_schema_and_tables_literally() {
    let s = connect(); // helper existente no arquivo
    block_on(s.execute("create schema if not exists qo_test".into())).unwrap();
    block_on(s.execute("create table if not exists qo_test.dev_orders(id int)".into())).unwrap();
    block_on(s.execute("create table if not exists qo_test.devxorders(id int)".into())).unwrap();
    let hits = block_on(rb_core::quick_open::pg_search(&s, "dev_")).unwrap();
    let names: Vec<_> = hits.iter().map(|h| h.name.as_str()).collect();
    assert!(names.contains(&"dev_orders") && !names.contains(&"devxorders"), "{names:?}");
    let schema = block_on(rb_core::quick_open::pg_search(&s, "qo_te")).unwrap();
    assert_eq!(schema[0].kind, rb_core::quick_open::HitKind::Schema);
}
```

(Ajustar `connect`/`block_on` aos nomes reais do arquivo.)

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --test session quick_open`
Expected: FAIL (função não existe). Sem Postgres local o teste é ignorado como os demais do arquivo.

- [ ] **Step 3: Implementar**

```rust
const PG_SEARCH: &str = "\
select 'schema'::text, nspname::text, null::text from pg_namespace
 where nspname ilike $1 and nspname not like 'pg\\_%' and nspname <> 'information_schema'
union all
select c.relkind::text, n.nspname::text, c.relname::text from pg_class c join pg_namespace n on n.oid = c.relnamespace
 where c.relkind in ('r','p','v','m','f') and c.relname ilike $1
   and n.nspname not like 'pg\\_%' and n.nspname <> 'information_schema'
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
    let objects = if rows.len() < SHOW_LIMIT {
        parse_sf_objects(&n, &rows)
    } else {
        // ponytail: sem paginação `FROM`; uma database com mais de 10k objetos fica truncada.
        let mut all = Vec::new();
        for db in dbs {
            let (n, rows) = s.query_named(&format!("show terse objects in database {}", quote_always(db))).await?;
            if rows.len() >= SHOW_LIMIT {
                tracing::warn!(db, "mais de 10 mil objetos: índice truncado");
            }
            all.extend(parse_sf_objects(&n, &rows));
        }
        all
    };
    Ok(SfIndex { objects, schemas, refreshed: Some(Instant::now()) })
}
```

Note: confira se `rb-core` já depende de `tracing` (sim, `Cargo.toml`).

- [ ] **Step 4: Rodar**

Run: `cargo test -p rb-core --test session quick_open` (com o Postgres de teste do projeto) e `cargo build -p rb-core`.
Expected: PASS / compila.

---

### Task 3: Revelar na árvore — `SchemaTree::reveal`

**Files:**
- Modify: `crates/rb-app/src/schema_tree.rs`

**Interfaces:**
- Consumes: `Hit::tree_path()` (Task 1).
- Produces: `pub fn reveal(&mut self, path: Vec<Vec<String>>, cx: &mut Context<Self>)`; `pub fn path_ids(path: &[Vec<String>]) -> Vec<SharedString>`.

- [ ] **Step 1: Teste puro** (em `mod tests` de `schema_tree.rs`): com o cache carregado e os ancestrais em `expanded`, `build_items` inclui o alvo como descendente visível.

```rust
#[::core::prelude::v1::test]
fn revealed_path_is_built_expanded() {
    let c = cache();
    let hit = rb_core::quick_open::Hit { kind: rb_core::quick_open::HitKind::Table, matview: false, database: None, schema: Some("public".into()), name: "t".into() };
    let ids = path_ids(&hit.tree_path());
    let expanded: HashSet<SharedString> = ids[..ids.len() - 1].iter().cloned().collect();
    let items = build_items(&c, &expanded);
    let schema = items.iter().find(|i| i.id == ids[0]).unwrap();
    assert!(schema.is_expanded());
    let group = schema.children.iter().find(|i| i.id == ids[1]).unwrap();
    assert!(group.is_expanded() && group.children.iter().any(|i| i.id == ids[2]));
}
```

- [ ] **Step 2: Rodar e ver falhar** — `cargo test -p rb-app revealed_path` → FAIL (`path_ids` não existe).

- [ ] **Step 3: Implementar**

1. `pub fn path_ids(path: &[Vec<String>]) -> Vec<SharedString> { path.iter().map(|p| id(&p.iter().map(String::as_str).collect::<Vec<_>>())).collect() }`
2. Campo novo em `SchemaTree`: `pending_reveal: Option<SharedString>` (inicia `None`).
3. Extrair do braço `TreeEvent::Expanded` a carga de metadados para `fn load_children(&mut self, node: &SharedString, cx: &mut Context<Self>)` (mesmo `match parts` de hoje) e chamá-la lá.
4. Novo método:

```rust
/// Expande do topo até o alvo (pedindo o que falta carregar) e seleciona o alvo quando ele aparecer.
pub fn reveal(&mut self, path: Vec<Vec<String>>, cx: &mut Context<Self>) {
    let ids = path_ids(&path);
    let Some((target, ancestors)) = ids.split_last() else { return };
    for node in ancestors {
        self.expanded.insert(node.clone());
        self.load_children(node, cx);
    }
    self.pending_reveal = Some(target.clone());
    self.rebuild(cx);
}
```

5. Em `rebuild`, depois de `set_items`, dentro do mesmo `update`:

```rust
let pending = self.pending_reveal.clone();
let found = self.state.update(cx, |st, cx| {
    st.set_items(items, cx);
    let target = pending?;
    let ix = st.index_of(&target)?;
    st.set_selected_index(Some(ix), cx);
    st.reveal_item(&target, ScrollStrategy::Center, cx);
    Some(())
});
if found.is_some() {
    self.pending_reveal = None;
}
```

(`ScrollStrategy` vem de `gpui_kit::*`.)

- [ ] **Step 4: Rodar** — `cargo test -p rb-app` → PASS.

---

### Task 4: Indexador do Snowflake (tick de 30 s, diff corrige a árvore)

**Files:**
- Create: `crates/rb-app/src/indexer.rs`
- Modify: `crates/rb-app/src/meta.rs` (método `apply_index_diff`), `crates/rb-app/src/workspace.rs` (`Open.indexer`), `crates/rb-app/src/main.rs` (`mod indexer;`)

**Interfaces:**
- Consumes: `sf_snapshot`, `diff`, `IndexDiff` (Tasks 1–2); `Db` global; `ConnTarget`.
- Produces: `pub struct Indexer { pub index: Option<SfIndex>, pub running: bool, pub error: Option<String>, .. }`, `Indexer::new(conn: ConnTarget, meta: Entity<MetaStore>, cx) -> Self`, `Indexer::refresh(&mut self, cx)`; `MetaStore::apply_index_diff(&mut self, d: IndexDiff, cx)`; `Open { .., indexer: Option<Entity<Indexer>> }`.

- [ ] **Step 1: `MetaStore::apply_index_diff`** (em `meta.rs`):

```rust
/// Recarrega só o que a árvore já tinha carregado e mudou no banco.
pub fn apply_index_diff(&mut self, d: IndexDiff, cx: &mut Context<Self>) {
    for db in d.changed_dbs {
        if self.cache.loaded_dbs.remove(&db) {
            self.request(MetaRequest::Schemas(db), cx);
        }
    }
    for s in d.changed_schemas {
        if self.cache.objects.contains_key(&s) {
            self.cache.invalidate_schema(&s);
            self.request(MetaRequest::Objects(s), cx);
        }
    }
}
```

- [ ] **Step 2: `indexer.rs`**

```rust
//! Índice da busca rápida no Snowflake: sessão própria, refeito a cada 30 s; o diff corrige a árvore.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use rb_core::quick_open::{SfIndex, diff, sf_snapshot};
use rb_core::session::Session;

use crate::Db;
use crate::meta::{ConnTarget, MetaStore};

const TICK: Duration = Duration::from_secs(30);

pub struct Indexer {
    pub index: Option<SfIndex>,
    pub running: bool,
    pub error: Option<String>,
    conn: ConnTarget,
    session: Option<Arc<Session>>,
    meta: Entity<MetaStore>,
    _tick: Task<()>,
}

impl Indexer {
    pub fn new(conn: ConnTarget, meta: Entity<MetaStore>, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |ix, cx| ix.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }
        });
        Indexer { index: None, running: false, error: None, conn, session: None, meta, _tick: tick }
    }

    /// Snapshot novo; pula se o anterior ainda roda.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        self.running = true;
        cx.notify();
        let (conn, existing, engine) = (self.conn.clone(), self.session.clone(), cx.global::<Db>().0.clone());
        let dbs: Vec<String> = self.meta.read(cx).cache.visible_databases().cloned().collect();
        cx.spawn(async move |this, cx| {
            let res = async {
                // Sessão própria (mesmo login): não entra na fila da árvore e do autocomplete.
                let s = match existing {
                    Some(s) => s,
                    None => Arc::new(engine.connect(&conn.cfg, conn.creds.clone()).await?),
                };
                let idx = sf_snapshot(&s, &dbs).await?;
                Ok::<_, rb_core::DbError>((s, idx))
            }
            .await;
            this.update(cx, |ix, cx| {
                ix.running = false;
                match res {
                    Ok((s, new)) => {
                        ix.session = Some(s);
                        if let Some(old) = &ix.index {
                            let d = diff(old, &new);
                            ix.meta.update(cx, |m, cx| m.apply_index_diff(d, cx));
                        }
                        ix.index = Some(new);
                        ix.error = None;
                    }
                    Err(e) => {
                        tracing::warn!("índice da busca rápida: {e}");
                        ix.error = Some(e.to_string());
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }
}
```

Note: o `this.update` falha quando o `Indexer` cai (desconexão) — o resultado é descartado e o laço do tick termina.

- [ ] **Step 3: Ligar no `Workspace`**: `Open` ganha `pub indexer: Option<Entity<Indexer>>`. No ponto onde `Open` é montado (`ws.open.push(Open { conn, meta, tree })`), antes do push:

```rust
let indexer = conn.cfg.snowflake.is_some().then(|| cx.new(|cx| Indexer::new(conn.clone(), meta.clone(), cx)));
ws.open.push(Open { conn, meta, tree, indexer });
```

e `mod indexer;` em `main.rs`.

- [ ] **Step 4: Build e testes** — `cargo build -p rb-app && cargo test --workspace --lib --bins` → PASS.

---

### Task 5: Popup ⌘K e revelação

**Files:**
- Create: `crates/rb-app/src/quick_open.rs`
- Modify: `crates/rb-app/src/main.rs` (ações `QuickOpen`, `QuickPrev`, `QuickNext`; atalhos; `mod quick_open;`), `crates/rb-app/src/workspace.rs` (handler e `reveal_hit`)

**Interfaces:**
- Consumes: `Indexer` (Task 4), `search_index`, `pg_search`, `Hit` (Tasks 1–2), `SchemaTree::reveal` (Task 3), `Workspace::open_table`.
- Produces: `quick_open::open(meta: Entity<MetaStore>, indexer: Option<Entity<Indexer>>, on_pick: impl Fn(Hit, &mut Window, &mut App) + 'static, window, cx)`; `Workspace::reveal_hit(&mut self, conn_id: &str, hit: Hit, window, cx)`.

- [ ] **Step 1: Ações e atalhos** em `main.rs`: acrescentar `QuickOpen, QuickPrev, QuickNext` ao `actions!` e:

```rust
KeyBinding::new("cmd-k", QuickOpen, Some("Workspace")),
// O campo usa ↑↓ para mover o cursor; a ligação mais profunda vence.
KeyBinding::new("up", QuickPrev, Some("QuickOpen > Input")),
KeyBinding::new("down", QuickNext, Some("QuickOpen > Input")),
```

- [ ] **Step 2: `quick_open.rs`** — view com `input`, `hits`, `selected`, `status`, `meta`, `indexer`, `on_pick`, `_search: Option<Task<()>>`, `_subs`.
  - `InputEvent::Change` → `self.search(cx)`; `InputEvent::PressEnter` → `self.pick(self.selected, window, cx)`.
  - `search`: texto < 2 chars → limpa. Com `indexer` (Snowflake): se `index` existe, `search_index(idx, &meta.cache, text)`; senão `status = "indexando…"`. Sem `indexer` (Postgres): `_search = Some(spawn(timer 250 ms → pg_search(&meta.session, text)))`, `status = "buscando…"`; resultado preenche `hits`, erro vai para `status`. `selected = 0`.
  - Observa o `indexer` (`cx.observe`) para refazer a busca quando o índice chega/atualiza.
  - `pick(i)`: `if let Some(h) = hits.get(i).cloned() { window.close_dialog(cx); (on_pick)(h, window, cx) }`.
  - `render`: `v_flex().key_context("QuickOpen")` com `on_action` para `QuickPrev`/`QuickNext` (move `selected` com limite), `Input::new(&self.input)`, lista de até 50 linhas (`div().id(("qo", i))`, `cursor_pointer`, fundo `theme.secondary` quando selecionada, `on_click` → `pick(i)`), cada linha: `Icon::new(icon).small()` (Database/Layers/Table/Eye/Sheet, como `schema_tree::Kind::icon`), nome, caminho em `muted_foreground` (`DB.SCHEMA`), tipo à direita ("database", "schema", "tabela", "view", "view mat."). Rodapé: `status`, e no Snowflake "índice: N objetos" + `Button::new("qo-refresh").label("↻").ghost()` → `indexer.update(|ix, cx| ix.refresh(cx))`; "nenhum resultado" quando busca ≥ 2 chars e `hits` vazio.
  - `open(..)`: `cx.new(QuickOpen::new)`, foca o input, `window.open_dialog(cx, |dialog, _, _| dialog.title("Buscar").w(px(640.)).child(view.clone()))`.

- [ ] **Step 3: `Workspace`**:

```rust
fn quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(o) = self.current_open() else {
        window.push_notification(Notification::error("abra uma conexão para buscar"), cx);
        return;
    };
    let (meta, indexer, id) = (o.meta.clone(), o.indexer.clone(), o.conn.cfg.id.clone());
    let ws = cx.entity().downgrade();
    quick_open::open(meta, indexer, move |hit, window, cx| {
        ws.update(cx, |ws, cx| ws.reveal_hit(&id, hit, window, cx)).ok();
    }, window, cx);
}

/// Mostra o objeto na árvore; tabela/view também abre a aba.
fn reveal_hit(&mut self, conn_id: &str, hit: Hit, window: &mut Window, cx: &mut Context<Self>) {
    let Some(tree) = self.open_conn(conn_id).map(|o| o.tree.clone()) else { return };
    self.current = Some(conn_id.to_string());
    self.collapsed = false;
    tree.update(cx, |t, cx| t.reveal(hit.tree_path(), cx));
    if matches!(hit.kind, HitKind::Table | HitKind::View)
        && let Some(schema) = hit.schema_key()
    {
        self.open_table(conn_id, schema, hit.name, window, cx);
    }
    cx.notify();
}
```

e `.on_action(cx.listener(|ws, _: &QuickOpen, window, cx| ws.quick_open(window, cx)))` no `render`.

- [ ] **Step 4: Build, testes, clippy**

Run: `cargo build -p rb-app && cargo test --workspace --lib --bins && cargo clippy --workspace --all-targets`
Expected: compila; testes passam; nenhum aviso novo nos arquivos tocados.

- [ ] **Step 5: Checklist manual** (`cargo run --release -p rb-app`)
  - Snowflake: ⌘K logo após conectar mostra "indexando…", depois resultados; `DEV_` não traz `DEVX…`.
  - Enter numa tabela de database não expandida: árvore abre até ela, seleciona, aba de tabela abre.
  - Filtro `!PROD` ativo: nada de PROD no ⌘K.
  - Criar tabela no Snowsight num schema aberto na árvore: aparece no ⌘K e na árvore em até 30 s.
  - ↑↓ movem a seleção, Esc fecha, ⌘K dentro do editor SQL abre o popup.
  - Postgres: ⌘K acha schema e tabela.
