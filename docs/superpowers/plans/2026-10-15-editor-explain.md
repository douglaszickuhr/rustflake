# Editor (formatar, histórico, abas de resultado) e EXPLAIN visual — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Formatar SQL (⌘⇧F), histórico por conexão (⌘Y), uma aba de resultado por statement e o plano de execução desenhado como diagrama (⌘E/⌘⇧E) no Postgres e no Snowflake.

**Architecture:** Lógica pura e I/O no `rb-core` (`format.rs`, `history.rs`, `explain.rs`, `Session::explain`), testada sem UI. No `rb-app`, o `SqlTab` passa a ter uma lista de painéis (resultados, mensagens, plano); `history_popup.rs` segue o molde do `quick_open.rs`; `plan_view.rs` desenha o diagrama com `div`s absolutas e um `canvas` para as setas.

**Tech Stack:** Rust 2024, gpui-kit 0.7.3, tokio-postgres 0.7, snowflake-connector-rs 1.1.0, `sqlformat` 0.5 (novo), `serde_json`.

**Spec:** `docs/superpowers/specs/2026-10-15-editor-explain-design.md`

## Global Constraints

- Palavras-chave em MAIÚSCULAS, indentação de 2 espaços; estilo do formatador não configurável.
- Histórico em `app_dir()/history/<conn_id>.jsonl`, permissão `0600`, `MAX_ENTRIES = 5_000`.
- Atalhos: ⌘⇧F formatar, ⌘⌥F localizar/substituir, ⌘Y histórico, ⌘E plano estimado, ⌘⇧E plano real, ⌘+/⌘−/⌘0 zoom.
- Plano real no Postgres sempre termina em ROLLBACK; falha no ROLLBACK descarta a conexão.
- EXPLAIN só para `SELECT`, `WITH`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `VALUES`, `TABLE`.
- Todo texto novo de UI em `locales/app.toml` com `en`, `pt`, `es` (o teste `every_key_is_translated_and_used_keys_exist` cobra).
- Estilo do repositório: linhas longas, sem `rustfmt`; comentários em português.
- Commits terminam com `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Rulings (decididos ao planejar)

- **R1 — Snowflake real só SELECT/WITH:** a sessão Snowflake é compartilhada entre as abas da conexão (`SfShared`); um `BEGIN…ROLLBACK` desfaria DML de outras abas. Plano real Snowflake aceita só leituras (`is_read_only_query`); DML recebe "use ⌘E (estimado) para DML no Snowflake". Custo se errado: o usuário não vê tempos reais de DML no Snowflake.
- **R2 — `simple_query` para o EXPLAIN no Postgres:** a coluna `QUERY PLAN` é `json`, que `try_get::<String>` não lê; o protocolo simples devolve texto.

## Review Focus

1. SQL com `$$ … $$` (função PL/pgSQL) e `--` dentro de string: o formatador não pode mudar o significado → teste em Task 1.
2. ⌘⇧E num `DELETE` no Postgres: a tabela fica intacta, mesmo se o EXPLAIN falhar no meio → teste de integração em Task 5.
3. Histórico com SQL de várias linhas, aspas e emoji: volta idêntico → teste em Task 2.
4. Plano com nó repetido em loop (Nested Loop, `Actual Loops > 1`): tempo exclusivo não fica negativo → teste em Task 3.
5. Script com erro no 2º de 3 statements: abas do 1º ficam e Mensagens mostra o erro → teste de `panes` em Task 6.

---

### Task 1: `explainable` e formatador

**Files:**
- Modify: `crates/rb-core/src/split.rs` (nova `explainable`)
- Create: `crates/rb-core/src/format.rs`
- Modify: `crates/rb-core/src/lib.rs` (`pub mod format;`), `crates/rb-core/Cargo.toml` (`sqlformat = "0.5"`)

**Interfaces:**
- Produces: `rb_core::split::explainable(stmt: &str) -> bool`; `rb_core::format::format_sql(sql: &str) -> String`.

- [ ] **Step 1: testes que falham**

Em `split.rs`, no módulo de testes:
```rust
    #[test]
    fn explainable_accepts_queries_and_dml_only() {
        for s in ["select 1", "/* c */ WITH a AS (select 1) select * from a", "insert into t values (1)", "update t set a = 1", "delete from t", "merge into t using s on true when matched then delete", "values (1)", "table t"] {
            assert!(explainable(s), "{s}");
        }
        for s in ["create table t (a int)", "drop table t", "alter table t add b int", "begin", "vacuum", "", "explain select 1"] {
            assert!(!explainable(s), "{s}");
        }
    }
```
`crates/rb-core/src/format.rs`:
```rust
//! Formatador de SQL: palavras-chave em MAIÚSCULAS, 2 espaços (estilo fixo).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercases_keywords_and_breaks_clauses() {
        let out = format_sql("select id, name from accounts a join orders o on o.account_id = a.id where a.active order by name");
        assert!(out.starts_with("SELECT\n  id,\n  name\nFROM\n  accounts a\n  JOIN orders o ON o.account_id = a.id\nWHERE"), "{out}");
    }

    #[test]
    fn keeps_literals_params_and_comments() {
        for (src, keep) in [
            ("select * from t where a = $1", "$1"),
            ("select * from t where a = :nome", ":nome"),
            ("select '-- não é comentário' as x", "'-- não é comentário'"),
            ("select \"Order Items\".id from \"Order Items\"", "\"Order Items\""),
            ("select 1 -- fim\n", "-- fim"),
            ("select /* meio */ 1", "/* meio */"),
        ] {
            assert!(format_sql(src).contains(keep), "{src} → {}", format_sql(src));
        }
    }

    #[test]
    fn dollar_quoted_bodies_stay_intact() {
        let body = "$$ begin raise notice 'oi'; end $$";
        let out = format_sql(&format!("create function f() returns void language plpgsql as {body};"));
        assert!(out.contains(body), "{out}");
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = format_sql("select a, b from t where x = 1; update t set a = 2");
        assert_eq!(format_sql(&once), once);
    }
}
```

- [ ] **Step 2: rodar e ver falhar**

Run: `cargo test -p rb-core --lib explainable format::`
Expected: erro de compilação (`explainable`/`format_sql` não existem).

- [ ] **Step 3: implementar**

`split.rs` (depois de `is_read_only_query`):
```rust
/// Statements que o EXPLAIN aceita: consultas e DML.
pub fn explainable(stmt: &str) -> bool {
    matches!(words(stmt).next().as_deref(), Some("select" | "with" | "insert" | "update" | "delete" | "merge" | "values" | "table"))
}
```
`Cargo.toml` do core: `sqlformat = "0.5"`. `lib.rs`: `pub mod format;`. `format.rs` acima dos testes:
```rust
use sqlformat::{FormatOptions, Indent, QueryParams};

