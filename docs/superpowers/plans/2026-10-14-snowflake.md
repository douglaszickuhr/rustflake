# Conexões Snowflake — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Conectar o rustbeaver a contas Snowflake (SSO, key-pair, PAT) com editor, árvore de objetos, grade somente leitura e seletor de warehouse/role.

**Architecture:** `rb-core` ganha `dialect.rs` (regras de nome por banco) e `snowflake.rs` (conversão de valores, paginação, ator e sessão compartilhada sobre `snowflake-connector-rs`). `Session` mantém a API pública; o ator do Postgres fica intacto e o do Snowflake fala o mesmo protocolo de comandos. `MetaCache` ganha `dialect`, databases e contexto; metadados, autocomplete, árvore e aba de tabela escolhem o caminho pelo dialeto.

**Tech Stack:** Rust 1.96 (edition 2024), snowflake-connector-rs =1.1.0 (features `key-pair-auth`, `external-browser-sso`), tokio 1.53, gpui-kit 0.7.3.

**Spec:** `docs/superpowers/specs/2026-10-14-snowflake-design.md`
**Notas da API de UI:** `docs/superpowers/plans/gpui-api-notes.md`

## Global Constraints

- Só macOS. O caminho Postgres não muda de comportamento: a suíte atual (101 testes) continua passando.
- `snowflake-connector-rs = { version = "=1.1.0", default-features = false, features = ["key-pair-auth", "external-browser-sso"] }`.
- `rb-core` não depende de GPUI e não entra em pânico em caminho de dados: erros viram `DbError`.
- Thread de UI sem rede nem Keychain.
- `FETCH_ROWS` = 1.000 também no Snowflake.
- Snowflake: navegar na árvore só usa `SHOW`/`DESCRIBE` e funções de contexto (sem warehouse).
- Segredos: PAT na conta `<id>`, passphrase da `.p8` na conta `key:<id>`, serviço `dev.rustbeaver`.
- Testes de integração Snowflake só rodam com `RB_TEST_SNOWFLAKE_ACCOUNT`, `_USER`, `_WAREHOUSE`, `_ROLE`, `_DATABASE` e um de `_PRIVATE_KEY` / `_PAT`; criam e removem schemas `RB_TEST_<pid>_<tag>`.
- Atributo de teste no `rb-app`: `#[::core::prelude::v1::test]`.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. Nomes Snowflake em minúsculas ou com espaço (`"Order Items"`) — árvore, aba de tabela e autocomplete devem citar e achar o objeto (testes `dialect::tests`, `complete::tests::snowflake_*`).
2. Sessão Snowflake expirada no meio do uso — a próxima query refaz o login uma vez e segue (`SfShared::renew`; checklist manual).
3. `fetch_more` atrasado de um resultado anterior não consome o cursor atual (teste `stale_fetch_more_returns_empty`, Tarefa 3).
4. `DESCRIBE TABLE` numa view — colunas da view carregam (fallback `DESCRIBE VIEW`; teste de integração `metadata_of_test_schema`).
5. Resultado com blocos de tamanhos irregulares — paginação em lotes de 1.000 sem perder nem repetir linhas (teste `pager_reslices_irregular_chunks`).

---

### Task 1: Modelo, dialeto e erros

**Files:**
- Create: `crates/rb-core/src/dialect.rs`
- Modify: `crates/rb-core/src/lib.rs`, `crates/rb-core/src/config.rs`, `crates/rb-core/src/error.rs`, `crates/rb-core/src/secrets.rs`, `crates/rb-core/src/lexer.rs`
- Modify (compilar com o campo novo): `crates/rb-core/tests/common/mod.rs`, `crates/rb-core/tests/ssh.rs`, `crates/rb-app/src/conn_dialog.rs`

**Interfaces:**
- Produces:
  - `rb_core::config::SnowflakeConfig { account: String, auth: SnowflakeAuth, private_key: Option<String>, warehouse: Option<String>, role: Option<String>, schema: Option<String> }` (`Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`)
  - `rb_core::config::SnowflakeAuth { ExternalBrowser, KeyPair, Pat }` (serde `lowercase`, `Default = ExternalBrowser`)
  - `ConnectionConfig.snowflake: Option<SnowflakeConfig>`; `host`, `port` com `#[serde(default)]`
  - `rb_core::config::expand_home(path: &str) -> PathBuf`
  - `DbError::Snowflake { code: String, message: String }`
  - `rb_core::secrets::key_account(id: &str) -> String` (= `"key:<id>"`)
  - `rb_core::lexer::quote_ident_snowflake(name: &str) -> String`
  - `rb_core::dialect::Dialect { Postgres, Snowflake }` (`Debug, Clone, Copy, PartialEq, Eq, Hash, Default`) com `quote(self, &str) -> String`, `fold(self, raw_token: &str) -> String`, `qualified(self, schema_key: &str, table: &str) -> String`

- [ ] **Step 1: Testes que falham**

`crates/rb-core/src/dialect.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_quotes_only_what_would_change_meaning() {
        let d = Dialect::Snowflake;
        assert_eq!(d.quote("ORDERS"), "ORDERS");
        assert_eq!(d.quote("ORDER_2$"), "ORDER_2$");
        assert_eq!(d.quote("orders"), r#""orders""#);
        assert_eq!(d.quote("Order Items"), r#""Order Items""#);
        assert_eq!(d.quote("SELECT"), r#""SELECT""#);
        assert_eq!(d.quote(r#"A"B"#), r#""A""B""#);
        assert_eq!(Dialect::Postgres.quote("orders"), "orders");
    }

    #[test]
    fn fold_follows_each_database() {
        assert_eq!(Dialect::Snowflake.fold("orders"), "ORDERS");
        assert_eq!(Dialect::Snowflake.fold(r#""Order Items""#), "Order Items");
        assert_eq!(Dialect::Postgres.fold("Orders"), "orders");
        assert_eq!(Dialect::Postgres.fold(r#""Orders""#), "Orders");
    }

    #[test]
    fn qualified_names() {
        assert_eq!(Dialect::Snowflake.qualified("DB.PUBLIC", "T x"), r#""DB"."PUBLIC"."T x""#);
        assert_eq!(Dialect::Postgres.qualified("public", "t"), r#""public"."t""#);
    }
}
```

Em `crates/rb-core/src/config.rs`, `mod tests`: em `sample()` acrescente `snowflake: None,` e adicione:

```rust
    #[test]
    fn snowflake_block_round_trips_and_host_is_optional() {
        let path = tmp("snowflake");
        let mut c = sample();
        c.host = String::new();
        c.port = 0;
        c.snowflake = Some(SnowflakeConfig {
            account: "xy12345.us-east-1".into(),
            auth: SnowflakeAuth::KeyPair,
            private_key: Some("~/.ssh/sf.p8".into()),
            warehouse: Some("COMPUTE_WH".into()),
            role: None,
            schema: Some("PUBLIC".into()),
        });
        let cfg = ConfigFile { connections: vec![c] };
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[connection.snowflake]") && text.contains("auth = \"keypair\""), "{text}");
        assert_eq!(load(&path).unwrap(), cfg);

        std::fs::write(&path, "[[connection]]\nid = \"s\"\nname = \"n\"\ndatabase = \"\"\nuser = \"u\"\n[connection.snowflake]\naccount = \"a\"\n").unwrap();
        let loaded = load(&path).unwrap().connections.remove(0);
        assert_eq!((loaded.host.as_str(), loaded.port), ("", 0));
        assert_eq!(loaded.snowflake.unwrap().auth, SnowflakeAuth::ExternalBrowser);
    }

    #[test]
    fn expand_home_only_touches_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~/.ssh/k.p8"), home.join(".ssh/k.p8"));
        assert_eq!(expand_home("/abs/k.p8"), PathBuf::from("/abs/k.p8"));
    }
```

Em `crates/rb-core/src/lib.rs`: `pub mod dialect;` (depois de `decode`).

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --lib 2>&1 | grep -E "^error" -A3 | head -20`
Expected: `cannot find type Dialect`, `SnowflakeConfig`, `expand_home`.

- [ ] **Step 3: Implementar**

`crates/rb-core/src/lexer.rs`, depois de `quote_always`:

```rust
/// Palavras reservadas do Snowflake (não podem ser identificador sem aspas).
const RESERVED_SNOWFLAKE: &[&str] = &[
    "ALL", "ALTER", "AND", "ANY", "AS", "BETWEEN", "BY", "CASE", "CAST", "CHECK", "COLUMN", "CONNECT",
    "CONNECTION", "CONSTRAINT", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "CURRENT_USER", "DATABASE", "DELETE", "DISTINCT", "DROP", "ELSE", "EXISTS",
    "FALSE", "FOLLOWING", "FOR", "FROM", "FULL", "GRANT", "GROUP", "GSCLUSTER", "HAVING", "ILIKE", "IN",
    "INCREMENT", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "ISSUE", "JOIN", "LATERAL", "LEFT", "LIKE",
    "LOCALTIME", "LOCALTIMESTAMP", "MINUS", "NATURAL", "NOT", "NULL", "OF", "ON", "OR", "ORDER",
    "ORGANIZATION", "QUALIFY", "REGEXP", "REVOKE", "RIGHT", "RLIKE", "ROW", "ROWS", "SAMPLE", "SCHEMA",
    "SELECT", "SET", "SOME", "START", "TABLE", "TABLESAMPLE", "THEN", "TO", "TRIGGER", "TRUE", "TRY_CAST",
    "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW", "WHEN", "WHENEVER", "WHERE", "WINDOW", "WITH",
];

/// Cita só quando o Snowflake mudaria o nome sem aspas (ele dobra para maiúsculas).
pub fn quote_ident_snowflake(name: &str) -> String {
    let plain = name.bytes().next().is_some_and(|c| c.is_ascii_uppercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_' || c == b'$')
        && !RESERVED_SNOWFLAKE.contains(&name);
    if plain { name.to_string() } else { quote_always(name) }
}
```

`crates/rb-core/src/dialect.rs`, acima dos testes:

```rust
//! Regras de nome que mudam de um banco para outro.

use crate::lexer::{ident_text, quote_always, quote_ident, quote_ident_snowflake};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    #[default]
    Postgres,
    Snowflake,
}