/// Statements com `$$` vão sem formatar: o sqlformat quebra linhas dentro do corpo.
pub fn format_sql(sql: &str) -> String {
    let opts = FormatOptions { indent: Indent::Spaces(2), uppercase: Some(true), lines_between_queries: 2, ..FormatOptions::default() };
    let ranges = crate::split::split_statements(sql);
    if ranges.iter().any(|r| sql[r.clone()].contains("$$")) {
        // ponytail: script com corpo $$ — formata statement a statement e preserva os com $$.
        return ranges
            .iter()
            .map(|r| {
                let s = sql[r.clone()].trim();
                if s.contains("$$") { s.to_string() } else { sqlformat::format(s, &QueryParams::None, &opts) }
            })
            .collect::<Vec<_>>()
            .join(";\n\n")
            + if sql.trim_end().ends_with(';') { ";" } else { "" };
    }
    sqlformat::format(sql, &QueryParams::None, &opts)
}
```
(Se `split_statements` não incluir o `;` no range, o `join(";\n\n")` repõe os separadores; ajustar ao comportamento real visto no teste.)

- [ ] **Step 4: rodar e ver passar**

Run: `cargo test -p rb-core --lib explainable format::`
Expected: PASS (5 testes). Se `keeps_literals_params_and_comments` falhar para `:nome` ou comentário, registrar Ruling e adaptar o teste para documentar o limite.

- [ ] **Step 5: commit**

```bash
git add crates/rb-core Cargo.lock
git commit -m "feat(core): formatador de SQL (sqlformat) e explainable"
```

### Task 2: Histórico (`rb-core/src/history.rs`)

**Files:**
- Create: `crates/rb-core/src/history.rs`; Modify: `crates/rb-core/src/lib.rs`

**Interfaces:**
- Produces:
```rust
pub struct Entry { pub ts: i64, pub sql: String, pub ms: u64, pub rows: Option<u64>, pub error: Option<String> } // Serialize, Deserialize, Clone, Debug, PartialEq
pub const MAX_ENTRIES: usize = 5_000;
pub fn history_path(conn_id: &str) -> PathBuf
pub fn append(path: &Path, e: &Entry) -> Result<(), DbError>
pub fn load(path: &Path) -> Vec<Entry>          // mais novo primeiro
pub fn search<'a>(entries: &'a [Entry], text: &str) -> Vec<&'a Entry>
pub fn now_ms() -> i64
```

- [ ] **Step 1: testes que falham**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rb-history-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("c1.jsonl")
    }
    fn e(ts: i64, sql: &str) -> Entry {
        Entry { ts, sql: sql.into(), ms: 3, rows: Some(1), error: None }
    }

    #[test]
    fn round_trip_keeps_multiline_sql_newest_first() {
        let p = tmp("rt");
        let weird = "select 'a''b', \"Ç\" -- 🚀\nfrom t\n\twhere x = 1";
        append(&p, &e(1, "select 1")).unwrap();
        append(&p, &e(2, weird)).unwrap();
        let got = load(&p);
        assert_eq!(got.iter().map(|x| x.sql.as_str()).collect::<Vec<_>>(), [weird, "select 1"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn corrupt_lines_are_skipped_and_excess_is_trimmed() {
        let p = tmp("trim");
        append(&p, &e(1, "a")).unwrap();
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"{quebrado\n").unwrap();
        for i in 0..MAX_ENTRIES as i64 + 10 {
            append(&p, &e(i + 2, "b")).unwrap();
        }
        let got = load(&p);
        assert_eq!(got.len(), MAX_ENTRIES);
        assert_eq!(got[0].ts, MAX_ENTRIES as i64 + 11);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), MAX_ENTRIES, "arquivo reescrito");
    }

    #[test]
    fn search_ignores_case_and_collapses_repeats() {
        let all = vec![e(5, "select * from Accounts "), e(4, "select * from Accounts"), e(3, "select 2"), e(2, "select * from accounts")];
        let hits: Vec<i64> = search(&all, "ACCOUNTS").iter().map(|x| x.ts).collect();
        assert_eq!(hits, [5, 2]);
        assert_eq!(search(&all, "").len(), 3);
    }
}
```

- [ ] **Step 2: rodar e ver falhar** — `cargo test -p rb-core --lib history::` → erro de compilação.

- [ ] **Step 3: implementar**

```rust
//! Histórico de queries por conexão: JSON Lines em `app_dir()/history/<id>.jsonl`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DbError;

pub const MAX_ENTRIES: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix ms.
    pub ts: i64,
    pub sql: String,
    pub ms: u64,
    pub rows: Option<u64>,
    pub error: Option<String>,
}

pub fn history_path(conn_id: &str) -> PathBuf {
    crate::config::app_dir().join("history").join(format!("{conn_id}.jsonl"))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn io(e: std::io::Error) -> DbError {
    DbError::Io(e.to_string())
}

fn open_private(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true);
    if append { o.append(true); } else { o.truncate(true); }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

pub fn append(path: &Path, e: &Entry) -> Result<(), DbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let line = serde_json::to_string(e).map_err(|e| DbError::Io(e.to_string()))?;
    writeln!(open_private(path, true).map_err(io)?, "{line}").map_err(io)
}

/// Mais novo primeiro. Linhas inválidas são ignoradas; acima de `MAX_ENTRIES` o arquivo é reescrito.
pub fn load(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut all: Vec<Entry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).map_err(|e| tracing::warn!("{}: linha inválida: {e}", path.display())).ok())
        .collect();
    if all.len() > MAX_ENTRIES || all.len() != text.lines().count() {
        all.drain(..all.len().saturating_sub(MAX_ENTRIES));
        let tmp = path.with_extension("jsonl.tmp");
        let body: String = all.iter().filter_map(|e| serde_json::to_string(e).ok()).map(|l| l + "\n").collect();
        let res = open_private(&tmp, false).and_then(|mut f| f.write_all(body.as_bytes())).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = res {
            tracing::warn!("{}: {e}", path.display());
        }
    }
    all.reverse();
    all
}

/// "Contém" sem caixa; o mesmo SQL (após `trim`) aparece uma vez, na entrada mais nova.
pub fn search<'a>(entries: &'a [Entry], text: &str) -> Vec<&'a Entry> {
    let needle = text.trim().to_lowercase();
    let mut seen = std::collections::HashSet::new();
    entries.iter().filter(|e| e.sql.to_lowercase().contains(&needle)).filter(|e| seen.insert(e.sql.trim().to_string())).collect()
}
```
(Spec diz "repetições seguidas"; deduplicar todas é mais útil num popup de busca e mais simples. Registrar como Ruling ao executar.) `lib.rs`: `pub mod history;`. Confirmar que `serde` com `derive` está nas deps do core (`config.rs` já usa `Serialize`).

- [ ] **Step 4: rodar e ver passar** — `cargo test -p rb-core --lib history::` → 3 passed.

- [ ] **Step 5: commit** — `git commit -m "feat(core): histórico de queries por conexão (JSON Lines, 0600, 5.000 entradas)"`

### Task 3: Modelo do plano, parser Postgres e layout

**Files:**
- Create: `crates/rb-core/src/explain.rs`, `crates/rb-core/tests/fixtures/pg_analyze.json`, `crates/rb-core/tests/fixtures/pg_estimate.json`
- Modify: `crates/rb-core/src/lib.rs`

**Interfaces:**
- Produces:
```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanNode { pub op: String, pub object: Option<String>, pub detail: Vec<(String, String)>, pub est_rows: Option<f64>, pub act_rows: Option<f64>, pub cost: Option<f64>, pub time_ms: Option<f64>, pub pct: f64, pub warn: Vec<String>, pub children: Vec<PlanNode> }
#[derive(Debug, Clone, PartialEq)]
pub struct Plan { pub root: PlanNode, pub analyzed: bool, pub total_ms: Option<f64> }
pub fn parse_pg(json: &str, analyzed: bool) -> Result<Plan, DbError>
#[derive(Debug, Clone, PartialEq)]
pub struct Placed { pub path: Vec<usize>, pub x: f32, pub y: f32, pub parent: Option<usize> }
pub fn layout(root: &PlanNode, box_w: f32, box_h: f32, gap_x: f32, gap_y: f32) -> Vec<Placed>
pub fn node_at<'a>(root: &'a PlanNode, path: &[usize]) -> Option<&'a PlanNode>
pub fn hottest(root: &PlanNode) -> Vec<usize>   // caminho do nó de maior pct
pub fn edge_width(rows: Option<f64>) -> f32       // 1 + log10(rows+1), máx 8
pub fn short_count(n: f64) -> String               // 1.2M, 98k, 12
```

- [ ] **Step 1: gerar fixtures reais**

```bash
mkdir -p crates/rb-core/tests/fixtures
Q="select a.x, count(*) from generate_series(1,1000) a(x) join generate_series(1,100) b(y) on a.x = b.y where a.x > 10 group by a.x"
docker exec rb-pg psql -U postgres -At -c "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) $Q" > crates/rb-core/tests/fixtures/pg_analyze.json
docker exec rb-pg psql -U postgres -At -c "EXPLAIN (FORMAT JSON) $Q" > crates/rb-core/tests/fixtures/pg_estimate.json
```
Conferir os nós: Aggregate → Hash Join → (Function Scan a, Hash → Function Scan b).