impl Dialect {
    /// Identificador pronto para inserir no SQL, com aspas só quando preciso.
    pub fn quote(self, name: &str) -> String {
        match self {
            Dialect::Postgres => quote_ident(name),
            Dialect::Snowflake => quote_ident_snowflake(name),
        }
    }

    /// Nome que o banco enxerga a partir do token digitado (sem aspas dobra a caixa).
    pub fn fold(self, raw: &str) -> String {
        match self {
            Dialect::Postgres => ident_text(raw),
            Dialect::Snowflake if raw.starts_with('"') => ident_text(raw),
            Dialect::Snowflake => raw.to_uppercase(),
        }
    }

    /// `FROM` totalmente citado. No Snowflake a chave de schema é `DB.SCHEMA`.
    /// ponytail: database com "." no nome não é suportado.
    pub fn qualified(self, schema_key: &str, table: &str) -> String {
        match (self, schema_key.split_once('.')) {
            (Dialect::Snowflake, Some((db, s))) => format!("{}.{}.{}", quote_always(db), quote_always(s), quote_always(table)),
            _ => format!("{}.{}", quote_always(schema_key), quote_always(table)),
        }
    }
}
```

`crates/rb-core/src/config.rs`: acima de `ConnectionConfig`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnowflakeAuth {
    #[default]
    ExternalBrowser,
    KeyPair,
    Pat,
}

/// Conexão Snowflake. `ConnectionConfig.user` e `.database` são reaproveitados.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnowflakeConfig {
    pub account: String,
    #[serde(default)]
    pub auth: SnowflakeAuth,
    #[serde(default)]
    pub private_key: Option<String>,
    #[serde(default)]
    pub warehouse: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
}
```

Em `ConnectionConfig`: `#[serde(default)]` em `host` e `port`, e no fim:

```rust
    /// Presente = conexão Snowflake; `host`, `port`, `ssl` e `ssh` são ignorados.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snowflake: Option<SnowflakeConfig>,
```

Depois de `connections_path()`:

```rust
/// `~/x` → `$HOME/x`; o resto fica igual.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}
```

`crates/rb-core/src/error.rs`: variante depois de `HostKeyUnknown`:

```rust
    /// Erro devolvido pelo Snowflake (código numérico do Snowflake, ex. 001003).
    Snowflake { code: String, message: String },
```

e no `Display`: `DbError::Snowflake { code, message } => write!(f, "[{code}] {message}"),`.

`crates/rb-core/src/secrets.rs`, depois de `ssh_account`:

```rust
/// Conta do Keychain com a passphrase da chave privada Snowflake de uma conexão.
pub fn key_account(id: &str) -> String {
    format!("key:{id}")
}
```