- [ ] **Step 2: testes que falham** (em `explain.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const ANALYZE: &str = include_str!("../tests/fixtures/pg_analyze.json");
    const ESTIMATE: &str = include_str!("../tests/fixtures/pg_estimate.json");

    fn leaf(op: &str) -> PlanNode { PlanNode { op: op.into(), ..Default::default() } }

    #[test]
    fn pg_analyze_builds_tree_with_exclusive_times() {
        let p = parse_pg(ANALYZE, true).unwrap();
        assert!(p.analyzed && p.total_ms.is_some());
        assert_eq!(p.root.op, "Aggregate");
        assert_eq!(p.root.children[0].op, "Hash Join");
        assert!(p.root.children[0].detail.iter().any(|(k, v)| k == "Hash Cond" && v.contains("a.x = b.y")));
        let mut sum = 0.0;
        fn walk(n: &PlanNode, sum: &mut f64) { assert!(n.time_ms.unwrap() >= 0.0, "{}", n.op); *sum += n.pct; n.children.iter().for_each(|c| walk(c, sum)); }
        walk(&p.root, &mut sum);
        assert!((sum - 100.0).abs() < 0.5, "{sum}");
    }

    #[test]
    fn pg_estimate_uses_exclusive_cost() {
        let p = parse_pg(ESTIMATE, false).unwrap();
        assert!(!p.analyzed && p.total_ms.is_none());
        assert!(p.root.cost.unwrap() >= 0.0 && p.root.act_rows.is_none());
    }

    #[test]
    fn loops_multiply_time_and_never_go_negative() {
        let json = r#"[{"Plan":{"Node Type":"Nested Loop","Actual Total Time":10.0,"Actual Loops":1,"Actual Rows":5,"Plan Rows":5,"Total Cost":50,
            "Plans":[{"Node Type":"Index Scan","Relation Name":"t","Index Name":"t_pk","Actual Total Time":0.5,"Actual Loops":30,"Actual Rows":1,"Plan Rows":1,"Total Cost":1}]}}]"#;
        let p = parse_pg(json, true).unwrap();
        let scan = &p.root.children[0];
        assert_eq!(scan.time_ms, Some(15.0));
        assert_eq!(p.root.time_ms, Some(0.0), "pai menor que a soma dos filhos vira 0, não negativo");
        assert_eq!(scan.object.as_deref(), Some("t (t_pk)"));
    }

    #[test]
    fn pg_warnings_for_bad_estimates_and_wasteful_seq_scans() {
        let json = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"big","Actual Total Time":5,"Actual Loops":1,"Actual Rows":10,"Plan Rows":1000,"Rows Removed by Filter":5000,"Total Cost":9}}]"#;
        let w = parse_pg(json, true).unwrap().root.warn;
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn layout_centers_parents_and_never_overlaps() {
        let mut root = leaf("root");
        let mut mid = leaf("mid");
        mid.children = vec![leaf("a"), leaf("b")];
        root.children = vec![mid, leaf("c")];
        let placed = layout(&root, 100.0, 50.0, 20.0, 30.0);
        assert_eq!(placed.len(), 5);
        let at = |p: &[usize]| placed.iter().find(|x| x.path == p).unwrap();
        assert_eq!(at(&[0, 0]).x, 0.0);
        assert_eq!(at(&[0, 1]).x, 120.0);
        assert_eq!(at(&[0]).x, 60.0, "centralizado sobre a e b");
        assert_eq!(at(&[1]).x, 240.0);
        assert_eq!(at(&[]).x, 120.0, "centralizado entre mid e c");
        assert_eq!(at(&[0, 0]).y, 160.0);
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(a.y != b.y || (a.x - b.x).abs() >= 100.0, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn helpers_for_the_diagram() {
        assert_eq!(edge_width(None), 1.0);
        assert_eq!(edge_width(Some(9.0)), 2.0);
        assert_eq!(edge_width(Some(1e12)), 8.0);
        assert_eq!(short_count(1_234_567.0), "1.2M");
        assert_eq!(short_count(98_000.0), "98k");
        assert_eq!(short_count(12.0), "12");
        let mut r = leaf("r");
        let mut hot = leaf("hot");
        hot.pct = 80.0;
        r.children = vec![leaf("x"), hot];
        assert_eq!(hottest(&r), vec![1]);
        assert_eq!(node_at(&r, &[1]).unwrap().op, "hot");
    }
}
```

- [ ] **Step 3: rodar e ver falhar** — `cargo test -p rb-core --lib explain::` → erro de compilação.

- [ ] **Step 4: implementar**

```rust
//! Plano de execução: modelo comum, parsers (Postgres e Snowflake) e layout do diagrama.

use rust_i18n::t;
use serde_json::Value;

use crate::DbError;

// (structs da seção Interfaces, com os derives indicados)

fn num(v: &Value, k: &str) -> Option<f64> { v.get(k).and_then(Value::as_f64) }

fn bad_json(e: impl std::fmt::Display) -> DbError { DbError::Unsupported(t!("err.explain_parse", error = e).to_string()) }

const PG_DETAIL: &[&str] = &["Filter", "Index Cond", "Recheck Cond", "Hash Cond", "Merge Cond", "Join Filter", "Rows Removed by Filter", "Sort Key", "Group Key", "Join Type", "Strategy", "Workers Planned", "Workers Launched", "Shared Hit Blocks", "Shared Read Blocks", "Temp Read Blocks", "Temp Written Blocks", "Peak Memory Usage"];

fn pg_node(v: &Value, analyzed: bool) -> PlanNode {
    let children: Vec<PlanNode> = v.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(|c| pg_node(c, analyzed)).collect()).unwrap_or_default();
    let loops = num(v, "Actual Loops").unwrap_or(1.0);
    let incl_time = num(v, "Actual Total Time").map(|t| t * loops);
    let incl_cost = num(v, "Total Cost");
    // Inclusivos dos filhos guardados temporariamente em `time_ms`/`cost` deles: recalcula aqui.
    let child_time: f64 = v.get("Plans").and_then(Value::as_array).map_or(0.0, |a| a.iter().map(|c| num(c, "Actual Total Time").unwrap_or(0.0) * num(c, "Actual Loops").unwrap_or(1.0)).sum());
    let child_cost: f64 = v.get("Plans").and_then(Value::as_array).map_or(0.0, |a| a.iter().filter_map(|c| num(c, "Total Cost")).sum());
    let object = v.get("Relation Name").and_then(Value::as_str).map(|r| match v.get("Index Name").and_then(Value::as_str) {
        Some(i) => format!("{r} ({i})"),
        None => r.to_string(),
    }).or_else(|| v.get("Index Name").and_then(Value::as_str).map(str::to_string)).or_else(|| v.get("Function Name").and_then(Value::as_str).map(str::to_string));
    let detail = PG_DETAIL.iter().filter_map(|k| v.get(*k).filter(|x| !x.is_null()).map(|x| (k.to_string(), match x { Value::String(s) => s.clone(), Value::Array(a) => a.iter().map(|i| i.as_str().map_or(i.to_string(), str::to_string)).collect::<Vec<_>>().join(", "), o => o.to_string() }))).collect();
    let (est, act) = (num(v, "Plan Rows"), if analyzed { num(v, "Actual Rows").map(|r| r * loops) } else { None });
    let mut warn = Vec::new();
    if let (Some(e), Some(a)) = (est, act) && e.max(a) / e.min(a).max(1.0) > 10.0 {
        warn.push(t!("plan.warn_estimate", est = short_count(e), act = short_count(a)).to_string());
    }
    if v.get("Node Type").and_then(Value::as_str) == Some("Seq Scan") && let (Some(removed), Some(a)) = (num(v, "Rows Removed by Filter"), act.or(est)) && removed > 9.0 * a.max(1.0) {
        warn.push(t!("plan.warn_seq_filter", n = short_count(removed)).to_string());
    }
    PlanNode {
        op: v.get("Node Type").and_then(Value::as_str).unwrap_or("?").to_string(),
        object,
        detail,
        est_rows: est,
        act_rows: act,
        cost: if analyzed { None } else { incl_cost.map(|c| (c - child_cost).max(0.0)) },
        time_ms: if analyzed { incl_time.map(|t| (t - child_time).max(0.0)) } else { None },
        pct: 0.0,
        warn,
        children,
    }
}

/// Distribui `pct` pelo peso (tempo no real, custo no estimado) de cada nó.
fn fill_pct(root: &mut PlanNode, weight: fn(&PlanNode) -> f64) {
    fn total(n: &PlanNode, w: fn(&PlanNode) -> f64) -> f64 { w(n) + n.children.iter().map(|c| total(c, w)).sum::<f64>() }
    fn set(n: &mut PlanNode, w: fn(&PlanNode) -> f64, t: f64) { n.pct = if t > 0.0 { 100.0 * w(n) / t } else { 0.0 }; n.children.iter_mut().for_each(|c| set(c, w, t)); }
    let t = total(root, weight);
    set(root, weight, t);
}

pub fn parse_pg(json: &str, analyzed: bool) -> Result<Plan, DbError> {
    let v: Value = serde_json::from_str(json).map_err(bad_json)?;
    let top = v.get(0).ok_or_else(|| bad_json("[]"))?;
    let plan = top.get("Plan").ok_or_else(|| bad_json("Plan"))?;
    let mut root = pg_node(plan, analyzed);
    fill_pct(&mut root, if analyzed { |n: &PlanNode| n.time_ms.unwrap_or(0.0) } else { |n: &PlanNode| n.cost.unwrap_or(0.0) });
    let total_ms = analyzed.then(|| num(top, "Execution Time").or_else(|| num(plan, "Actual Total Time"))).flatten();
    Ok(Plan { root, analyzed, total_ms })
}

pub fn layout(root: &PlanNode, box_w: f32, box_h: f32, gap_x: f32, gap_y: f32) -> Vec<Placed> {
    fn width(n: &PlanNode, w: f32, g: f32) -> f32 {
        if n.children.is_empty() { w } else { n.children.iter().map(|c| width(c, w, g)).sum::<f32>() + g * (n.children.len() - 1) as f32 }
    }
    fn go(n: &PlanNode, path: Vec<usize>, left: f32, depth: usize, parent: Option<usize>, a: (f32, f32, f32, f32), out: &mut Vec<Placed>) {
        let (w, h, gx, gy) = a;
        let me = out.len();
        let span = width(n, w, gx);
        out.push(Placed { path: path.clone(), x: left + (span - w) / 2.0, y: depth as f32 * (h + gy), parent });
        let mut x = left;
        for (i, c) in n.children.iter().enumerate() {
            let mut p = path.clone();
            p.push(i);
            go(c, p, x, depth + 1, Some(me), a, out);
            x += width(c, w, gx) + gx;
        }
    }
    let mut out = Vec::new();
    go(root, Vec::new(), 0.0, 0, None, (box_w, box_h, gap_x, gap_y), &mut out);
    out
}

pub fn node_at<'a>(root: &'a PlanNode, path: &[usize]) -> Option<&'a PlanNode> {
    path.iter().try_fold(root, |n, &i| n.children.get(i))
}

pub fn hottest(root: &PlanNode) -> Vec<usize> {
    fn go(n: &PlanNode, path: &mut Vec<usize>, best: &mut (f64, Vec<usize>)) {
        if n.pct > best.0 { *best = (n.pct, path.clone()); }
        for (i, c) in n.children.iter().enumerate() { path.push(i); go(c, path, best); path.pop(); }
    }
    let mut best = (f64::MIN, Vec::new());
    go(root, &mut Vec::new(), &mut best);
    best.1
}

pub fn edge_width(rows: Option<f64>) -> f32 {
    rows.map_or(1.0, |r| (1.0 + (r + 1.0).log10()).min(8.0) as f32)
}

pub fn short_count(n: f64) -> String {
    match n {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.0}k", n / 1e3),
        n => format!("{n:.0}"),
    }
}
```
Chaves novas em `crates/rb-core/locales/app.toml` (en/pt/es): `err.explain_parse` ("plan in an unexpected format: %{error}" / "plano em formato inesperado: %{error}" / "plan con formato inesperado: %{error}"), `plan.warn_estimate` ("estimated %{est} rows, got %{act}" / "estimou %{est} linhas, vieram %{act}" / "estimó %{est} filas, llegaron %{act}"), `plan.warn_seq_filter` ("sequential scan discarded %{n} rows: an index may help" / "leitura sequencial descartou %{n} linhas: um índice pode ajudar" / "lectura secuencial descartó %{n} filas: un índice puede ayudar"). `lib.rs`: `pub mod explain;`.

- [ ] **Step 5: rodar e ver passar** — `cargo test -p rb-core --lib explain::` → 6 passed (ajustar `edge_width(Some(9.0))`: `1 + log10(10) = 2`).

- [ ] **Step 6: commit** — `git commit -m "feat(core): modelo de plano, parser do EXPLAIN do Postgres e layout do diagrama"`

### Task 4: Parsers do Snowflake

**Files:**
- Modify: `crates/rb-core/src/explain.rs`
- Create: `crates/rb-core/tests/fixtures/sf_explain.json`, `crates/rb-core/tests/fixtures/sf_stats.json`

**Interfaces:**
- Consumes: `PlanNode`, `Plan`, `fill_pct`, `short_count` (Task 3).
- Produces: `parse_sf_explain(json: &str) -> Result<Plan, DbError>`; `parse_sf_stats(names: &[String], rows: &[Vec<Option<String>>]) -> Result<Plan, DbError>` (recebe os nomes de coluna de `query_named`; a spec previa só `rows` — Ruling: as colunas são localizadas pelo nome, não pela posição).

- [ ] **Step 1: fixtures** (formato documentado do Snowflake)

`sf_explain.json`:
```json
{"GlobalStats":{"partitionsTotal":120,"partitionsAssigned":115,"bytesAssigned":52428800},
 "Operations":[[
  {"id":0,"operation":"Result","expressions":["SUM(O.AMOUNT)"]},
  {"id":1,"parentOperators":[0],"operation":"Aggregate","expressions":["aggExprs: [SUM(O.AMOUNT)]"]},
  {"id":2,"parentOperators":[1],"operation":"InnerJoin","expressions":["joinKey: (C.ID = O.CUSTOMER_ID)"]},
  {"id":3,"parentOperators":[2],"operation":"TableScan","objects":["DB.PUBLIC.CUSTOMERS"],"expressions":["ID"],"partitionsAssigned":5,"partitionsTotal":10,"bytesAssigned":2097152},
  {"id":4,"parentOperators":[2],"operation":"Filter","expressions":["O.STATUS = 'PAID'"]},
  {"id":5,"parentOperators":[4],"operation":"TableScan","objects":["DB.PUBLIC.ORDERS"],"expressions":["CUSTOMER_ID","AMOUNT","STATUS"],"partitionsAssigned":110,"partitionsTotal":110,"bytesAssigned":50331648}
 ]]}
```
`sf_stats.json` — linhas como `query_named` devolve (strings; variants em JSON):
```json
{"names":["QUERY_ID","STEP_ID","OPERATOR_ID","PARENT_OPERATORS","OPERATOR_TYPE","OPERATOR_STATISTICS","EXECUTION_TIME_BREAKDOWN","OPERATOR_ATTRIBUTES"],
 "rows":[
  ["q","1","0",null,"Result","{\"output_rows\":1}","{\"overall_percentage\":0.01}","{}"],
  ["q","1","1","[0]","Aggregate","{\"input_rows\":900000,\"output_rows\":1}","{\"overall_percentage\":0.09}","{}"],
  ["q","1","2","[1]","Join","{\"input_rows\":1000000,\"output_rows\":900000}","{\"overall_percentage\":0.2}","{\"equality_join_condition\":\"(C.ID = O.CUSTOMER_ID)\"}"],
  ["q","1","3","[2]","TableScan","{\"output_rows\":10000,\"pruning\":{\"partitions_scanned\":5,\"partitions_total\":10}}","{\"overall_percentage\":0.05}","{\"table_name\":\"DB.PUBLIC.CUSTOMERS\"}"],
  ["q","1","4","[2]","TableScan","{\"output_rows\":990000,\"spilling\":{\"bytes_spilled_remote_storage\":1024},\"pruning\":{\"partitions_scanned\":110,\"partitions_total\":110}}","{\"overall_percentage\":0.65}","{\"table_name\":\"DB.PUBLIC.ORDERS\"}"]
 ]}
```

- [ ] **Step 2: testes que falham**

```rust
    #[test]
    fn sf_explain_links_operations_by_parent() {
        let p = parse_sf_explain(include_str!("../tests/fixtures/sf_explain.json")).unwrap();
        assert!(!p.analyzed);
        assert_eq!(p.root.op, "Result");
        let join = &p.root.children[0].children[0];
        assert_eq!(join.op, "InnerJoin");
        assert_eq!(join.children.len(), 2);
        let orders = &join.children[1].children[0];
        assert_eq!(orders.object.as_deref(), Some("DB.PUBLIC.ORDERS"));
        assert!(orders.pct > 90.0, "bytes do scan / bytes totais");
        assert!(orders.warn.is_empty(), "110 partições: abaixo do limite de 100? não — 110 > 100 e 100% lidas → aviso");
    }

    #[test]
    fn sf_stats_use_time_share_and_warn_on_spill() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/sf_stats.json")).unwrap();
        let names: Vec<String> = v["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
        let rows: Vec<Vec<Option<String>>> = v["rows"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|c| c.as_str().map(str::to_string)).collect()).collect();
        let p = parse_sf_stats(&names, &rows).unwrap();
        assert!(p.analyzed);
        let orders = &p.root.children[0].children[0].children[1];
        assert_eq!(orders.object.as_deref(), Some("DB.PUBLIC.ORDERS"));
        assert!((orders.pct - 65.0).abs() < 0.01);
        assert_eq!(orders.act_rows, Some(990000.0));
        assert_eq!(orders.warn.len(), 2, "spill remoto e poda ruim: {:?}", orders.warn);
        assert!(p.root.children[0].children[0].detail.iter().any(|(k, _)| k == "equality_join_condition"));
    }
```
Corrigir o assert de `sf_explain_links_operations_by_parent` ao escrever: ORDERS tem 110 > 100 partições e 110/110 lidas → `orders.warn.len() == 1`.

- [ ] **Step 3: rodar e ver falhar** — `cargo test -p rb-core --lib explain::` → erro de compilação.

- [ ] **Step 4: implementar**

```rust
/// Monta a árvore a partir de (id, pais, nó); um nó com vários pais fica sob o primeiro.
fn tree_from_parents(mut items: Vec<(i64, Option<i64>, PlanNode)>) -> Result<PlanNode, DbError> {
    fn take(id: i64, items: &mut Vec<(i64, Option<i64>, PlanNode)>) -> Option<PlanNode> {
        let ix = items.iter().position(|(i, _, _)| *i == id)?;
        let (_, _, mut node) = items.remove(ix);
        let kids: Vec<i64> = items.iter().filter(|(_, p, _)| *p == Some(id)).map(|(i, _, _)| *i).collect();
        node.children = kids.into_iter().filter_map(|k| take(k, items)).collect();
        Some(node)
    }
    let root = items.iter().find(|(_, p, _)| p.is_none()).map(|(i, _, _)| *i).ok_or_else(|| bad_json("sem raiz"))?;
    take(root, &mut items).ok_or_else(|| bad_json("raiz"))
}

fn first_parent(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_i64)
}

fn bad_pruning(scanned: f64, total: f64) -> bool { total > 100.0 && scanned / total > 0.9 }

pub fn parse_sf_explain(json: &str) -> Result<Plan, DbError> {
    let v: Value = serde_json::from_str(json).map_err(bad_json)?;
    let ops: Vec<&Value> = v.get("Operations").and_then(Value::as_array).ok_or_else(|| bad_json("Operations"))?.iter().flat_map(|step| step.as_array().into_iter().flatten()).collect();
    let items = ops.iter().map(|o| {
        let mut detail: Vec<(String, String)> = o.get("expressions").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|e| ("expression".to_string(), e.to_string())).collect()).unwrap_or_default();
        let (pa, pt) = (num(o, "partitionsAssigned"), num(o, "partitionsTotal"));
        if let (Some(a), Some(t)) = (pa, pt) { detail.push(("partitions".into(), format!("{} / {}", short_count(a), short_count(t)))); }
        let bytes = num(o, "bytesAssigned");
        if let Some(b) = bytes { detail.push(("bytes".into(), short_count(b))); }
        let warn = match (pa, pt) { (Some(a), Some(t)) if bad_pruning(a, t) => vec![t!("plan.warn_pruning", a = short_count(a), t = short_count(t)).to_string()], _ => Vec::new() };
        let node = PlanNode { op: o.get("operation").and_then(Value::as_str).unwrap_or("?").into(), object: o.get("objects").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string), detail, cost: bytes, warn, ..Default::default() };
        (o.get("id").and_then(Value::as_i64).unwrap_or(-1), first_parent(o.get("parentOperators")), node)
    }).collect();
    let mut root = tree_from_parents(items)?;
    // Estimado no Snowflake: o peso são os bytes lidos nos scans (`cost` guarda bytes aqui).
    fill_pct(&mut root, |n| n.cost.unwrap_or(0.0));
    Ok(Plan { root, analyzed: false, total_ms: None })
}

pub fn parse_sf_stats(names: &[String], rows: &[Vec<Option<String>>]) -> Result<Plan, DbError> {
    let col = |n: &str| names.iter().position(|c| c.eq_ignore_ascii_case(n)).ok_or_else(|| bad_json(n));
    let (id, parents, op, stats, time, attrs) = (col("OPERATOR_ID")?, col("PARENT_OPERATORS")?, col("OPERATOR_TYPE")?, col("OPERATOR_STATISTICS")?, col("EXECUTION_TIME_BREAKDOWN")?, col("OPERATOR_ATTRIBUTES")?);
    let json = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
    let items = rows.iter().map(|r| {
        let (st, tm, at) = (json(r, stats), json(r, time), json(r, attrs));
        let mut warn = Vec::new();
        if st.pointer("/spilling/bytes_spilled_remote_storage").and_then(Value::as_f64).is_some_and(|b| b > 0.0) {
            warn.push(t!("plan.warn_spill").to_string());
        }
        if let (Some(s), Some(t)) = (st.pointer("/pruning/partitions_scanned").and_then(Value::as_f64), st.pointer("/pruning/partitions_total").and_then(Value::as_f64)) && bad_pruning(s, t) {
            warn.push(t!("plan.warn_pruning", a = short_count(s), t = short_count(t)).to_string());
        }
        let mut detail: Vec<(String, String)> = at.as_object().into_iter().flatten().filter(|(k, _)| k.as_str() != "table_name").map(|(k, v)| (k.clone(), v.as_str().map_or(v.to_string(), str::to_string))).collect();
        detail.extend(st.as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.to_string())));
        let node = PlanNode {
            op: r.get(op).cloned().flatten().unwrap_or_default(),
            object: at.get("table_name").and_then(Value::as_str).map(str::to_string),
            detail,
            act_rows: st.get("output_rows").and_then(Value::as_f64),
            pct: tm.get("overall_percentage").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
            warn,
            ..Default::default()
        };
        let pid = r.get(parents).cloned().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()).as_ref().and_then(|v| first_parent(Some(v)));
        (r.get(id).cloned().flatten().and_then(|s| s.parse().ok()).unwrap_or(-1), pid, node)
    }).collect();
    Ok(Plan { root: tree_from_parents(items)?, analyzed: true, total_ms: None })
}
```
Chaves novas no core (en/pt/es): `plan.warn_pruning` ("read %{a} of %{t} partitions: poor pruning" / "leu %{a} de %{t} partições: poda ruim" / "leyó %{a} de %{t} particiones: poda deficiente"), `plan.warn_spill` ("spilled to remote storage" / "transbordou para o armazenamento remoto" / "desbordó al almacenamiento remoto").

- [ ] **Step 5: rodar e ver passar** — `cargo test -p rb-core --lib explain::` → 8 passed.

- [ ] **Step 6: commit** — `git commit -m "feat(core): parsers do EXPLAIN e do GET_QUERY_OPERATOR_STATS do Snowflake"`

### Task 5: `Session::explain`

**Files:**
- Modify: `crates/rb-core/src/session.rs` (Cmd, Session, Actor), `crates/rb-core/src/snowflake.rs` (SfActor)
- Test: `crates/rb-core/tests/session.rs`

**Interfaces:**
- Consumes: `explain::{Plan, parse_pg, parse_sf_explain, parse_sf_stats}`, `split::{explainable, is_read_only_query}`.
- Produces: `Session::explain(&self, sql: String, analyze: bool) -> Result<Plan, DbError>`.

- [ ] **Step 1: teste de integração que falha** (`tests/session.rs`, no padrão dos outros: pula sem `RB_TEST_DATABASE_URL`)

```rust
#[test]
fn explain_analyze_of_dml_changes_nothing() {
    let Some((engine, cfg)) = pg() else { return };
    engine.block_on(async {
        let s = engine.connect(&cfg, Default::default()).await.unwrap();
        s.execute("drop table if exists rb_explain; create table rb_explain (id int primary key, v int); insert into rb_explain select g, 0 from generate_series(1, 50) g".into()).await.unwrap();
        let p = s.explain("update rb_explain set v = 1".into(), true).await.unwrap();
        assert!(p.analyzed && p.root.op.contains("Update"), "{:?}", p.root.op);
        let p = s.explain("select * from rb_explain where id = 3".into(), false).await.unwrap();
        assert!(!p.analyzed);
        let rows = s.query_text("select count(*)::text from rb_explain where v = 1", &[]).await.unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"), "rollback desfez o UPDATE");
        assert!(s.explain("drop table rb_explain".into(), false).await.is_err(), "DDL recusado");
        s.execute("drop table rb_explain".into()).await.unwrap();
    });
}
```
(Ajustar `pg()`/`block_on` aos helpers que `tests/session.rs` já usa; se o `execute` não aceitar vários statements, usar três chamadas.)

- [ ] **Step 2: rodar e ver falhar** — `RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test -p rb-core --test session explain` → erro de compilação (`explain` não existe).

- [ ] **Step 3: implementar**

`session.rs`:
```rust
// Cmd:
    Explain { sql: String, analyze: bool, reply: Reply<Plan> },
// Cmd::fail:
            Cmd::Explain { reply, .. } => reply.send(Err(e)).is_ok(),
// Session:
    /// Plano do statement. `analyze` executa (Postgres: dentro de transação desfeita no fim).
    pub async fn explain(&self, sql: String, analyze: bool) -> Result<Plan, DbError> {
        if !explainable(&sql) {
            return Err(DbError::Unsupported(t!("err.explain_only_dml").to_string()));
        }
        self.call(|reply| Cmd::Explain { sql, analyze, reply }).await
    }
// Actor::run:
                Cmd::Explain { sql, analyze, reply } => {
                    let _ = reply.send(self.explain(&sql, analyze).await);
                    rx.recv().await
                }
// Actor:
    async fn explain(&mut self, sql: &str, analyze: bool) -> Result<Plan, DbError> {
        let opts = if analyze { "ANALYZE, BUFFERS, FORMAT JSON" } else { "FORMAT JSON" };
        let tx = self.client.transaction().await?;
        let res = tx.simple_query(&format!("EXPLAIN ({opts}) {sql}")).await;
        // Sempre desfaz: o ANALYZE executou o statement de verdade.
        if let Err(e) = tx.rollback().await {
            tracing::warn!("rollback do EXPLAIN falhou, descartando a conexão: {e}");
            self.client = self.connector.client().await?;
            *self.cancel.lock().expect("mutex do cancel token") = self.client.cancel_token();
            return Err(e.into());
        }
        let json: String = res?.iter().filter_map(|m| match m { tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string), _ => None }).collect::<Vec<_>>().join("\n");
        parse_pg(&json, analyze)
    }
```
`snowflake.rs` (`SfActor::run` e método novo):
```rust
                Cmd::Explain { sql, analyze, reply } => {
                    let _ = reply.send(self.explain(&sql, analyze).await);
                    rx.recv().await
                }
    /// Estimado: `EXPLAIN USING JSON`. Real: executa (só leituras; a sessão é compartilhada
    /// entre abas e um BEGIN/ROLLBACK atingiria as outras) e lê GET_QUERY_OPERATOR_STATS.
    async fn explain(&self, sql: &str, analyze: bool) -> Result<Plan, DbError> {
        if !analyze {
            let (_, rows) = self.query_named(&format!("EXPLAIN USING JSON {sql}")).await?;
            let json = rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default();
            return parse_sf_explain(&json);
        }
        if !is_read_only_query(sql) {
            return Err(DbError::Unsupported(t!("err.explain_sf_dml").to_string()));
        }
        let started = Instant::now();
        let cursor = self.submit(sql).await?;
        let id = cursor.query_id().replace('\'', "");
        drop(cursor);
        let total = started.elapsed().as_secs_f64() * 1000.0;
        let (names, rows) = self.query_named(&format!("SELECT * FROM TABLE(GET_QUERY_OPERATOR_STATS('{id}'))")).await?;
        let mut plan = parse_sf_stats(&names, &rows)?;
        plan.total_ms = Some(total);
        Ok(plan)
    }
```
Chaves novas no core: `err.explain_only_dml` ("EXPLAIN only works on queries and DML" / "EXPLAIN só para consultas e DML" / "EXPLAIN solo para consultas y DML"), `err.explain_sf_dml` ("on Snowflake the real plan only runs queries; use ⌘E (estimated) for DML" / "no Snowflake o plano real só executa consultas; use ⌘E (estimado) para DML" / "en Snowflake el plan real solo ejecuta consultas; usa ⌘E (estimado) para DML").

- [ ] **Step 4: rodar e ver passar** — mesmo comando → PASS. Depois a suíte inteira: `RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace` → 0 failed.

- [ ] **Step 5: commit** — `git commit -m "feat(core): Session::explain (Postgres com rollback; Snowflake estimado e real só para consultas)"`

### Task 6: Abas de resultado e mensagens no `SqlTab`, gravando o histórico

**Files:**
- Modify: `crates/rb-app/src/sql_tab.rs`, `crates/rb-app/locales/app.toml`

**Interfaces:**
- Consumes: `rb_core::history::{Entry, append, history_path, now_ms}`, `grid::{ResultGrid, busy_later, set_idle}`.
- Produces (usados pelas Tasks 7–9):
```rust
pub enum Pane { Result(usize), Messages, Plan }
pub struct Message { pub sql_head: String, pub rows: Option<u64>, pub affected: Option<u64>, pub ms: u64, pub error: Option<String> }
fn panes(has_columns: &[bool], statements: usize) -> (Vec<usize>, bool)
impl SqlTab { pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) }
```

- [ ] **Step 1: teste que falha** (em `sql_tab.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn statements_become_result_tabs_and_messages() {
        assert_eq!(panes(&[true], 1), (vec![0], false), "um statement: só a grade");
        assert_eq!(panes(&[true, false, true], 3), (vec![0, 2], true));
        assert_eq!(panes(&[false, false], 2), (vec![], true), "só DML: só mensagens");
        assert_eq!(panes(&[true], 3), (vec![0], true), "erro no 2º de 3: o 1º fica, mensagens aparecem");
    }

    #[::core::prelude::v1::test]
    fn sql_head_is_first_line_trimmed() {
        assert_eq!(sql_head("\n  select *\n from t"), "select *");
        assert_eq!(sql_head(&"x".repeat(200)).chars().count(), 81);
    }
}
```

- [ ] **Step 2: rodar e ver falhar** — `cargo test -p rb-app statements_become sql_head` → erro de compilação.

- [ ] **Step 3: implementar**

Funções puras:
```rust
/// Índices dos statements que viram aba de resultado (devolveram colunas) e se "Mensagens"
/// aparece (script com mais de um statement).
fn panes(has_columns: &[bool], statements: usize) -> (Vec<usize>, bool) {
    (has_columns.iter().enumerate().filter(|(_, c)| **c).map(|(i, _)| i).collect(), statements > 1)
}

fn sql_head(sql: &str) -> String {
    let line = sql.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 { line.chars().take(80).collect::<String>() + "…" } else { line.to_string() }
}
```
Estado do `SqlTab`: troca `table: Entity<TableState<ResultGrid>>` por
```rust
    results: Vec<(SharedString, Entity<TableState<ResultGrid>>)>,
    messages: Vec<Message>,
    show_messages: bool,
    plan: Option<Entity<crate::plan_view::PlanView>>,   // Task 9; até lá, omitir
    active: Pane,
```
Em `new`, `results = vec![("".into(), nova grade)]`, `active = Pane::Result(0)`. Um helper `fn new_grid(window, cx) -> Entity<TableState<ResultGrid>>` cria a grade e a assina com `cx.observe` (guardar a `Subscription` em `_subs`). `fn active_grid(&self) -> Option<&Entity<…>>` devolve a grade da aba ativa (para ⌘C, skeleton, contador de linhas).

`run` passa a coletar um `Vec<(Range<usize>, Result<ExecOutcome, DbError>)>` e chama `finish(outcomes, src, total_statements, session, window, cx)`:
- para cada outcome: `Message { sql_head, rows: (cols>0).then(batch.rows()), affected, ms, error }` e grava no histórico em segundo plano (`cx.background_spawn(async move { history::append(&path, &entry) })`, `tracing::warn` no erro);
- `panes(...)` decide as abas; cada statement com colunas ganha grade nova com `set_result`; título `t!("results.result", n = k+1)`; para todas menos a última, `rs.more = false` (o portal já fechou) e o título ganha `t!("results.first_rows_only")` no rodapé da aba quando havia `more`;
- `active` = última aba de resultado, ou `Messages` se houve erro e `show_messages`;
- o erro mantém o comportamento atual (`finish_error`: diagnóstico no editor + notificação).
- O skeleton (`busy_later`) e `set_idle` passam a usar a grade 0 recém-criada no início do run (o run limpa `results` para uma grade vazia).

Render: barra de abas só quando `results.len() > 1 || show_messages || plan.is_some()`, usando `TabBar`/`Tab` do gpui-kit (mesmo usado no `workspace.rs`); conteúdo conforme `active`: grade (`DataTable`), lista de mensagens (`v_flex` com uma linha por `Message`: nº, `sql_head`, linhas/afetadas, ms, erro em `theme.danger`), ou o plano. Rodapé: contagem de linhas da grade ativa (como hoje).

`insert_text`:
```rust
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| {
            e.insert(text.to_string(), window, cx);
            e.focus(window, cx);
        });
    }
```
Chaves em `crates/rb-app/locales/app.toml` (en/pt/es): `results.result` ("Result %{n}" / "Resultado %{n}" / "Resultado %{n}"), `results.messages` ("Messages"/"Mensagens"/"Mensajes"), `results.first_rows_only` ("first rows only" / "só as primeiras" / "solo las primeras"), `results.affected` ("%{n} affected" / "%{n} afetada(s)" / "%{n} afectada(s)").

- [ ] **Step 4: rodar e ver passar** — `cargo test -p rb-app` → PASS (inclui o teste de i18n). `cargo build -p rb-app` sem warnings novos.

- [ ] **Step 5: commit** — `git commit -m "feat(app): uma aba de resultado por statement, aba Mensagens e histórico gravado a cada execução"`

### Task 7: Formatar (⌘⇧F) e ⌘⌥F para localizar/substituir

**Files:**
- Modify: `crates/rb-app/src/main.rs` (ação `FormatSql`, atalhos), `crates/rb-app/src/sql_tab.rs`

**Interfaces:**
- Consumes: `rb_core::format::format_sql`.

- [ ] **Step 1: teste que falha** — a lógica de "o que formatar" é pura:

```rust
    #[::core::prelude::v1::test]
    fn format_target_is_selection_or_everything() {
        assert_eq!(format_target("select 1; select 2", 0..0), 0..18);
        assert_eq!(format_target("select 1; select 2", 10..18), 10..18);
    }
```
- [ ] **Step 2: rodar e ver falhar** — `cargo test -p rb-app format_target` → erro de compilação.
- [ ] **Step 3: implementar**

```rust
fn format_target(src: &str, selection: Range<usize>) -> Range<usize> {
    if selection.is_empty() { 0..src.len() } else { selection }
}

    fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (src, sel) = { let e = self.editor.read(cx); (e.value().to_string(), e.selected_range()) };
        let range = format_target(&src, sel);
        let formatted = rb_core::format::format_sql(&src[range.clone()]);
        if formatted == src[range.clone()] { return; }
        self.editor.update(cx, |e, cx| {
            if range.len() == src.len() { e.select_all(window, cx); }
            e.replace(formatted, window, cx);
        });
    }
```
(Confirmar os nomes `selected_range`/`select_all` no `InputBaseState` com `grep -n "pub fn selected_range\|pub fn select_all" ~/.cargo/registry/src/*/gpui-base-0.7.3/src/input/base/state.rs`; se faltarem, usar `set_selection(0, len)` — registrar Ruling.)
`main.rs`: `FormatSql` no `actions!`; atalhos `KeyBinding::new("cmd-shift-f", FormatSql, Some("SqlTab > Input"))` e `KeyBinding::new("cmd-alt-f", gpui_kit::component::input::Replace, Some("SqlTab > Input"))`. `SqlTab::render`: `.on_action(cx.listener(|t, _: &FormatSql, w, cx| t.format(w, cx)))`.
- [ ] **Step 4: rodar e ver passar** — `cargo test -p rb-app` → PASS.
- [ ] **Step 5: commit** — `git commit -m "feat(app): ⌘⇧F formata o SQL (seleção ou script); localizar e substituir vai para ⌘⌥F"`

### Task 8: Popup do histórico (⌘Y)

**Files:**
- Create: `crates/rb-app/src/history_popup.rs`
- Modify: `crates/rb-app/src/main.rs` (ação `ShowHistory`, `cmd-y` em `Workspace`; `HistoryPrev/HistoryNext` ou reaproveitar `QuickPrev/QuickNext` no contexto `HistoryPopup > Input`), `crates/rb-app/src/workspace.rs`, `crates/rb-app/locales/app.toml`

**Interfaces:**
- Consumes: `history::{load, search, history_path, Entry}`, `SqlTab::insert_text`, `Workspace::open_sql_file`.
- Produces: `history_popup::open(conn_name: String, path: PathBuf, on_pick: impl Fn(String, bool /* nova aba */, &mut Window, &mut App) + 'static, window, cx)`.

- [ ] **Step 1: teste que falha** — rótulo de data é a parte pura:

```rust
    #[::core::prelude::v1::test]
    fn when_label_says_today_yesterday_or_date() {
        let now = 1_760_560_000_000; // ms
        let day = 86_400_000;
        assert!(when_label(now - 60_000, now).starts_with(&t!("history.today", time = "").to_string().trim_end().to_string()));
        assert!(when_label(now - day, now).contains(&t!("history.yesterday", time = "").to_string().trim().to_string()));
        assert!(when_label(now - 10 * day, now).contains('-'), "data ISO");
    }
```
(Calcular dia/hora em horário local via `libc::localtime_r` seria nova dependência; usar UTC com `ts / day` — Ruling: datas do histórico em UTC; custo: perto da meia-noite "hoje/ontem" pode errar por algumas horas. Ajustar o teste para UTC.)

- [ ] **Step 2: rodar e ver falhar** — `cargo test -p rb-app when_label` → erro de compilação.
- [ ] **Step 3: implementar** — `history_popup.rs` no molde de `quick_open.rs`: `HistoryPopup { input, all: Vec<Entry>, hits: Vec<usize>, selected, scroll, on_pick, _subs }`; `new` carrega `history::load(&path)` em `cx.background_spawn` e preenche; `InputEvent::Change` refaz `search` (limite 200); `PressEnter { secondary }` → `pick(selected, secondary)`; linhas com `sql_head` em destaque e embaixo `when_label · {ms} ms · {rows}` ou `t!("history.error")` em `theme.danger`; `when_label(ts, now)`: `hh:mm` UTC de `ts`, com `t!("history.today", time)` / `t!("history.yesterday", time)` / `YYYY-MM-DD hh:mm`. `Workspace::show_history`: sem `current_open()` → notificação `ws.open_connection_to_search`; senão abre com `history_path(&id)` e `on_pick(sql, new_tab)`: se `!new_tab` e a aba ativa é `TabView::Sql` desta conexão → `insert_text`; senão `new_sql_tab` e `insert_text` na aba criada.
Chaves (en/pt/es): `history.title` ("History — %{conn}" / "Histórico — %{conn}" / "Historial — %{conn}"), `history.placeholder` ("search queries…"/"buscar queries…"/"buscar consultas…"), `history.empty` ("nothing yet"/"nada ainda"/"nada todavía"), `history.error` ("error"/"erro"/"error"), `history.today` ("today %{time}"/"hoje %{time}"/"hoy %{time}"), `history.yesterday` ("yesterday %{time}"/"ontem %{time}"/"ayer %{time}"), `history.hint` ("Enter inserts · ⌘Enter new tab" / "Enter insere · ⌘Enter aba nova" / "Enter inserta · ⌘Enter pestaña nueva").
- [ ] **Step 4: rodar e ver passar** — `cargo test -p rb-app` → PASS.
- [ ] **Step 5: commit** — `git commit -m "feat(app): ⌘Y abre o histórico da conexão com busca"`

### Task 9: Diagrama do plano e ⌘E/⌘⇧E

**Files:**
- Create: `crates/rb-app/src/plan_view.rs`
- Modify: `crates/rb-app/src/main.rs` (ações `ExplainEstimate`, `ExplainAnalyze`, `PlanZoomIn`, `PlanZoomOut`, `PlanFit`; atalhos), `crates/rb-app/src/sql_tab.rs`, `crates/rb-app/locales/app.toml`

**Interfaces:**
- Consumes: `Session::explain`, `explain::{Plan, layout, node_at, hottest, edge_width, short_count}`, `Pane::Plan`.
- Produces: `PlanView::new(plan: Plan, cx) -> PlanView`.

- [ ] **Step 1: teste que falha** — cor e zoom são puros:

```rust
    #[::core::prelude::v1::test]
    fn heat_and_zoom_are_clamped() {
        assert_eq!(heat(0.0), 0.0);
        assert_eq!(heat(25.0), 0.5);
        assert_eq!(heat(90.0), 1.0);
        assert_eq!(step_zoom(2.0, true), 2.0);
        assert_eq!(step_zoom(0.25, false), 0.25);
        assert_eq!(fit_zoom((1000.0, 400.0), (500.0, 400.0)), 0.5);
        assert_eq!(fit_zoom((100.0, 100.0), (500.0, 400.0)), 1.0, "não amplia além de 1");
    }
```
- [ ] **Step 2: rodar e ver falhar** — `cargo test -p rb-app heat_and_zoom` → erro de compilação.
- [ ] **Step 3: implementar**

```rust
const BOX_W: f32 = 220.0;
const BOX_H: f32 = 64.0;
const GAP_X: f32 = 32.0;
const GAP_Y: f32 = 56.0;

/// 0 = neutro, 1 = vermelho forte; satura em 50%.
fn heat(pct: f64) -> f32 { (pct / 50.0).clamp(0.0, 1.0) as f32 }
fn step_zoom(z: f32, up: bool) -> f32 { (if up { z * 1.25 } else { z / 1.25 }).clamp(0.25, 2.0) }
fn fit_zoom(content: (f32, f32), view: (f32, f32)) -> f32 { (view.0 / content.0).min(view.1 / content.1).clamp(0.25, 1.0) }

pub struct PlanView { plan: Plan, placed: Vec<Placed>, selected: Vec<usize>, zoom: f32, fitted: bool, focus: FocusHandle, scroll: ScrollHandle }
```
- `new`: `placed = layout(&plan.root, BOX_W, BOX_H, GAP_X, GAP_Y)`, `selected = hottest(&plan.root)`, `zoom = 1.0`, `fitted = false`.
- Render: `h_flex` com (a) área `div().id("plan-area").flex_1().overflow_scroll().track_scroll(&scroll)` contendo um `div().relative().w(px(W*zoom)).h(px(H*zoom))` com: um `canvas` (`size_full`, `absolute`) que, no `paint`, para cada `Placed` com `parent`, monta `PathBuilder::stroke(px(edge_width(node.act_rows.or(node.est_rows)) * zoom))` do topo-centro do filho ao fundo-centro do pai (`move_to` + `line_to`) e chama `window.paint_path(path, theme.muted_foreground)`; e as caixas: `div().absolute().left(px(x*zoom)).top(px(y*zoom)).w(px(BOX_W*zoom)).h(px(BOX_H*zoom))` com fundo `theme.background.blend(theme.danger.opacity(heat(pct) * 0.6))`, borda `theme.border` (ou `theme.primary` se selecionada), texto: linha 1 `op` + `"{pct:.0}%"`, linha 2 `object`, linha 3 tempo (`{t:.1} ms`) ou custo/bytes + `short_count(rows)` + "⚠" se `warn` não vazio; `on_click` seleciona `path`. Rótulo de linhas de cada seta: `div` absoluta no meio da seta com `short_count(rows)` em `text_xs`. (b) painel `w(px(280.))` com `op`, `object`, números e `detail` (chave em cinza, valor em mono) e `warn` em `theme.warning`.
- Cabeçalho acima: `t!("plan.estimated")` ou `t!("plan.real", time = …)`.
- Zoom: ações `PlanZoomIn/PlanZoomOut/PlanFit` com `key_context("PlanView")` e `track_focus`; `PlanFit` e o primeiro render (via `fitted`, lendo o tamanho da área em `on_children_prepainted`/`canvas` prepaint) usam `fit_zoom`.
- `SqlTab::explain(analyze, window, cx)`: statement em `statement_at`; sem sessão → conecta como no `run`; `session.explain(sql, analyze)`; ok → `self.plan = Some(cx.new(|cx| PlanView::new(plan, cx)))`, `active = Pane::Plan`; erro → notificação. Spinner da barra enquanto roda (`running = true`), ⌘. cancela (`session.cancel()`).
- `main.rs`: `KeyBinding::new("cmd-e", ExplainEstimate, Some("SqlTab > Input"))`, `KeyBinding::new("cmd-shift-e", ExplainAnalyze, Some("SqlTab > Input"))`, `KeyBinding::new("cmd-=", PlanZoomIn, Some("PlanView"))`, `KeyBinding::new("cmd-+", PlanZoomIn, Some("PlanView"))`, `KeyBinding::new("cmd--", PlanZoomOut, Some("PlanView"))`, `KeyBinding::new("cmd-0", PlanFit, Some("PlanView"))`.
Chaves (en/pt/es): `plan.tab` ("Plan"/"Plano"/"Plan"), `plan.estimated` ("Estimated plan"/"Plano estimado"/"Plan estimado"), `plan.real` ("Actual plan · %{time}"/"Plano real · %{time}"/"Plan real · %{time}"), `plan.rows` ("rows"/"linhas"/"filas"), `plan.details` ("Details"/"Detalhes"/"Detalles").
- [ ] **Step 4: rodar e ver passar** — `cargo test -p rb-app` → PASS; `cargo build -p rb-app` limpo.
- [ ] **Step 5: verificação visual** (só com o Mac ocioso, instância isolada com HOME temporário — conduta da sessão): ⌘E e ⌘⇧E na conexão "teste-visual" com a query do fixture; diagrama, setas, zoom.
- [ ] **Step 6: commit** — `git commit -m "feat(app): plano de execução em diagrama (⌘E estimado, ⌘⇧E real) com zoom e detalhes"`

### Task 10: Fechamento

- [ ] Suíte completa: `RB_TEST_SSH=127.0.0.1:52222 RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres cargo test --workspace` → 0 failed (conferir `exit=0`, não só o resumo).
- [ ] Revisão final do branch (executing-plans: reviewer no modelo mais capaz) e fix pass.
- [ ] Ledger com Rulings e minors.