Nos literais `ConnectionConfig { .. }` de `tests/common/mod.rs`, `tests/ssh.rs` e `conn_dialog.rs` (dois lugares), acrescente `snowflake: None,`.

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core --lib 2>&1 | grep "test result"; cargo build --workspace 2>&1 | grep -E "^error" | head -3`
Expected: `ok`, 0 failed; build sem erro.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat(core): modelo Snowflake, dialeto e erro próprio

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Conversão de valores e paginação

**Files:**
- Modify: `crates/rb-core/Cargo.toml`
- Create: `crates/rb-core/src/snowflake.rs`
- Modify: `crates/rb-core/src/lib.rs`

**Interfaces:**
- Consumes: `Batch::empty`, `Batch::push_row`, `DbError`, `metadata::{ColumnMeta, SchemaObjects}`.
- Produces (todos em `rb_core::snowflake`):
  - `pub fn cell_text(v: &CellValue) -> Option<String>`
  - `pub(crate) trait Chunks { fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<Vec<Option<String>>>>, DbError>> + Send; }`
  - `pub(crate) struct Pager<C> { .. }` com `Pager::new(src: C, ncols: usize)` e `async fn next_batch(&mut self, n: usize) -> Result<(Batch, bool), DbError>`
  - `pub fn parse_objects(names: &[String], rows: &[Vec<Option<String>>]) -> SchemaObjects`
  - `pub fn parse_functions(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<String>`
  - `pub fn parse_describe(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<ColumnMeta>`
  - `pub fn first_named(names: &[String], rows: &[Vec<Option<String>>], col: &str) -> Vec<String>`

- [ ] **Step 1: Dependências**

`crates/rb-core/Cargo.toml`:

```toml
[dependencies]
snowflake-connector-rs = { version = "=1.1.0", default-features = false, features = ["key-pair-auth", "external-browser-sso"] }

[dev-dependencies]
chrono = "0.4"
serde_json = "1"
```

(acrescente às seções existentes). Em `lib.rs`: `pub mod snowflake;` (depois de `session`).

Run: `cargo build -p rb-core 2>&1 | tail -1`
Expected: `Finished`.

- [ ] **Step 2: Testes que falham**

`crates/rb-core/src/snowflake.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};

    use super::*;

    #[test]
    fn values_become_grid_text() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
        let ntz = d.and_hms_milli_opt(9, 5, 7, 120).unwrap();
        assert_eq!(cell_text(&CellValue::Null), None);
        assert_eq!(cell_text(&CellValue::Boolean(true)).as_deref(), Some("true"));
        assert_eq!(cell_text(&CellValue::Integer(-170141183460469231731687303715884105728)).as_deref(), Some("-170141183460469231731687303715884105728"));
        assert_eq!(cell_text(&CellValue::Float(1.5)).as_deref(), Some("1.5"));
        assert_eq!(cell_text(&CellValue::String("olá".into())).as_deref(), Some("olá"));
        assert_eq!(cell_text(&CellValue::Date(d)).as_deref(), Some("2026-10-14"));
        assert_eq!(cell_text(&CellValue::Time(ntz.time())).as_deref(), Some("09:05:07.120"));
        assert_eq!(cell_text(&CellValue::TimestampNtz(ntz)).as_deref(), Some("2026-10-14 09:05:07.120"));
        assert_eq!(cell_text(&CellValue::TimestampNtz(d.and_hms_opt(0, 0, 0).unwrap())).as_deref(), Some("2026-10-14 00:00:00"));
        assert_eq!(cell_text(&CellValue::TimestampLtz(Utc.from_utc_datetime(&ntz))).as_deref(), Some("2026-10-14 09:05:07.120Z"));
        let tz = FixedOffset::east_opt(3 * 3600).unwrap().from_local_datetime(&ntz).unwrap();
        assert_eq!(cell_text(&CellValue::TimestampTz(tz)).as_deref(), Some("2026-10-14 09:05:07.120+03:00"));
        assert_eq!(cell_text(&CellValue::Json(serde_json::json!({"a": [1, 2]}))).as_deref(), Some(r#"{"a":[1,2]}"#));
    }

    struct Fake(VecDeque<Vec<Vec<Option<String>>>>);

    impl Chunks for Fake {
        async fn next_chunk(&mut self) -> Result<Option<Vec<Vec<Option<String>>>>, DbError> {
            Ok(self.0.pop_front())
        }
    }

    fn chunk(from: usize, n: usize) -> Vec<Vec<Option<String>>> {
        (from..from + n).map(|i| vec![Some(i.to_string())]).collect()
    }

    #[test]
    fn pager_reslices_irregular_chunks() {
        let mut p = Pager::new(Fake(VecDeque::from([chunk(0, 700), Vec::new(), chunk(700, 2300)])), 1);
        let mut seen = Vec::new();
        let mut flags = Vec::new();
        loop {
            let (b, more) = pollster::block_on(p.next_batch(1000)).unwrap();
            seen.extend((0..b.rows()).map(|r| String::from_utf8(b.cell(r, 0).unwrap().to_vec()).unwrap()));
            flags.push((b.rows(), more));
            if !more {
                break;
            }
        }
        assert_eq!(flags, vec![(1000, true), (1000, true), (1000, false)]);
        assert_eq!(seen, (0..3000).map(|i| i.to_string()).collect::<Vec<_>>());
    }

    #[test]
    fn pager_with_empty_result() {
        let mut p = Pager::new(Fake(VecDeque::new()), 2);
        let (b, more) = pollster::block_on(p.next_batch(1000)).unwrap();
        assert_eq!((b.rows(), more), (0, false));
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn row(v: &[&str]) -> Vec<Option<String>> {
        v.iter().map(|s| (!s.is_empty()).then(|| s.to_string())).collect()
    }

    #[test]
    fn show_objects_split_by_kind() {
        let n = names(&["created_on", "name", "database_name", "schema_name", "kind"]);
        let rows = vec![
            row(&["x", "ORDERS", "DB", "S", "TABLE"]),
            row(&["x", "V_ORDERS", "DB", "S", "VIEW"]),
            row(&["x", "MV", "DB", "S", "MATERIALIZED_VIEW"]),
        ];
        let o = parse_objects(&n, &rows);
        assert_eq!((o.tables, o.views, o.matviews), (vec!["ORDERS".into()], vec!["V_ORDERS".into()], vec!["MV".into()]));
    }

    #[test]
    fn show_functions_keep_signature() {
        let n = names(&["created_on", "name", "arguments"]);
        let rows = vec![row(&["x", "ADD1", "ADD1(NUMBER, VARCHAR) RETURN NUMBER"])];
        assert_eq!(parse_functions(&n, &rows), vec!["ADD1(NUMBER, VARCHAR)".to_string()]);
    }

    #[test]
    fn describe_reads_type_null_default_and_pk() {
        let n = names(&["name", "type", "kind", "null?", "default", "primary key", "unique key"]);
        let rows = vec![row(&["ID", "NUMBER(38,0)", "COLUMN", "N", "", "Y", "N"]), row(&["NOME", "VARCHAR(16777216)", "COLUMN", "Y", "'x'", "N", "N"])];
        let c = parse_describe(&n, &rows);
        assert_eq!((c[0].name.as_str(), c[0].type_name.as_str(), c[0].nullable, c[0].is_pk), ("ID", "NUMBER(38,0)", false, true));
        assert_eq!((c[1].nullable, c[1].default.as_deref(), c[1].is_pk), (true, Some("'x'"), false));
    }

    #[test]
    fn first_named_is_case_insensitive() {
        let n = names(&["created_on", "NAME"]);
        assert_eq!(first_named(&n, &[row(&["x", "A"]), row(&["y", "B"])], "name"), vec!["A".to_string(), "B".to_string()]);
    }
}
```

- [ ] **Step 3: Rodar e ver falhar**

Run: `cargo test -p rb-core --lib snowflake 2>&1 | grep -E "^error" -A3 | head`
Expected: `cannot find function cell_text`, `trait Chunks`, `struct Pager`.

- [ ] **Step 4: Implementar**

Acima do `mod tests` em `snowflake.rs`:

```rust
//! Snowflake: conversão de valores, paginação, metadados e sessão compartilhada.

use std::collections::VecDeque;
use std::future::Future;

use snowflake_connector_rs::{CellValue, VectorValue};

use crate::DbError;
use crate::batch::Batch;
use crate::metadata::{ColumnMeta, SchemaObjects};

type Rows = Vec<Vec<Option<String>>>;

/// Texto exibido na grade. Decimal fica exato; timestamps em ISO 8601.
pub fn cell_text(v: &CellValue) -> Option<String> {
    Some(match v {
        CellValue::Null => return None,
        CellValue::Boolean(b) => b.to_string(),
        CellValue::Integer(i) => i.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Decimal(d) => d.raw().to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Date(d) => d.format("%Y-%m-%d").to_string(),
        CellValue::Time(t) => t.format("%H:%M:%S%.f").to_string(),
        CellValue::TimestampNtz(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        CellValue::TimestampLtz(t) => t.format("%Y-%m-%d %H:%M:%S%.fZ").to_string(),
        CellValue::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string(),
        CellValue::Json(j) => j.to_string(),
        CellValue::Binary(b) => format!("\\x{}", b.as_bytes().iter().map(|x| format!("{x:02x}")).collect::<String>()),
        CellValue::Vector(VectorValue::Int(x)) => format!("{:?}", x.clone().into_vec()),
        CellValue::Vector(VectorValue::Float(x)) => format!("{:?}", x.clone().into_vec()),
        // CellValue é non_exhaustive: tipo novo aparece como texto de depuração em vez de sumir.
        other => format!("{other:?}"),
    })
}

/// Fonte de blocos de linhas (o cursor do conector; nos testes, uma fila).
pub(crate) trait Chunks {
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Rows>, DbError>> + Send;
}

/// Reparte blocos de tamanho qualquer em lotes de `n` linhas.
pub(crate) struct Pager<C> {
    src: C,
    buf: VecDeque<Vec<Option<String>>>,
    done: bool,
    ncols: usize,
}

impl<C: Chunks> Pager<C> {
    pub(crate) fn new(src: C, ncols: usize) -> Self {
        Pager { src, buf: VecDeque::new(), done: false, ncols }
    }

    async fn pull(&mut self) -> Result<(), DbError> {
        match self.src.next_chunk().await? {
            Some(rows) => self.buf.extend(rows),
            None => self.done = true,
        }
        Ok(())
    }

    /// Próximo lote e se ainda há linhas depois dele.
    pub(crate) async fn next_batch(&mut self, n: usize) -> Result<(Batch, bool), DbError> {
        while self.buf.len() < n && !self.done {
            self.pull().await?;
        }
        let take = n.min(self.buf.len());
        let mut batch = Batch::empty(self.ncols);
        for row in self.buf.drain(..take) {
            let cells: Vec<Option<&[u8]>> = row.iter().map(|c| c.as_deref().map(str::as_bytes)).collect();
            batch.push_row(&cells);
        }
        // Lote exato no fim de um bloco: confere se há outro para responder "more" certo.
        while self.buf.is_empty() && !self.done {
            self.pull().await?;
        }
        Ok((batch, !self.buf.is_empty()))
    }
}

fn col_index(names: &[String], col: &str) -> Option<usize> {
    names.iter().position(|n| n.eq_ignore_ascii_case(col))
}

fn field<'a>(names: &[String], row: &'a [Option<String>], col: &str) -> Option<&'a str> {
    col_index(names, col).and_then(|i| row.get(i)).and_then(|v| v.as_deref())
}

/// Valores de uma coluna de um `SHOW` (nomes de coluna sem diferenciar caixa).
pub fn first_named(names: &[String], rows: &[Vec<Option<String>>], col: &str) -> Vec<String> {
    rows.iter().filter_map(|r| field(names, r, col).map(String::from)).collect()
}

/// `SHOW OBJECTS IN SCHEMA`.
pub fn parse_objects(names: &[String], rows: &[Vec<Option<String>>]) -> SchemaObjects {
    let mut o = SchemaObjects::default();
    for r in rows {
        let (Some(name), kind) = (field(names, r, "name"), field(names, r, "kind").unwrap_or("TABLE")) else { continue };
        let list = if kind.contains("MATERIALIZED") {
            &mut o.matviews
        } else if kind.contains("VIEW") {
            &mut o.views
        } else {
            &mut o.tables
        };
        list.push(name.to_string());
    }
    o
}

/// `SHOW USER FUNCTIONS IN SCHEMA`: `arguments` = `NOME(TIPOS) RETURN TIPO`.
pub fn parse_functions(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<String> {
    first_named(names, rows, "arguments").into_iter().map(|a| a.split(" RETURN ").next().unwrap_or(&a).to_string()).collect()
}

/// `DESCRIBE TABLE` / `DESCRIBE VIEW`.
pub fn parse_describe(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<ColumnMeta> {
    rows.iter()
        .filter_map(|r| {
            let name = field(names, r, "name")?.to_string();
            let type_name = field(names, r, "type").unwrap_or("").to_string();
            Some(ColumnMeta {
                name,
                cast_type: type_name.clone(),
                type_name,
                nullable: field(names, r, "null?") != Some("N"),
                default: field(names, r, "default").map(String::from),
                is_pk: field(names, r, "primary key") == Some("Y"),
                generated: false,
            })
        })
        .collect()
}
```

- [ ] **Step 5: Rodar e ver passar**

Run: `cargo test -p rb-core --lib snowflake 2>&1 | grep -E "test result|panicked" -A3`
Expected: `ok. 7 passed`. Se a asserção de `%.f` divergir (ex.: zeros à direita), registre a ruling e ajuste o formato para o que o Snowflake mostra no Snowsight (`.120` → mantém 3 casas).

- [ ] **Step 6: Commit**

```bash
git add crates/rb-core Cargo.lock
git commit -m "feat(core): conversão de valores, paginação e parse de metadados Snowflake

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Sessão Snowflake compartilhada e ator

**Files:**
- Modify: `crates/rb-core/src/snowflake.rs`, `crates/rb-core/src/session.rs`
- Create: `crates/rb-core/tests/snowflake.rs`

**Interfaces:**
- Consumes: `Pager`, `Chunks`, `cell_text` (Tarefa 2); `SnowflakeConfig`, `SnowflakeAuth`, `expand_home`, `DbError::Snowflake`, `Dialect` (Tarefa 1).
- Produces:
  - `Credentials.key_passphrase: Option<String>` (campo novo)
  - `Session::dialect(&self) -> Dialect`
  - `Session::query_named(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), DbError>` (Postgres e Snowflake)
  - `rb_core::snowflake::login_count() -> usize` (contador de logins, para teste)
  - `Engine::connect` decide pelo `cfg.snowflake`

- [ ] **Step 1: Testes que falham (integração; pulam sem as variáveis)**

`crates/rb-core/tests/snowflake.rs`:

```rust
//! Contra uma conta Snowflake real. Variáveis: RB_TEST_SNOWFLAKE_{ACCOUNT,USER,WAREHOUSE,ROLE,DATABASE}
//! e RB_TEST_SNOWFLAKE_PRIVATE_KEY (caminho da .p8) ou RB_TEST_SNOWFLAKE_PAT.

use std::time::{Duration, Instant};

use pollster::block_on;
use rb_core::DbError;
use rb_core::config::{ConnectionConfig, SnowflakeAuth, SnowflakeConfig, SslMode};
use rb_core::session::{Credentials, Engine, Session};

struct Env {
    engine: Engine,
    cfg: ConnectionConfig,
    creds: Credentials,
    database: String,
}

fn env() -> Option<Env> {
    let v = |k: &str| std::env::var(format!("RB_TEST_SNOWFLAKE_{k}")).ok().filter(|s| !s.is_empty());
    let (key, pat) = (v("PRIVATE_KEY"), v("PAT"));
    let auth = match (&key, &pat) {
        (Some(_), _) => SnowflakeAuth::KeyPair,
        (None, Some(_)) => SnowflakeAuth::Pat,
        _ => return None,
    };
    let database = v("DATABASE")?;
    let cfg = ConnectionConfig {
        id: format!("sf-test-{}", std::process::id()),
        name: "sf".into(),
        host: String::new(),
        port: 0,
        database: database.clone(),
        user: v("USER")?,
        ssl: SslMode::Disable,
        ssh: None,
        snowflake: Some(SnowflakeConfig {
            account: v("ACCOUNT")?,
            auth,
            private_key: key,
            warehouse: v("WAREHOUSE"),
            role: v("ROLE"),
            schema: None,
        }),
    };
    let creds = Credentials { password: pat, ..Default::default() };
    Some(Env { engine: Engine::new().unwrap(), cfg, creds, database })
}

fn skip() {
    eprintln!("RB_TEST_SNOWFLAKE_* ausentes; teste Snowflake pulado");
}

fn connect(e: &Env) -> Session {
    block_on(e.engine.connect(&e.cfg, e.creds.clone())).expect("conectar ao Snowflake")
}

/// Cria um schema descartável e devolve `"DB"."RB_TEST_<pid>_<tag>"`.
fn scratch_schema(e: &Env, s: &Session, tag: &str) -> String {
    let name = format!("\"{}\".\"RB_TEST_{}_{}\"", e.database, std::process::id(), tag.to_uppercase());
    block_on(s.execute(format!("create or replace schema {name}"))).unwrap();
    name
}

fn drop_schema(s: &Session, name: &str) {
    let _ = block_on(s.execute(format!("drop schema if exists {name}")));
}

fn texts(s: &Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = block_on(s.execute(sql.into())).unwrap();
    (0..out.batch.rows())
        .map(|r| (0..out.columns.len()).map(|c| out.batch.cell(r, c).map(|b| String::from_utf8(b.to_vec()).unwrap())).collect())
        .collect()
}

#[test]
fn select_one() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    assert_eq!(texts(&s, "select 1 as x"), vec![vec![Some("1".to_string())]]);
}

#[test]
fn large_result_pages_in_order() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let out = block_on(s.execute("select seq4() as n from table(generator(rowcount => 5000)) order by n".into())).unwrap();
    let mut got: Vec<String> = (0..out.batch.rows()).map(|r| String::from_utf8(out.batch.cell(r, 0).unwrap().to_vec()).unwrap()).collect();
    assert_eq!(got.len(), 1000);
    let mut more = out.more;
    while more {
        let f = block_on(s.fetch_more(out.exec_id)).unwrap();
        assert!(f.batch.rows() <= 1000);
        got.extend((0..f.batch.rows()).map(|r| String::from_utf8(f.batch.cell(r, 0).unwrap().to_vec()).unwrap()));
        more = f.more;
    }
    assert_eq!(got, (0..5000).map(|i| i.to_string()).collect::<Vec<_>>());
}

#[test]
fn stale_fetch_more_returns_empty() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let a = block_on(s.execute("select seq4() from table(generator(rowcount => 3000))".into())).unwrap();
    let b = block_on(s.execute("select seq4() from table(generator(rowcount => 3000))".into())).unwrap();
    let stale = block_on(s.fetch_more(a.exec_id)).unwrap();
    assert_eq!((stale.batch.rows(), stale.more), (0, false));
    assert_eq!(block_on(s.fetch_more(b.exec_id)).unwrap().batch.rows(), 1000);
}

#[test]
fn cancel_interrupts_wait() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let started = Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            s.cancel();
        });
        block_on(s.execute("call system$wait(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Snowflake { .. })), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[test]
fn types_round_trip() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let row = texts(
        &s,
        "select 12345678901234567890.1234567890::number(38,10), 1.5::float, 'olá'::varchar, true, \
         '2026-10-14'::date, '09:05:07'::time, '2026-10-14 09:05:07.120'::timestamp_ntz, \
         '2026-10-14 09:05:07.120 +03:00'::timestamp_tz, parse_json('{\"a\":[1,2]}'), \
         to_binary('cafe', 'HEX'), null",
    )
    .remove(0);
    let expect = [
        Some("12345678901234567890.1234567890"),
        Some("1.5"),
        Some("olá"),
        Some("true"),
        Some("2026-10-14"),
        Some("09:05:07"),
        Some("2026-10-14 09:05:07.120"),
        Some("2026-10-14 09:05:07.120+03:00"),
        Some(r#"{"a":[1,2]}"#),
        Some("\\xcafe"),
        None,
    ];
    assert_eq!(row, expect.iter().map(|v| v.map(String::from)).collect::<Vec<_>>());
}

#[test]
fn syntax_error_has_snowflake_code() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let res = block_on(s.execute("selec 1".into()));
    assert!(matches!(&res, Err(DbError::Snowflake { code, .. }) if !code.is_empty()), "{res:?}");
}

#[test]
fn manual_transactions_and_grid_edits_are_refused() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    assert!(matches!(block_on(s.execute("begin".into())), Err(DbError::Unsupported(_))));
    assert!(matches!(block_on(s.run_in_transaction(vec![])), Err(DbError::Unsupported(_))));
}

#[test]
fn sessions_of_one_connection_share_one_login() {
    let Some(e) = env() else { return skip() };
    let before = rb_core::snowflake::login_count();
    let a = connect(&e);
    let b = connect(&e);
    assert_eq!(rb_core::snowflake::login_count() - before, 1);
    let schema = scratch_schema(&e, &a, "share");
    assert_eq!(texts(&b, "select current_schema()")[0][0].as_deref(), Some(schema.rsplit('.').next().unwrap().trim_matches('"')));
    drop_schema(&a, &schema);
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --test snowflake 2>&1 | grep -E "^error" -A3 | head`
Expected: `no function login_count`; e, com as variáveis definidas, nenhuma compilação ainda.

- [ ] **Step 3: Implementar a sessão compartilhada e o ator**

Em `snowflake.rs`, acrescente imports e o código (antes do `mod tests`):

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use snowflake_connector_rs as sf;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::batch::ColumnInfo;
use crate::config::{ConnectionConfig, SnowflakeAuth, expand_home};
use crate::session::{Cmd, Credentials, ExecOutcome, FETCH_ROWS, FetchOutcome, Reply};
use crate::split::is_transaction_control;

static LOGINS: AtomicUsize = AtomicUsize::new(0);

/// Quantos logins Snowflake o processo fez (testes verificam o compartilhamento).
pub fn login_count() -> usize {
    LOGINS.load(Ordering::Relaxed)
}

pub(crate) fn sf_err(e: sf::Error) -> DbError {
    match e.snowflake_code() {
        Some(code) => DbError::Snowflake { code: code.to_string(), message: e.snowflake_message().unwrap_or_default().to_string() },
        None if e.is_cancelled() => DbError::Snowflake { code: "cancelada".into(), message: "query cancelada".into() },
        None => DbError::Connection(e.to_string()),
    }
}

impl Chunks for sf::ResultCursor {
    async fn next_chunk(&mut self) -> Result<Option<Rows>, DbError> {
        let Some(table) = self.next_table().await.map_err(sf_err)? else { return Ok(None) };
        let mut rows = Vec::with_capacity(table.row_count());
        for row in table.dynamic_rows().map_err(sf_err)? {
            rows.push(row.map_err(sf_err)?.values().iter().map(cell_text).collect());
        }
        Ok(Some(rows))
    }
}

/// Uma sessão Snowflake por conexão do app, compartilhada por árvore e abas. Se expirar,
/// `renew` faz um novo login (no SSO, o navegador abre de novo).
pub(crate) struct SfShared {
    client: sf::Client,
    session: AsyncMutex<Arc<sf::Session>>,
}

impl SfShared {
    pub(crate) async fn login(cfg: &ConnectionConfig, creds: &Credentials) -> Result<SfShared, DbError> {
        let s = cfg.snowflake.as_ref().ok_or_else(|| DbError::Config("conexão sem bloco snowflake".into()))?;
        let auth = match s.auth {
            SnowflakeAuth::ExternalBrowser => sf::AuthConfig::external_browser(sf::ExternalBrowserConfig::default()),
            SnowflakeAuth::Pat => {
                let token = creds.password.clone().ok_or_else(|| DbError::Config("token PAT não informado".into()))?;
                sf::AuthConfig::password(sf::PasswordConfig::new(token))
            }
            SnowflakeAuth::KeyPair => {
                let path = s.private_key.as_deref().ok_or_else(|| DbError::Config("chave privada não informada".into()))?;
                let pem = std::fs::read_to_string(expand_home(path)).map_err(|e| DbError::Config(format!("{path}: {e}")))?;
                let key = match &creds.key_passphrase {
                    Some(p) => sf::KeyPairConfig::from_encrypted_pem(pem, p.as_bytes().to_vec()),
                    None => sf::KeyPairConfig::from_pem(pem),
                };
                sf::AuthConfig::key_pair(key)
            }
        };
        let mut session = sf::SessionConfig::new().with_session_parameter("CLIENT_SESSION_KEEP_ALIVE", serde_json::Value::Bool(true));
        if !cfg.database.is_empty() {
            session = session.with_database(&cfg.database);
        }
        for (v, f) in [
            (&s.warehouse, sf::SessionConfig::with_warehouse::<&String> as fn(_, _) -> _),
            (&s.schema, sf::SessionConfig::with_schema::<&String>),
            (&s.role, sf::SessionConfig::with_role::<&String>),
        ] {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                session = f(session, v);
            }
        }
        let client = sf::Client::new(sf::ClientConfig::new(&cfg.user, &s.account, auth).with_session(session)).map_err(sf_err)?;
        let first = client.create_session().await.map_err(sf_err)?;
        LOGINS.fetch_add(1, Ordering::Relaxed);
        Ok(SfShared { client, session: AsyncMutex::new(Arc::new(first)) })
    }

    pub(crate) async fn current(&self) -> Arc<sf::Session> {
        self.session.lock().await.clone()
    }

    /// Novo login, a não ser que outro ator já tenha renovado a sessão vencida.
    async fn renew(&self, stale: &Arc<sf::Session>) -> Result<Arc<sf::Session>, DbError> {
        let mut cur = self.session.lock().await;
        if Arc::ptr_eq(&cur, stale) {
            tracing::info!("sessão Snowflake expirada; novo login");
            *cur = Arc::new(self.client.create_session().await.map_err(sf_err)?);
            LOGINS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(cur.clone())
    }
}

pub(crate) type SfCanceller = Arc<Mutex<Option<sf::QueryCanceller>>>;

pub(crate) struct SfActor {
    pub(crate) shared: Arc<SfShared>,
    pub(crate) canceller: SfCanceller,
    pub(crate) last_exec: u64,
}

impl SfActor {
    pub(crate) async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut next = rx.recv().await;
        while let Some(cmd) = next.take() {
            next = match cmd {
                Cmd::Execute { sql, reply } => self.execute(sql, reply, &mut rx).await,
                Cmd::FetchMore { reply, .. } => {
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                    rx.recv().await
                }
                Cmd::QueryText { sql, reply, .. } => {
                    let _ = reply.send(self.query_named(&sql).await.map(|(_, rows)| rows));
                    rx.recv().await
                }
                Cmd::QueryNamed { sql, reply } => {
                    let _ = reply.send(self.query_named(&sql).await);
                    rx.recv().await
                }
                Cmd::Transaction { reply, .. } => {
                    let _ = reply.send(Err(DbError::Unsupported("Snowflake: somente leitura".into())));
                    rx.recv().await
                }
            };
        }
    }

    /// Envia o statement; se a sessão venceu, renova e tenta uma vez de novo.
    async fn submit(&self, sql: &str) -> Result<sf::ResultCursor, DbError> {
        let mut session = self.shared.current().await;
        for attempt in 0..2 {
            let handle = session.query_handle(sql).map_err(sf_err)?;
            *self.canceller.lock().expect("mutex do cancelador") = Some(handle.canceller());
            let res = handle.execute().await;
            *self.canceller.lock().expect("mutex do cancelador") = None;
            match res {
                Err(e) if e.is_session_expired() && attempt == 0 => session = self.shared.renew(&session).await?,
                other => return other.map_err(sf_err),
            }
        }
        unreachable!("o laço sempre retorna na segunda tentativa")
    }

    async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Rows), DbError> {
        let mut cursor = self.submit(sql).await?;
        let names = cursor.schema().columns().iter().map(|c| c.name().to_string()).collect();
        let mut rows = Vec::new();
        while let Some(chunk) = cursor.next_chunk().await? {
            rows.extend(chunk);
        }
        Ok((names, rows))
    }

    async fn execute(&mut self, sql: String, reply: Reply<ExecOutcome>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Option<Cmd> {
        let started = Instant::now();
        if is_transaction_control(&sql) {
            let msg = "transações manuais (BEGIN/START/SAVEPOINT): o rustbeaver roda em autocommit";
            let _ = reply.send(Err(DbError::Unsupported(msg.into())));
            return rx.recv().await;
        }
        self.last_exec += 1;
        let exec_id = self.last_exec;
        let cursor = match self.submit(&sql).await {
            Ok(c) => c,
            Err(e) => {
                let _ = reply.send(Err(e));
                return rx.recv().await;
            }
        };
        let columns: Arc<[ColumnInfo]> = cursor
            .schema()
            .columns()
            .iter()
            .map(|c| ColumnInfo { name: c.name().to_string(), type_: crate::Type::TEXT })
            .collect();
        let mut pager = Pager::new(cursor, columns.len());
        let (batch, mut more) = match pager.next_batch(FETCH_ROWS as usize).await {
            Ok(b) => b,
            Err(e) => {
                let _ = reply.send(Err(e));
                return rx.recv().await;
            }
        };
        let elapsed = started.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, rows = batch.rows(), "primeiro lote (Snowflake)");
        let _ = reply.send(Ok(ExecOutcome { exec_id, columns, batch, more, affected: None, elapsed }));
        while more {
            match rx.recv().await {
                Some(Cmd::FetchMore { exec_id: id, reply }) if id != exec_id => {
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                }
                Some(Cmd::FetchMore { reply, .. }) => match pager.next_batch(FETCH_ROWS as usize).await {
                    Ok((batch, m)) => {
                        more = m;
                        let _ = reply.send(Ok(FetchOutcome { batch, more }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return rx.recv().await;
                    }
                },
                other => return other,
            }
        }
        rx.recv().await
    }
}
```

Nota de compilação: se o array de `fn` em `login` não inferir os tipos, troque por três `if let` explícitos (`if let Some(w) = …warehouse { session = session.with_warehouse(w) }` etc.).

Em `session.rs`:

1. Torne públicos para o crate: `pub(crate) enum Cmd`, `pub(crate) type Reply<T>`, e acrescente a variante
   `QueryNamed { sql: String, reply: Reply<(Vec<String>, Vec<Vec<Option<String>>>)> }` (com braço em `Cmd::fail`).
2. `Credentials` ganha `pub key_passphrase: Option<String>`.
3. Cache de sessões Snowflake no `Engine`:

```rust
type SnowflakeCache = Mutex<HashMap<(String, String, String, SnowflakeConfig), Weak<SfShared>>>;
```

campo `sf_sessions: Arc<SnowflakeCache>` (inicializado com `Arc::default()`), e em `Engine::connect`, antes do caminho Postgres:

```rust
        if cfg.snowflake.is_some() {
            let (cfg, cache) = (cfg.clone(), self.sf_sessions.clone());
            let handle = self.rt.handle().clone();
            return self
                .rt
                .spawn(async move {
                    let shared = shared_snowflake(&cache, &cfg, &creds).await?;
                    Ok(Session::open_snowflake(shared, handle))
                })
                .await
                .map_err(|e| DbError::Connection(e.to_string()))?;
        }
```

```rust
/// ponytail: duas sessões abrindo ao mesmo tempo podem fazer dois logins; o segundo fica no cache.
async fn shared_snowflake(cache: &SnowflakeCache, cfg: &ConnectionConfig, creds: &Credentials) -> Result<Arc<SfShared>, DbError> {
    let s = cfg.snowflake.clone().expect("chamado só com bloco snowflake");
    let key = (cfg.id.clone(), cfg.user.clone(), cfg.database.clone(), s);
    if let Some(t) = cache.lock().expect("cache Snowflake").get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let shared = Arc::new(SfShared::login(cfg, creds).await?);
    cache.lock().expect("cache Snowflake").insert(key, Arc::downgrade(&shared));
    Ok(shared)
}
```

4. `Session` troca `cancel: Arc<Mutex<CancelToken>>` e `connector: Connector` por:

```rust
enum Canceller {
    Postgres { token: Arc<Mutex<CancelToken>>, connector: Connector },
    Snowflake(SfCanceller),
}

pub struct Session {
    tx: mpsc::UnboundedSender<Cmd>,
    rt: Handle,
    canceller: Canceller,
    dialect: Dialect,
    /// Mantém a sessão Snowflake viva enquanto esta `Session` existir.
    _snowflake: Option<Arc<SfShared>>,
}
```

`Session::open` (Postgres) monta `canceller: Canceller::Postgres { token: cancel, connector }`, `dialect: Dialect::Postgres`, `_snowflake: None`. Novo:

```rust
    fn open_snowflake(shared: Arc<SfShared>, rt: Handle) -> Session {
        let canceller: SfCanceller = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        rt.spawn(SfActor { shared: shared.clone(), canceller: canceller.clone(), last_exec: 0 }.run(rx));
        Session { tx, rt, canceller: Canceller::Snowflake(canceller), dialect: Dialect::Snowflake, _snowflake: Some(shared) }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Linhas e nomes de coluna (usado nos `SHOW`/`DESCRIBE` do Snowflake).
    pub async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), DbError> {
        let sql = sql.to_string();
        self.call(|reply| Cmd::QueryNamed { sql, reply }).await
    }
```

`Session::cancel`:

```rust
    pub fn cancel(&self) {
        match &self.canceller {
            Canceller::Postgres { token, connector } => {
                let (token, connector) = (token.lock().expect("mutex do cancel token").clone(), connector.clone());
                self.rt.spawn(async move {
                    if let Err(e) = connector.cancel(token).await {
                        tracing::warn!("cancelamento falhou: {e}");
                    }
                });
            }
            Canceller::Snowflake(c) => {
                if let Some(c) = c.lock().expect("mutex do cancelador").clone() {
                    self.rt.spawn(async move {
                        if let Err(e) = c.cancel().await {
                            tracing::warn!("cancelamento Snowflake falhou: {e}");
                        }
                    });
                }
            }
        }
    }
```

5. Ator Postgres: braço `Cmd::QueryNamed { sql, reply }` com

```rust
    async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), DbError> {
        let stmt = self.client.prepare(sql).await?;
        let names = stmt.columns().iter().map(|c| c.name().to_string()).collect();
        Ok((names, self.query_text(sql, &[]).await?))
    }
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core 2>&1 | grep -E "test result|FAILED"`
Expected: tudo `ok` (os testes Snowflake pulam sem variáveis).

Run (com as variáveis da conta de teste exportadas): `cargo test -p rb-core --test snowflake -- --test-threads=4 2>&1 | grep -E "test |test result|panicked" -A3`
Expected: `ok. 8 passed`. Ajuste formatos de `types_round_trip` só se o Snowflake devolver outra forma *válida*; registre a ruling.

Run: `RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace 2>&1 | grep "test result" | awk '{p+=$4; f+=$6} END {print p, f}'`
Expected: `0 failed` (Postgres intacto).

- [ ] **Step 5: Commit**

```bash
git add crates Cargo.lock
git commit -m "feat(core): sessões Snowflake compartilhadas, paginação e cancelamento

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Metadados Snowflake e autocomplete por dialeto

**Files:**
- Modify: `crates/rb-core/src/metadata.rs`, `crates/rb-core/src/complete.rs`, `crates/rb-core/src/table.rs`
- Modify: `crates/rb-core/tests/snowflake.rs`, `crates/rb-core/tests/table_edit.rs`, `crates/rb-app/src/table_tab.rs:121`

**Interfaces:**
- Consumes: `Session::dialect`, `Session::query_named`, `parse_*`, `first_named` (Tarefas 2–3), `Dialect` (Tarefa 1).
- Produces:
  - `MetaCache` campos novos: `dialect: Dialect`, `databases: Vec<String>`, `loaded_dbs: HashSet<String>`, `warehouse: Option<String>`, `role: Option<String>`
  - `MetaCache::schema_key(&self, s: &str) -> String`, `MetaCache::current_db(&self) -> Option<&str>`
  - `MetaRequest::Schemas(String)` (database Snowflake)
  - `metadata::Context { search_path: Vec<String>, warehouse: Option<String>, role: Option<String> }`; `metadata::databases(s) -> Result<Vec<String>>`, `metadata::context(s) -> Result<Context>`, `metadata::db_schemas(s, db) -> Result<Vec<String>>`
  - `metadata::{objects, columns, indexes, schemas}` passam a funcionar nos dois dialetos
  - `table::select_sql(from: &str, filter: Option<&str>, order: Option<(&str, bool)>) -> String` (recebe o `FROM` pronto)

- [ ] **Step 1: Testes que falham**

Em `complete.rs`, `mod tests`, acrescente:

```rust
    fn snowflake_cache() -> MetaCache {
        let mut c = MetaCache { dialect: crate::dialect::Dialect::Snowflake, ..Default::default() };
        c.search_path = vec!["DB.PUBLIC".into()];
        c.schemas = vec!["DB.PUBLIC".into(), "DB.RAW".into()];
        c.objects.insert(
            "DB.PUBLIC".into(),
            SchemaObjects { tables: vec!["ORDERS".into(), "Order Items".into()], ..Default::default() },
        );
        c.objects.insert("DB.RAW".into(), SchemaObjects { tables: vec!["EVENTS".into()], ..Default::default() });
        c.columns.insert(("DB.PUBLIC".into(), "ORDERS".into()), vec![col("ID", "NUMBER(38,0)"), col("status", "VARCHAR")]);
        c
    }

    #[test]
    fn snowflake_tables_and_columns_use_its_case_rules() {
        let c = snowflake_cache();
        let src = "select * from or";
        let got = complete(src, src.len(), &c);
        let inserts: Vec<&str> = got.candidates.iter().map(|x| x.insert.as_str()).collect();
        assert!(inserts.contains(&"ORDERS") && inserts.contains(&r#""Order Items""#), "{inserts:?}");

        let src = "select o. from orders o";
        let got = complete(src, "select o.".len(), &c);
        let inserts: Vec<&str> = got.candidates.iter().map(|x| x.insert.as_str()).collect();
        assert_eq!(inserts, vec!["ID", r#""status""#]);
    }

    #[test]
    fn snowflake_schema_qualifier_is_relative_to_current_database() {
        let c = snowflake_cache();
        let src = "select * from raw.";
        let got = complete(src, src.len(), &c);
        assert_eq!(got.candidates.iter().map(|x| x.insert.as_str()).collect::<Vec<_>>(), vec!["EVENTS"]);
        let src = "select * from ";
        let labels: Vec<String> = complete(src, src.len(), &c).candidates.iter().filter(|x| x.kind == CandidateKind::Schema).map(|x| x.label.clone()).collect();
        assert_eq!(labels, vec!["PUBLIC".to_string(), "RAW".to_string()]);
    }
```

Em `table.rs`, troque os testes de `select_sql` para o novo formato:

```rust
        assert_eq!(select_sql(&qualified("Sales", "Order Items"), None, None), r#"SELECT * FROM "Sales"."Order Items""#);
```

(e o mesmo padrão nos outros três `assert` que chamam `select_sql`, passando `&qualified("public", "t")`). Em `tests/table_edit.rs:29`: `select_sql(&qualified(&schema, "Order Items"), None, Some(("id", true)))` (importe `qualified`).

Em `tests/snowflake.rs`, acrescente:

```rust
#[test]
fn metadata_of_test_schema() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let schema = scratch_schema(&e, &s, "meta");
    block_on(s.execute(format!("create table {schema}.T (ID number primary key, \"nome\" varchar)"))).unwrap();
    block_on(s.execute(format!("create view {schema}.V as select ID from {schema}.T"))).unwrap();
    let key = format!("{}.RB_TEST_{}_META", e.database, std::process::id());

    let dbs = block_on(rb_core::metadata::databases(&s)).unwrap();
    assert!(dbs.contains(&e.database), "{dbs:?}");
    assert!(block_on(rb_core::metadata::db_schemas(&s, &e.database)).unwrap().contains(&key));
    let o = block_on(rb_core::metadata::objects(&s, &key)).unwrap();
    assert_eq!((o.tables, o.views), (vec!["T".to_string()], vec!["V".to_string()]));
    let cols = block_on(rb_core::metadata::columns(&s, &key, "T")).unwrap();
    assert_eq!(cols.iter().map(|c| (c.name.as_str(), c.is_pk)).collect::<Vec<_>>(), vec![("ID", true), ("nome", false)]);
    assert_eq!(block_on(rb_core::metadata::columns(&s, &key, "V")).unwrap().len(), 1);
    drop_schema(&s, &schema);
}

#[test]
fn use_role_and_warehouse_change_context() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let wh = e.cfg.snowflake.as_ref().unwrap().warehouse.clone().unwrap();
    block_on(s.execute(format!("use warehouse \"{wh}\""))).unwrap();
    block_on(s.execute(format!("use database \"{}\"", e.database))).unwrap();
    block_on(s.execute("use schema INFORMATION_SCHEMA".into())).unwrap();
    let ctx = block_on(rb_core::metadata::context(&s)).unwrap();
    assert_eq!(ctx.warehouse.as_deref(), Some(wh.as_str()));
    assert_eq!(ctx.search_path, vec![format!("{}.INFORMATION_SCHEMA", e.database)]);
    assert!(ctx.role.is_some());
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --lib complete 2>&1 | grep -E "^error" -A3 | head`
Expected: `struct MetaCache has no field named dialect`.

- [ ] **Step 3: Implementar metadados**

`metadata.rs`:

```rust
use std::collections::HashSet;

use crate::dialect::Dialect;
use crate::lexer::quote_always;
use crate::snowflake::{first_named, parse_describe, parse_functions, parse_objects};
```

`MetaCache` ganha (todos `Default`):

```rust
    pub dialect: Dialect,
    /// Snowflake: databases da conta e quais já tiveram os schemas carregados.
    pub databases: Vec<String>,
    pub loaded_dbs: HashSet<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
```

`MetaRequest` ganha `Schemas(String)`; em `MetaCache::has`: `MetaRequest::Schemas(db) => self.loaded_dbs.contains(db),`. Métodos novos:

```rust
    /// Database atual no Snowflake (início do search_path `DB.SCHEMA`).
    pub fn current_db(&self) -> Option<&str> {
        self.search_path.first().and_then(|s| s.split_once('.')).map(|(db, _)| db)
    }

    /// Chave de schema como está no cache: no Snowflake, `SCHEMA` vira `DB_ATUAL.SCHEMA`.
    pub fn schema_key(&self, s: &str) -> String {
        match (self.dialect, self.current_db()) {
            (Dialect::Snowflake, Some(db)) if !s.contains('.') => format!("{db}.{s}"),
            _ => s.to_string(),
        }
    }
```

Funções (Snowflake usa `SHOW`/`DESCRIBE`, sem warehouse):

```rust
fn sf_schema(key: &str) -> String {
    match key.split_once('.') {
        Some((db, s)) => format!("{}.{}", quote_always(db), quote_always(s)),
        None => quote_always(key),
    }
}

pub struct Context {
    pub search_path: Vec<String>,
    pub warehouse: Option<String>,
    pub role: Option<String>,
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
```

No início de `objects`, `columns`, `indexes` e `schemas`:

```rust
// objects:
    if s.dialect() == Dialect::Snowflake {
        let (n, rows) = s.query_named(&format!("show objects in schema {}", sf_schema(schema))).await?;
        let mut o = parse_objects(&n, &rows);
        let (n, rows) = s.query_named(&format!("show user functions in schema {}", sf_schema(schema))).await?;
        o.functions = parse_functions(&n, &rows);
        return Ok(o);
    }
// columns:
    if s.dialect() == Dialect::Snowflake {
        let name = format!("{}.{}", sf_schema(schema), quote_always(table));
        let (n, rows) = match s.query_named(&format!("describe table {name}")).await {
            Ok(r) => r,
            // DESCRIBE TABLE recusa views; a view tem o próprio DESCRIBE.
            Err(_) => s.query_named(&format!("describe view {name}")).await?,
        };
        return Ok(parse_describe(&n, &rows));
    }
// indexes:
    if s.dialect() == Dialect::Snowflake {
        return Ok(Vec::new());
    }
// schemas (no Snowflake a árvore parte das databases):
    if s.dialect() == Dialect::Snowflake {
        return Ok(Vec::new());
    }
```

`table.rs`: `select_sql` passa a receber o `FROM` pronto:

```rust
pub fn select_sql(from: &str, filter: Option<&str>, order: Option<(&str, bool)>) -> String {
    let mut sql = format!("SELECT * FROM {from}");
```

(resto igual). `crates/rb-app/src/table_tab.rs:121`: `select_sql(&self.meta.read(cx).cache.dialect.qualified(&self.schema, &self.table), Some(&filter), …)`.

- [ ] **Step 4: Autocomplete por dialeto**

Em `complete.rs`:
- Troque `use crate::lexer::{TokKind, Token, ident_text, is_unterminated, quote_ident, tokenize};` por `use crate::lexer::{TokKind, Token, ident_text, is_unterminated, tokenize};`.
- `table_refs(src, toks)` → `table_refs(src, toks, cache.dialect)`; dentro, a detecção de palavra-chave continua com `ident_text` (minúsculo), mas os nomes guardados em `TableRef` usam `d.fold(...)`:

```rust
fn table_refs(src: &str, toks: &[Token], d: crate::dialect::Dialect) -> Vec<TableRef> {
    let raw = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent).then(|| &src[t.start..t.end]);
    let word = |t: &Token| raw(t).map(ident_text);
    let name = |t: &Token| raw(t).map(|r| d.fold(r));
```

  e nas atribuições de `TableRef` (`table: first`, `r.schema = Some(...)`, `r.alias = Some(a)`) use `name(...)` para o valor guardado, mantendo `word(...)` só nas comparações com `NOT_ALIAS`/`"as"`/contexto.
- Em `complete`: `let qual = cache.dialect.fold(&src[q.start..q.end]);` e `let key = cache.schema_key(&qual);` com `cache.schemas.contains(&key)` / `push_objects(&mut out, cache, &key)`.
- Candidatos de schema (contexto de tabela): no Snowflake, só os do database atual, com o rótulo sem o prefixo:

```rust
        for s in &cache.schemas {
            let label = match (cache.dialect, cache.current_db()) {
                (crate::dialect::Dialect::Snowflake, Some(db)) => match s.strip_prefix(db).and_then(|r| r.strip_prefix('.')) {
                    Some(rest) => rest.to_string(),
                    None => continue,
                },
                _ => s.clone(),
            };
            out.candidates.push(Candidate { insert: cache.dialect.quote(&label), label, kind: CandidateKind::Schema, detail: None });
        }
```

- `push_columns`: `let schema = r.schema.as_deref().map(|s| cache.schema_key(s)).unwrap_or_else(|| resolve_schema(cache, &r.table));` e `insert: cache.dialect.quote(&c.name)`.
- `push_objects`: `insert: cache.dialect.quote(name)` nos três lugares.
- `filter_and_rank` compara em minúsculas (já faz); o prefixo vem minúsculo, então `or` acha `ORDERS`.

- [ ] **Step 5: Rodar e ver passar**

Run: `cargo test -p rb-core 2>&1 | grep -E "test result|FAILED"; cargo build --workspace 2>&1 | grep -E "^error" -A5 | head`
Expected: tudo `ok`; build sem erro.

Run (com a conta de teste): `cargo test -p rb-core --test snowflake 2>&1 | grep "test result"`
Expected: `ok. 10 passed`.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "feat(core): metadados Snowflake e autocomplete com as regras de nome de cada banco

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Diálogo e ativação da conexão Snowflake

**Files:**
- Modify: `crates/rb-app/src/conn_dialog.rs`, `crates/rb-app/src/workspace.rs`, `crates/rb-app/src/meta.rs`

**Interfaces:**
- Consumes: `SnowflakeConfig`, `SnowflakeAuth`, `secrets::key_account`, `Credentials.key_passphrase`, `metadata::{databases, db_schemas, context}`, `MetaRequest::Schemas`.
- Produces: `conn_dialog::parse_snowflake(account, auth, key, warehouse, role, schema) -> Result<SnowflakeConfig, String>`; `MetaStore::reload_roots` carrega databases e contexto no Snowflake.

- [ ] **Step 1: Teste que falha**

No `mod tests` de `conn_dialog.rs`:

```rust
    #[::core::prelude::v1::test]
    fn snowflake_fields() {
        let c = parse_snowflake("xy12345.us-east-1", SnowflakeAuth::KeyPair, "~/.ssh/sf.p8", "WH", "", "PUBLIC").unwrap();
        assert_eq!(c.private_key.as_deref(), Some("~/.ssh/sf.p8"));
        assert_eq!((c.warehouse.as_deref(), c.role, c.schema.as_deref()), (Some("WH"), None, Some("PUBLIC")));
        assert!(parse_snowflake("", SnowflakeAuth::Pat, "", "", "", "").is_err(), "conta obrigatória");
        assert!(parse_snowflake("a", SnowflakeAuth::KeyPair, "", "", "", "").is_err(), "keypair sem chave");
    }
```

Run: `cargo test -p rb-app conn_dialog 2>&1 | grep -E "^error" -A3 | head`
Expected: `cannot find function parse_snowflake`.

- [ ] **Step 2: Implementar o diálogo**

Em `conn_dialog.rs`:

```rust
fn parse_snowflake(account: &str, auth: SnowflakeAuth, key: &str, warehouse: &str, role: &str, schema: &str) -> Result<SnowflakeConfig, String> {
    if account.is_empty() {
        return Err("informe a conta Snowflake (ex.: xy12345.us-east-1)".into());
    }
    if auth == SnowflakeAuth::KeyPair && key.is_empty() {
        return Err("key-pair precisa do caminho da chave privada (.p8)".into());
    }
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
    Ok(SnowflakeConfig { account: account.into(), auth, private_key: opt(key), warehouse: opt(warehouse), role: opt(role), schema: opt(schema) })
}
```

`Fields` ganha `kind: Rc<Cell<bool>>` (true = Snowflake), `sf_auth: Rc<Cell<SnowflakeAuth>>` e inputs `sf_account`, `sf_key`, `sf_warehouse`, `sf_role`, `sf_schema`, `sf_key_pass` (mascarado). `read` passa a devolver `(ConnectionConfig, String, String)` onde o terceiro é a passphrase SSH **ou** a da chave Snowflake, conforme o tipo; com Snowflake:

```rust
        if self.kind.get() {
            let snowflake = parse_snowflake(&v(&self.sf_account), self.sf_auth.get(), &v(&self.sf_key), &v(&self.sf_warehouse), &v(&self.sf_role), &v(&self.sf_schema))?;
            if v(&self.name).is_empty() || v(&self.user).is_empty() {
                return Err("preencha nome e usuário".into());
            }
            let cfg = ConnectionConfig {
                id: self.id.clone(),
                name: v(&self.name),
                host: String::new(),
                port: 0,
                database: v(&self.database),
                user: v(&self.user),
                ssl: SslMode::Disable,
                ssh: None,
                snowflake: Some(snowflake),
            };
            return Ok((cfg, self.password.read(cx).value().to_string(), self.sf_key_pass.read(cx).value().to_string()));
        }
```

No `open`, os valores iniciais vêm de `c.snowflake` quando existir (`kind = existing.snowflake.is_some()`). No `open_dialog`, troque o builder para escolher os campos pelo tipo (o builder roda a cada render):

```rust
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let (f, ws) = (f.clone(), ws.clone());
        let snowflake = f.kind.get();
        let tipo = {
            let (k1, k2) = (f.kind.clone(), f.kind.clone());
            h_flex()
                .gap_2()
                .child(Button::new("kind-pg").label("Postgres").selected(!snowflake).on_click(move |_, w, _| { k1.set(false); w.refresh(); }))
                .child(Button::new("kind-sf").label("Snowflake").selected(snowflake).on_click(move |_, w, _| { k2.set(true); w.refresh(); }))
        };
        let mut form = v_form().child(field().label("Tipo").child(tipo)).child(field().label("Nome").child(Input::new(&f.name)));
        if snowflake {
            let auth = f.sf_auth.get();
            let pick = |id: &'static str, label: &'static str, a: SnowflakeAuth| {
                let cell = f.sf_auth.clone();
                Button::new(id).label(label).selected(auth == a).on_click(move |_, w, _| { cell.set(a); w.refresh(); })
            };
            form = form
                .child(field().label("Conta (ex.: xy12345.us-east-1)").child(Input::new(&f.sf_account)))
                .child(field().label("Usuário").child(Input::new(&f.user)))
                .child(field().label("Autenticação").child(
                    h_flex()
                        .gap_2()
                        .child(pick("auth-sso", "SSO navegador", SnowflakeAuth::ExternalBrowser))
                        .child(pick("auth-key", "Key-pair", SnowflakeAuth::KeyPair))
                        .child(pick("auth-pat", "PAT", SnowflakeAuth::Pat)),
                ))
                .when(auth == SnowflakeAuth::KeyPair, |d| {
                    d.child(field().label("Chave privada (.p8)").child(Input::new(&f.sf_key)))
                        .child(field().label(format!("Passphrase da chave{keep}")).child(Input::new(&f.sf_key_pass).mask_toggle()))
                })
                .when(auth == SnowflakeAuth::Pat, |d| d.child(field().label(format!("Token PAT{keep}")).child(Input::new(&f.password).mask_toggle())))
                .child(field().label("Warehouse").child(Input::new(&f.sf_warehouse)))
                .child(field().label("Role").child(Input::new(&f.sf_role)))
                .child(field().label("Database").child(Input::new(&f.database)))
                .child(field().label("Schema").child(Input::new(&f.sf_schema)));
        } else {
            form = form /* campos Postgres atuais: Host, Porta, Banco, Usuário, Senha, SSL e os de SSH, como hoje */;
        }
        dialog.title(title).w(px(480.)).child(form).footer(/* igual ao atual */)
    });
```

(Os campos Postgres são exatamente os `.child(...)` que hoje vão no `v_form()`; mova-os para o `else`. `Button::selected` vem de `gpui_kit::component::Selectable`: importe `use gpui_kit::component::Selectable as _;`.)

- [ ] **Step 3: Segredos e ativação**

`workspace.rs`:
- `save_connection`: com Snowflake, o terceiro valor é a passphrase da chave e vai para `secrets::key_account(&id)`; sem Snowflake, a conta `key:` é apagada. O `password` (PAT) segue na conta `<id>`, como a senha.

```rust
        let is_sf = cfg.snowflake.is_some();
        // dentro do background_spawn, no lugar do bloco de passphrase SSH:
                        let (keep, drop) = if is_sf { (secrets::key_account(&id), secrets::ssh_account(&id)) } else { (secrets::ssh_account(&id), secrets::key_account(&id)) };
                        let wants_secret = if is_sf { true } else { has_ssh };
                        if !wants_secret {
                            secrets::delete_password(&keep)?;
                        } else if !passphrase.is_empty() {
                            secrets::set_password(&keep, &passphrase)?;
                        }
                        secrets::delete_password(&drop)?;
```

- `connect`: `Credentials { password, ssh_passphrase: if has_ssh {…} else { None }, key_passphrase: if is_sf { secrets::get_password(&secrets::key_account(&id))? } else { None } }`.
- `remove_connection`: também `secrets::delete_password(&secrets::key_account(&id))`.

`meta.rs`:
- `MetaStore::new`: `this.cache.dialect = session.dialect();` antes de `reload_roots`.
- `reload_roots`: usa `metadata::context` e, no Snowflake, `metadata::databases`:

```rust
            let res = async {
                let dbs = if s.dialect() == rb_core::dialect::Dialect::Snowflake { metadata::databases(&s).await? } else { Vec::new() };
                Ok::<_, rb_core::DbError>((metadata::schemas(&s).await?, dbs, metadata::context(&s).await?))
            }
            .await;
            // no update:
                    Ok((schemas, dbs, ctx)) => {
                        if m.cache.dialect == rb_core::dialect::Dialect::Postgres {
                            m.cache.schemas = schemas;
                        }
                        m.cache.databases = dbs;
                        m.cache.search_path = ctx.search_path;
                        m.cache.warehouse = ctx.warehouse;
                        m.cache.role = ctx.role;
                    }
```

- `request`: novo braço `MetaRequest::Schemas(db) => metadata::db_schemas(&s, db).await.map(Loaded::Schemas)` com `Loaded::Schemas(Vec<String>)`, e no update:

```rust
                    (Ok(Loaded::Schemas(list)), MetaRequest::Schemas(db)) => {
                        m.cache.schemas.retain(|s| !s.starts_with(&format!("{db}.")));
                        m.cache.schemas.extend(list);
                        m.cache.schemas.sort();
                        m.cache.loaded_dbs.insert(db);
                    }
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-app 2>&1 | grep "test result"; cargo build -p rb-app 2>&1 | grep -E "^(error|warning: unused)" -A5 | head`
Expected: `ok` (7 testes); sem erros.

- [ ] **Step 5: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): conexão Snowflake no diálogo, segredos e carga de databases

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Árvore, aba somente leitura e seletores de warehouse/role

**Files:**
- Modify: `crates/rb-app/src/schema_tree.rs`, `crates/rb-app/src/table_tab.rs`, `crates/rb-app/src/workspace.rs`, `crates/rb-app/src/sql_tab.rs`, `crates/rb-app/src/main.rs`
- Modify: `docs/manual-checklist.md`

**Interfaces:**
- Consumes: `MetaCache::{dialect, databases, loaded_dbs, warehouse, role}`, `MetaRequest::Schemas`, `Dialect::{qualified, quote}`, `Session::query_named`.
- Produces: `Kind::Database`; `Workspace::pick_context(kind: &'static str, …)`.

- [ ] **Step 1: Teste que falha**

No `mod tests` de `schema_tree.rs`:

```rust
    #[::core::prelude::v1::test]
    fn snowflake_tree_starts_at_databases() {
        let mut c = cache();
        c.dialect = rb_core::dialect::Dialect::Snowflake;
        c.databases = vec!["DB".into(), "OUTRO".into()];
        c.schemas = vec!["DB.PUBLIC".into()];
        c.loaded_dbs.insert("DB".into());
        let items = build_items(&c, &HashSet::new());
        assert_eq!(items.iter().map(|i| i.label.to_string()).collect::<Vec<_>>(), vec!["DB", "OUTRO"]);
        assert_eq!(items[0].children.iter().map(|i| i.label.to_string()).collect::<Vec<_>>(), vec!["PUBLIC"]);
        assert_eq!(describe(&id(&["d", "DB"]), &c).0, Kind::Database);
        assert_eq!(items[1].children[0].label.to_string(), "carregando…");
    }
```

Run: `cargo test -p rb-app schema_tree 2>&1 | grep -E "^error" -A3 | head`
Expected: `no variant Database` / campos de `children`/`label` (se `TreeItem` não expõe `children` como campo público, troque a asserção por `items[0].children()` conforme a API de `gpui-base::tree::TreeItem`; registre a ruling).

- [ ] **Step 2: Árvore**

`schema_tree.rs`:
- `Kind::Database` com ícone `IconName::Database`; em `describe`: `["d", _] => Kind::Database,`.
- `build_items`: extraia o corpo do `map` de schema para `fn schema_item(c, key: &str, label: &str, expanded) -> TreeItem` (id `["s", key]`, rótulo `label`). No Postgres, `c.schemas.iter().map(|s| schema_item(c, s, s, expanded))`. No Snowflake:

```rust
    if c.dialect == Dialect::Snowflake {
        return c
            .databases
            .iter()
            .map(|db| {
                let did = id(&["d", db]);
                let prefix = format!("{db}.");
                let children = if c.loaded_dbs.contains(db) {
                    c.schemas.iter().filter_map(|k| k.strip_prefix(&prefix).map(|s| schema_item(c, k, s, expanded))).collect()
                } else {
                    vec![loading(&did)]
                };
                TreeItem::new(did.clone(), db.clone()).expanded(open(&did)).children(children)
            })
            .collect();
    }
```

- `relation`: no Snowflake não monta o grupo de índices (`if c.dialect == Dialect::Postgres && let Some(idx) = idx`).
- `on_tree_event` (`Expanded`): `["d", db] => m.request(MetaRequest::Schemas(db.to_string()), cx),`; em `["r", schema, rel]`, só pede `Indexes` no Postgres.
- `refresh`: `["d", db, ..] => { m.cache.loaded_dbs.remove(*db); m.request(MetaRequest::Schemas(db.to_string()), cx) }`.
- `main.rs`: `Database` já está em `ExtraIcons`.

- [ ] **Step 3: Aba de tabela somente leitura**

`table_tab.rs`:
- `on_meta`: `let editable = !target.pk().is_empty() && self.meta.read(cx).cache.dialect == Dialect::Postgres;`
- `render`: mensagem do aviso:

```rust
        let sf = self.meta.read(cx).cache.dialect == Dialect::Snowflake;
        // …
                    .when(read_only, |d| {
                        let msg = if sf { "Snowflake: somente leitura" } else { "somente leitura: tabela sem chave primária" };
                        d.child(div().text_color(cx.theme().muted_foreground).child(msg))
                    })
```

- [ ] **Step 4: Contexto após `USE` e seletores**

`sql_tab.rs`: guarde `meta: Entity<MetaStore>` no `SqlTab`. Em `finish_ok`, se algum statement executado começa com `use ` (minúsculas, sem espaços à esquerda), recarregue o contexto:

```rust
        if self.ran_use {
            self.meta.update(cx, |m, cx| m.reload_roots(cx));
        }
```

(`ran_use` calculado em `run` a partir dos `ranges`: `ranges.iter().any(|r| src[r.clone()].trim_start().get(..4).is_some_and(|w| w.eq_ignore_ascii_case("use ")))`.)

`workspace.rs`, em `render_sidebar`, logo abaixo da lista de conexões quando a ativa for Snowflake:

```rust
            .when_some(self.active.as_ref().filter(|a| a.meta.read(cx).cache.dialect == Dialect::Snowflake), |d, a| {
                let c = &a.meta.read(cx).cache;
                let wh = c.warehouse.clone().unwrap_or_else(|| "—".into());
                let role = c.role.clone().unwrap_or_else(|| "—".into());
                d.child(
                    h_flex()
                        .gap_1()
                        .child(Button::new("pick-wh").label(format!("WH: {wh}")).ghost().on_click(cx.listener(|ws, _, w, cx| ws.pick_context("warehouse", w, cx))))
                        .child(Button::new("pick-role").label(format!("Role: {role}")).ghost().on_click(cx.listener(|ws, _, w, cx| ws.pick_context("role", w, cx)))),
                )
            })
```

```rust
    /// Lista warehouses ou roles (`SHOW …`) e roda `USE …` na escolhida; a sessão é compartilhada.
    fn pick_context(&mut self, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.active.as_ref().map(|a| a.meta.clone()) else { return };
        let session = meta.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let listed = session.query_named(&format!("show {kind}s")).await.map(|(n, rows)| rb_core::snowflake::first_named(&n, &rows, "name"));
            this.update_in(cx, |_, window, cx| match listed {
                Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                Ok(names) => {
                    let (session, meta) = (session.clone(), meta.clone());
                    window.open_dialog(cx, move |dialog, _, _| {
                        let list = names.iter().enumerate().fold(v_flex().gap_1(), |l, (i, n)| {
                            let (session, meta, n) = (session.clone(), meta.clone(), n.clone());
                            l.child(Button::new(("ctx", i)).label(n.clone()).ghost().on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                let (session, meta, sql) = (session.clone(), meta.clone(), format!("use {kind} {}", rb_core::lexer::quote_always(&n)));
                                window
                                    .spawn(cx, async move |cx| {
                                        let res = session.execute(sql).await;
                                        cx.update(|window, cx| match res {
                                            Ok(_) => meta.update(cx, |m, cx| m.reload_roots(cx)),
                                            Err(e) => window.push_notification(Notification::error(e.to_string()), cx),
                                        })
                                    })
                                    .detach();
                            }))
                        });
                        dialog.title(format!("Escolher {kind}")).w(px(360.)).child(list)
                    });
                }
            })
        })
        .detach();
    }
```

(Se `window.spawn` tiver outra assinatura no gpui-pre 0.3.9, use `cx.spawn` do contexto do `Workspace` via `ws.update`; registre a ruling.)

- [ ] **Step 5: Rodar, conferir na tela e commit**

Run: `cargo test -p rb-app 2>&1 | grep "test result"; cargo build -p rb-app 2>&1 | grep -cE "^error"`
Expected: `ok` (8 testes); `0`.

Run: `RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace 2>&1 | grep "test result" | awk '{p+=$4; f+=$6} END {print p, f}'`
Expected: `0 failed`.

Verificação visual com a instância isolada (`HOME` temporário, conexão Postgres de teste): árvore Postgres igual a antes. Com a conta Snowflake de teste (key-pair ou PAT, em `HOME` temporário): databases na raiz, schema → tabelas → colunas, aba somente leitura, seletores trocam warehouse/role.

Acrescente a `docs/manual-checklist.md`:

```markdown
## Snowflake

- [ ] Diálogo: "Snowflake" troca os campos; key-pair mostra chave e passphrase; PAT mostra o token.
- [ ] SSO: ativar a conexão abre o navegador uma vez; abrir abas e a árvore não abre de novo.
- [ ] Árvore: databases → schemas → Tabelas/Views/Views materializadas/Funções → colunas com PK em amarelo; sem "Índices".
- [ ] Duplo clique abre a tabela; filtro e ordenação funcionam; aviso "Snowflake: somente leitura" e sem botões de edição.
- [ ] WH/Role: os botões listam e trocam; um `USE ROLE` no editor atualiza os botões.
- [ ] ⌘. cancela `call system$wait(60)`.
- [ ] Autocomplete sugere tabelas do schema atual sem aspas (ORDERS) e com aspas quando preciso ("Order Items").
- [ ] Deixar o app aberto > 4 h com SSO: a próxima query refaz o login.
```

```bash
git add crates/rb-app docs/manual-checklist.md
git commit -m "feat(app): árvore Snowflake, aba somente leitura e seletores de warehouse/role

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
