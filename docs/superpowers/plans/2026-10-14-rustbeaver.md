# rustbeaver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cliente PostgreSQL desktop para macOS em Rust (editor SQL, navegador de schema, grade editável) que substitui o DBeaver no uso diário do autor.

**Architecture:** Workspace Cargo com `rb-core` (Rust puro: conexão, execução paginada por portal, decodificação binária preguiçosa, metadados, autocomplete, SQL de edição) e `rb-app` (GPUI via `gpui-kit`). O `rb-core` roda I/O num runtime tokio próprio e expõe futures que só dependem de canais `tokio::sync`, aguardados direto no executor do GPUI.

**Tech Stack:** Rust 1.96 (edition 2024), tokio 1.53, tokio-postgres 0.7.18, tokio-postgres-rustls 0.14 + rustls 0.23, security-framework 3, gpui-kit 0.7.3 (gpui-pre 0.3.9 + gpui-component 0.7.3), lsp-types 0.97.

**Spec:** `docs/superpowers/specs/2026-10-14-rustbeaver-design.md`
**Notas da API de UI:** `docs/superpowers/plans/gpui-api-notes.md` (ler antes das Tarefas 10–16)

## Global Constraints

- Só macOS. Só PostgreSQL. Só autocommit (transações internas curtas apenas para portal de leitura e gravação da grade).
- Versões fixadas: `gpui-kit = "=0.7.3"` (feature `tree-sitter-sql`), `tokio-postgres = "0.7.18"`, `tokio-postgres-rustls = "0.14"`, `rustls = "0.23"`, `security-framework = "3"`.
- `rb-core` não depende de GPUI. `rb-core` nunca entra em pânico em caminho de dados: erros viram `DbError`.
- A thread de UI nunca faz I/O de rede, Keychain ou disco pesado.
- Lote de busca: 1.000 linhas (`FETCH_ROWS`). Grade pede mais quando faltam < 200 linhas.
- Senhas só no Keychain (serviço `dev.rustbeaver`, conta = id da conexão). Nunca no TOML.
- Arquivos: `~/Library/Application Support/rustbeaver/connections.toml` e `.../scripts/*.sql`.
- Identificadores sempre passam por `quote_ident` ao gerar SQL. Valores de edição sempre como parâmetro texto com `($n::text)::<format_type>`.
- Comentários de código e mensagens ao usuário em português.
- Metas de performance da spec §8 (janela < 300 ms, memória parada < 150 MB, primeiro lote < 200 ms, autocomplete < 16 ms, 1M×10 < 400 MB).

## Review Focus

1. **Células enormes (texto/jsonb de vários MB):** a grade deve exibir só o começo, numa linha, sem travar a rolagem; a edição usa o valor completo. → teste `display_truncates_huge_text_without_full_decode` (Tarefa 3).
2. **Células com quebra de linha, tab ou aspas:** exibição em uma linha; cópia TSV com aspas escapadas para colar em planilha. → testes `display_is_single_line` e `tsv_quotes_special_fields` (Tarefa 3).
3. **Identificadores com maiúsculas, espaços e aspas** (`"Order Items"`, schema `"Sales"`): abrir, ordenar, editar e autocompletar sempre com aspas corretas. → testes `select_sql_quotes_identifiers`, `update_quotes_mixed_case_identifiers` (Tarefa 9), `mixed_case_identifiers_are_quoted_on_insert` (Tarefa 8), integração com `"Order Items"` (Tarefas 7 e 9).
4. **Abandonar um resultado parcial e rodar outra query:** a transação do portal anterior precisa fechar antes do novo statement, e não pode ficar "idle in transaction". → teste `abandoning_a_partial_result_closes_its_transaction` (Tarefa 6).
5. **Resultado com número de linhas múltiplo exato de 1.000:** a última busca volta vazia, `more = false`, sem travar a grade. → teste `exact_multiple_of_page_size_terminates` (Tarefa 5).

---

## Estrutura de arquivos

```
Cargo.toml                         workspace
.gitignore
crates/rb-core/
  Cargo.toml
  src/lib.rs                       declara módulos, reexporta DbError
  src/error.rs                     DbError, error_byte_offset
  src/lexer.rs                     tokenize, ident_text, quote_ident
  src/split.rs                     split_statements, statement_at
  src/decode.rs                    cell_text, display_text, one_line
  src/batch.rs                     ColumnInfo, Batch, ResultSet (+ TSV)
  src/config.rs                    ConnectionConfig, SslMode, ConfigFile, load/save, paths
  src/secrets.rs                   get/set/delete_password (Keychain)
  src/session.rs                   Engine, Session, ExecOutcome, FetchOutcome, SqlWithParams
  src/metadata.rs                  SchemaObjects, ColumnMeta, IndexMeta, MetaCache, MetaRequest, consultas
  src/complete.rs                  complete(), Candidate, Completion
  src/table.rs                     select_sql, EditTarget, RowEdit, build_sql, PendingEdits
  tests/common/mod.rs              helpers de integração (RB_TEST_DATABASE_URL)
  tests/session.rs                 execução, paginação, erros, transação
  tests/session_lifecycle.rs       cancelamento, reconexão, abandono
  tests/metadata.rs                catálogo
  tests/table_edit.rs              edição ida e volta
crates/rb-app/
  Cargo.toml
  src/main.rs                      bootstrap, ações, atalhos, global Db
  src/workspace.rs                 janela: barra lateral, abas
  src/conn_dialog.rs               diálogo de nova conexão
  src/meta.rs                      MetaStore (entidade com sessão de metadados + cache), ConnTarget
  src/schema_tree.rs               árvore de schema preguiçosa
  src/grid.rs                      ResultGrid (TableDelegate), cópia
  src/sql_tab.rs                   aba SQL: editor, execução, autosave
  src/completion.rs                CompletionProvider sobre rb_core::complete
  src/table_tab.rs                 aba de tabela: filtro, ordenação, edição
docs/manual-checklist.md           checklist manual da UI
docs/perf.md                       como medir as metas
scripts/seed-perf.sql              dados de performance
```

---

### Task 1: Workspace, erros e lexer SQL

**Files:**
- Create: `Cargo.toml`, `crates/rb-core/Cargo.toml`, `crates/rb-core/src/lib.rs`, `crates/rb-core/src/error.rs`, `crates/rb-core/src/lexer.rs`
- Modify: `.gitignore` (já existe com `rea/`, `target/`, `.DS_Store`)

**Interfaces:**
- Produces:
  - `rb_core::DbError` (enum: `Postgres { code: String, message: String, detail: Option<String>, hint: Option<String>, position: Option<u32> }`, `Connection(String)`, `Config(String)`, `Keychain(String)`, `Io(String)`, `Edit(String)`), `impl Display`, `impl From<tokio_postgres::Error>`
  - `rb_core::error::error_byte_offset(stmt: &str, position: u32) -> usize`
  - `rb_core::lexer::{TokKind, Token { kind, start, end }, tokenize(&str) -> Vec<Token>, ident_text(&str) -> String, quote_ident(&str) -> String}`

- [ ] **Step 1: Criar o workspace e o crate**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[profile.release]
lto = "thin"
codegen-units = 1
debug = "line-tables-only"
```

`crates/rb-core/Cargo.toml`:
```toml
[package]
name = "rb-core"
version = "0.1.0"
edition = "2024"

[dependencies]
rustls = "0.23"
rustls-native-certs = "0.8"
security-framework = "3"
serde = { version = "1", features = ["derive"] }
tokio = { version = "1.53", features = ["rt-multi-thread", "sync", "time", "macros"] }
tokio-postgres = "0.7.18"
tokio-postgres-rustls = "0.14"
toml = "1"
tracing = "0.1.44"

[dev-dependencies]
pollster = "1"
```

`crates/rb-core/src/lib.rs`:
```rust
pub mod error;
pub mod lexer;

pub use error::DbError;
```

- [ ] **Step 2: Escrever `error.rs`**

```rust
use std::fmt;

use tokio_postgres::error::ErrorPosition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    Postgres {
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        /// Posição 1-based em caracteres dentro do statement enviado.
        position: Option<u32>,
    },
    Connection(String),
    Config(String),
    Keychain(String),
    Io(String),
    Edit(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Postgres { code, message, detail, hint, .. } => {
                write!(f, "[{code}] {message}")?;
                if let Some(d) = detail {
                    write!(f, "\nDETAIL: {d}")?;
                }
                if let Some(h) = hint {
                    write!(f, "\nHINT: {h}")?;
                }
                Ok(())
            }
            DbError::Connection(m) => write!(f, "conexão: {m}"),
            DbError::Config(m) => write!(f, "configuração: {m}"),
            DbError::Keychain(m) => write!(f, "Keychain: {m}"),
            DbError::Io(m) => write!(f, "E/S: {m}"),
            DbError::Edit(m) => write!(f, "edição: {m}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<tokio_postgres::Error> for DbError {
    fn from(e: tokio_postgres::Error) -> Self {
        match e.as_db_error() {
            Some(db) => DbError::Postgres {
                code: db.code().code().to_string(),
                message: db.message().to_string(),
                detail: db.detail().map(Into::into),
                hint: db.hint().map(Into::into),
                position: match db.position() {
                    Some(ErrorPosition::Original(p)) => Some(*p),
                    _ => None,
                },
            },
            None => DbError::Connection(e.to_string()),
        }
    }
}

/// Converte a posição do Postgres (1-based, em caracteres) para offset em bytes no statement.
pub fn error_byte_offset(stmt: &str, position: u32) -> usize {
    let n = position.saturating_sub(1) as usize;
    stmt.char_indices().nth(n).map_or(stmt.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_offset_counts_chars_not_bytes() {
        assert_eq!(error_byte_offset("select ção, x", 13), 14);
        assert_eq!(error_byte_offset("select x", 8), 7);
        assert_eq!(error_byte_offset("ab", 99), 2);
    }
}
```

`"select ção, x"`: o 13º caractere é `x`; antes dele há 12 caracteres que ocupam 14 bytes (`ç` e `ã` têm 2 bytes cada), por isso o offset esperado é 14.

- [ ] **Step 3: Escrever os testes do lexer (falhando)**

`crates/rb-core/src/lexer.rs`, só o módulo de testes por enquanto, mais os tipos:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Word,
    QuotedIdent,
    Str,
    DollarStr,
    Number,
    Param,
    Comment,
    Semicolon,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
}

pub fn tokenize(_src: &str) -> Vec<Token> {
    unimplemented!()
}

pub fn ident_text(_tok: &str) -> String {
    unimplemented!()
}

pub fn quote_ident(_name: &str) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::TokKind::*;
    use super::*;

    fn kinds(src: &str) -> Vec<(TokKind, &str)> {
        tokenize(src).into_iter().map(|t| (t.kind, &src[t.start..t.end])).collect()
    }

    #[test]
    fn words_numbers_punct() {
        assert_eq!(
            kinds("select a1, 3.5 from t;"),
            vec![(Word, "select"), (Word, "a1"), (Punct, ","), (Number, "3.5"), (Word, "from"), (Word, "t"), (Semicolon, ";")]
        );
    }

    #[test]
    fn strings_hide_semicolons() {
        assert_eq!(kinds("'a;''b'"), vec![(Str, "'a;''b'")]);
    }

    #[test]
    fn escape_strings() {
        assert_eq!(kinds(r"E'it\'s;'"), vec![(Str, r"E'it\'s;'")]);
    }

    #[test]
    fn quoted_ident() {
        assert_eq!(kinds(r#""we""ird;""#), vec![(QuotedIdent, r#""we""ird;""#)]);
    }

    #[test]
    fn dollar_quoting() {
        assert_eq!(
            kinds("$$a;b$$ $fn$ x $$ ; $fn$"),
            vec![(DollarStr, "$$a;b$$"), (DollarStr, "$fn$ x $$ ; $fn$")]
        );
    }

    #[test]
    fn params_are_not_dollar_strings() {
        assert_eq!(kinds("$1 = $22"), vec![(Param, "$1"), (Punct, "="), (Param, "$22")]);
    }

    #[test]
    fn comments() {
        assert_eq!(
            kinds("-- a;b\nx /* a /* b; */ c */"),
            vec![(Comment, "-- a;b"), (Word, "x"), (Comment, "/* a /* b; */ c */")]
        );
    }

    #[test]
    fn unterminated_runs_to_end() {
        assert_eq!(kinds("select 'abc"), vec![(Word, "select"), (Str, "'abc")]);
        assert_eq!(kinds("$$ abc"), vec![(DollarStr, "$$ abc")]);
        assert_eq!(kinds("/* abc"), vec![(Comment, "/* abc")]);
    }

    #[test]
    fn non_ascii_identifiers() {
        assert_eq!(kinds("select ação"), vec![(Word, "select"), (Word, "ação")]);
    }

    #[test]
    fn ident_text_folds_and_unquotes() {
        assert_eq!(ident_text("Users"), "users");
        assert_eq!(ident_text(r#""My""T""#), r#"My"T"#);
    }

    #[test]
    fn quote_ident_only_when_needed() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("Order Items"), r#""Order Items""#);
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(quote_ident("select"), r#""select""#);
        assert_eq!(quote_ident("1abc"), r#""1abc""#);
    }
}
```

- [ ] **Step 4: Rodar e ver falhar**

Run: `cargo test -p rb-core lexer`
Expected: FAIL com `not implemented`.

- [ ] **Step 5: Implementar o lexer**

Substitua as três funções `unimplemented!()` por:

```rust
pub fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokKind::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Postgres aceita comentários de bloco aninhados.
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                TokKind::Comment
            }
            b'\'' => {
                i = quoted_end(b, i, b'\'', false);
                TokKind::Str
            }
            b'"' => {
                i = quoted_end(b, i, b'"', false);
                TokKind::QuotedIdent
            }
            b'e' | b'E' if b.get(i + 1) == Some(&b'\'') => {
                i = quoted_end(b, i + 1, b'\'', true);
                TokKind::Str
            }
            b'$' => match dollar_tag_end(b, i) {
                Some(open_end) => {
                    let tag = &b[i..open_end];
                    i = b[open_end..]
                        .windows(tag.len())
                        .position(|w| w == tag)
                        .map_or(b.len(), |p| open_end + p + tag.len());
                    TokKind::DollarStr
                }
                None if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    TokKind::Param
                }
                None => {
                    i += 1;
                    TokKind::Punct
                }
            },
            b';' => {
                i += 1;
                TokKind::Semicolon
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                TokKind::Number
            }
            c if is_ident_start(c) => {
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
                TokKind::Word
            }
            _ => {
                i += 1;
                TokKind::Punct
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// Fim (exclusivo) de um literal que começa em `open` com a aspa `q`.
/// Aspa dobrada é escape; com `backslash`, `\x` também é. Sem fechamento, vai até o fim.
fn quoted_end(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Se `$` em `i` abre um dollar-quote (`$$` ou `$tag$`), devolve o fim da tag de abertura.
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if j < b.len() && !b[j].is_ascii_digit() {
        while j < b.len() && is_ident_char(b[j]) && b[j] != b'$' {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

/// Nome de um identificador como o Postgres enxerga: sem aspas fica minúsculo.
pub fn ident_text(tok: &str) -> String {
    match tok.strip_prefix('"') {
        Some(rest) => rest.strip_suffix('"').unwrap_or(rest).replace("\"\"", "\""),
        None => tok.to_lowercase(),
    }
}

const RESERVED: &[&str] = &[
    "all", "and", "any", "array", "as", "asc", "both", "case", "cast", "check", "collate", "column",
    "constraint", "create", "default", "desc", "distinct", "do", "else", "end", "except", "false",
    "for", "foreign", "from", "grant", "group", "having", "in", "into", "is", "join", "leading",
    "limit", "not", "null", "offset", "on", "only", "or", "order", "primary", "references", "select",
    "table", "then", "to", "true", "union", "unique", "user", "using", "when", "where", "window", "with",
];

/// Cita o identificador só quando o Postgres exigiria.
pub fn quote_ident(name: &str) -> String {
    let plain = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'$')
        && !RESERVED.contains(&name);
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}
```

- [ ] **Step 6: Rodar e ver passar**

Run: `cargo test -p rb-core`
Expected: PASS (testes de `error` e `lexer`).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/rb-core
git commit -m "feat(core): workspace, DbError e lexer SQL"
```

---

### Task 2: Divisão de statements

**Files:**
- Create: `crates/rb-core/src/split.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod split;`)

**Interfaces:**
- Consumes: `lexer::{tokenize, TokKind}`
- Produces: `split::split_statements(src: &str) -> Vec<Range<usize>>`, `split::statement_at(src: &str, offset: usize) -> Option<Range<usize>>`. Os ranges são bytes de `src`, sem o `;`, sem comentários nas pontas.

- [ ] **Step 1: Testes (falhando)**

```rust
use std::ops::Range;

use crate::lexer::{TokKind, tokenize};

pub fn split_statements(_src: &str) -> Vec<Range<usize>> {
    unimplemented!()
}

pub fn statement_at(_src: &str, _offset: usize) -> Option<Range<usize>> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str) -> Vec<&str> {
        split_statements(src).into_iter().map(|r| &src[r]).collect()
    }

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(texts("select 1; select 2"), vec!["select 1", "select 2"]);
    }

    #[test]
    fn keeps_function_bodies_whole() {
        let src = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\nselect f()";
        assert_eq!(
            texts(src),
            vec!["create function f() returns int as $$ begin; return 1; end $$ language plpgsql", "select f()"]
        );
    }

    #[test]
    fn ignores_empty_and_comment_only_statements() {
        assert_eq!(texts(";; -- nada\n; /* x */ select 1 -- fim\n;"), vec!["select 1"]);
    }

    #[test]
    fn keeps_inner_comments() {
        assert_eq!(texts("select /* a; */ 1"), vec!["select /* a; */ 1"]);
    }

    #[test]
    fn statement_at_picks_containing_or_previous() {
        let src = "select 1;\n\nselect 2;\n\n";
        let at = |o| statement_at(src, o).map(|r| &src[r]);
        assert_eq!(at(3), Some("select 1"));
        assert_eq!(at(11), Some("select 2"));
        assert_eq!(at(19), Some("select 2")); // logo após "select 2"
        assert_eq!(at(src.len()), Some("select 2")); // linha em branco no fim
        assert_eq!(at(10), Some("select 1")); // linha em branco entre os dois
        assert_eq!(statement_at("   ", 1), None);
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core split`
Expected: FAIL com `not implemented`.

- [ ] **Step 3: Implementar**

```rust
/// Divide o script em statements pelo lexer, respeitando strings, dollar-quotes e comentários.
pub fn split_statements(src: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut cur: Option<Range<usize>> = None;
    for t in tokenize(src) {
        match t.kind {
            TokKind::Semicolon => out.extend(cur.take()),
            TokKind::Comment => {}
            _ => match &mut cur {
                Some(r) => r.end = t.end,
                None => cur = Some(t.start..t.end),
            },
        }
    }
    out.extend(cur);
    out
}

/// Statement sob o cursor; se o cursor estiver entre statements, o anterior; senão o primeiro.
pub fn statement_at(src: &str, offset: usize) -> Option<Range<usize>> {
    let stmts = split_statements(src);
    stmts
        .iter()
        .find(|r| r.start <= offset && offset <= r.end)
        .or_else(|| stmts.iter().rev().find(|r| r.end < offset))
        .or(stmts.first())
        .cloned()
}
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core split`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): divisão de statements e statement sob o cursor"
```

---

### Task 3: Decodificação binária e ResultSet

**Files:**
- Create: `crates/rb-core/src/decode.rs`, `crates/rb-core/src/batch.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod batch; pub mod decode;`)

**Interfaces:**
- Produces:
  - `decode::cell_text(ty: &Type, raw: &[u8]) -> String` (texto completo, aceito pelo input do Postgres)
  - `decode::display_text(ty: &Type, raw: &[u8], max_chars: usize) -> String` (uma linha, truncada com `…`)
  - `decode::one_line(s: &str, max_chars: usize) -> String`
  - `batch::ColumnInfo { name: String, type_: Type }`
  - `batch::Batch` com `empty(ncols)`, `push_row(&[Option<&[u8]>])`, `from_rows(&[Row], ncols)`, `rows()`, `cell(row, col) -> Option<&[u8]>`
  - `batch::ResultSet { pub columns: Arc<[ColumnInfo]>, pub more: bool, .. }` com `new(columns, first: Batch, more)`, `push(batch, more)`, `rows()`, `raw(row, col)`, `text(row, col) -> Option<String>`, `display(row, col, max_chars) -> Option<String>`, `to_tsv(rows: Range<usize>, cols: &[usize]) -> String`
  - `Type` é `tokio_postgres::types::Type`.

- [ ] **Step 1: Testes de `decode` (falhando)**

`crates/rb-core/src/decode.rs`:
```rust
use tokio_postgres::types::{Kind, Type};

pub fn cell_text(_ty: &Type, _raw: &[u8]) -> String {
    unimplemented!()
}

pub fn display_text(_ty: &Type, _raw: &[u8], _max_chars: usize) -> String {
    unimplemented!()
}

pub fn one_line(_s: &str, _max_chars: usize) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(ndigits.to_be_bytes());
        v.extend(weight.to_be_bytes());
        v.extend(sign.to_be_bytes());
        v.extend(dscale.to_be_bytes());
        for d in digits {
            v.extend(d.to_be_bytes());
        }
        v
    }

    #[test]
    fn scalars() {
        assert_eq!(cell_text(&Type::BOOL, &[1]), "true");
        assert_eq!(cell_text(&Type::INT2, &(-7i16).to_be_bytes()), "-7");
        assert_eq!(cell_text(&Type::INT4, &42i32.to_be_bytes()), "42");
        assert_eq!(cell_text(&Type::INT8, &(1i64 << 40).to_be_bytes()), "1099511627776");
        assert_eq!(cell_text(&Type::FLOAT8, &1.5f64.to_be_bytes()), "1.5");
        assert_eq!(cell_text(&Type::FLOAT4, &f32::INFINITY.to_be_bytes()), "Infinity");
        assert_eq!(cell_text(&Type::TEXT, "olá".as_bytes()), "olá");
        assert_eq!(cell_text(&Type::CHAR, b"r"), "r");
        assert_eq!(cell_text(&Type::BYTEA, &[0xde, 0xad]), "\\xdead");
    }

    #[test]
    fn numerics() {
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(2, 0, 0, 2, &[123, 4500])), "123.45");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(1, -1, 0x4000, 3, &[10])), "-0.001");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(1, 1, 0, 0, &[1])), "10000");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(0, 0, 0, 0, &[])), "0");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(0, 0, 0xC000, 0, &[])), "NaN");
    }

    #[test]
    fn dates_and_times() {
        // 2024-02-29 = 8825 dias após 2000-01-01
        assert_eq!(cell_text(&Type::DATE, &8825i32.to_be_bytes()), "2024-02-29");
        assert_eq!(cell_text(&Type::DATE, &(-1i32).to_be_bytes()), "1999-12-31");
        assert_eq!(cell_text(&Type::DATE, &i32::MAX.to_be_bytes()), "infinity");
        let us: i64 = 8825 * 86_400_000_000 + (13 * 3600 + 45 * 60 + 30) * 1_000_000 + 500_000;
        assert_eq!(cell_text(&Type::TIMESTAMP, &us.to_be_bytes()), "2024-02-29 13:45:30.5");
        assert_eq!(cell_text(&Type::TIMESTAMPTZ, &us.to_be_bytes()), "2024-02-29 13:45:30.5+00");
        assert_eq!(cell_text(&Type::TIMESTAMPTZ, &i64::MAX.to_be_bytes()), "infinity");
        assert_eq!(cell_text(&Type::TIME, &(3_600_000_000i64).to_be_bytes()), "01:00:00");
    }

    #[test]
    fn intervals() {
        let mut v = Vec::new();
        v.extend(((4 * 3600 + 5 * 60 + 6) * 1_000_000i64).to_be_bytes());
        v.extend(3i32.to_be_bytes());
        v.extend(14i32.to_be_bytes());
        assert_eq!(cell_text(&Type::INTERVAL, &v), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(cell_text(&Type::INTERVAL, &[0u8; 16]), "00:00:00");
    }

    #[test]
    fn uuid_json_jsonb() {
        let u: Vec<u8> = (0u8..16).collect();
        assert_eq!(cell_text(&Type::UUID, &u), "00010203-0405-0607-0809-0a0b0c0d0e0f");
        assert_eq!(cell_text(&Type::JSON, br#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(cell_text(&Type::JSONB, b"\x01{\"a\":1}"), r#"{"a":1}"#);
    }

    #[test]
    fn arrays() {
        let mut v = Vec::new();
        for x in [1i32, 1, 23, 3, 1] {
            v.extend(x.to_be_bytes()); // ndim, has_null, oid, len, lbound
        }
        v.extend(4i32.to_be_bytes());
        v.extend(1i32.to_be_bytes());
        v.extend((-1i32).to_be_bytes());
        v.extend(4i32.to_be_bytes());
        v.extend(3i32.to_be_bytes());
        assert_eq!(cell_text(&Type::INT4_ARRAY, &v), "{1,NULL,3}");

        let mut t = Vec::new();
        for x in [1i32, 0, 25, 2, 1] {
            t.extend(x.to_be_bytes());
        }
        t.extend(3i32.to_be_bytes());
        t.extend(b"a b");
        t.extend(0i32.to_be_bytes());
        assert_eq!(cell_text(&Type::TEXT_ARRAY, &t), r#"{"a b",""}"#);
    }

    #[test]
    fn enums_domains_and_unknown() {
        let mood = Type::new("mood".into(), 90001, Kind::Enum(vec![]), "public".into());
        assert_eq!(cell_text(&mood, b"sad"), "sad");
        let dom = Type::new("pos".into(), 90002, Kind::Domain(Type::INT4), "public".into());
        assert_eq!(cell_text(&dom, &5i32.to_be_bytes()), "5");
        assert_eq!(cell_text(&Type::POINT, &[0u8; 16]), "<point: 16 bytes>");
        assert_eq!(cell_text(&Type::INT4, &[1, 2]), "<int4: 2 bytes>");
    }

    #[test]
    fn display_is_single_line() {
        assert_eq!(display_text(&Type::TEXT, b"ab\ncd\te", 50), "ab cd e");
    }

    #[test]
    fn display_truncates_huge_text_without_full_decode() {
        let big = vec![b'x'; 5_000_000];
        let s = display_text(&Type::TEXT, &big, 100);
        assert_eq!(s.chars().count(), 101);
        assert!(s.ends_with('…'));
        let jb = [b"\x01".as_slice(), &vec![b'y'; 1_000_000]].concat();
        assert!(display_text(&Type::JSONB, &jb, 10).starts_with("yyyyyyyyyy…"));
        assert_eq!(display_text(&Type::BYTEA, &vec![0xab; 1_000_000], 10), "\\xababababa…");
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core decode`
Expected: FAIL com `not implemented`.

- [ ] **Step 3: Implementar `decode`**

Substitua as três funções e adicione os auxiliares:

```rust
/// Texto completo do valor no formato que o Postgres aceita de volta como entrada.
pub fn cell_text(ty: &Type, raw: &[u8]) -> String {
    decode(ty, raw).unwrap_or_else(|| format!("<{}: {} bytes>", ty.name(), raw.len()))
}

/// Texto para a grade: uma linha, no máximo `max_chars` caracteres (+ `…`).
/// Para tipos textuais e bytea grandes, decodifica só o começo.
pub fn display_text(ty: &Type, raw: &[u8], max_chars: usize) -> String {
    let cap = max_chars * 4 + 8;
    let text = if *ty == Type::BYTEA && raw.len() > max_chars {
        format!("\\x{}", hex(&raw[..max_chars / 2 + 1]))
    } else if raw.len() > cap && is_textual(ty) {
        let body = if *ty == Type::JSONB { &raw[1..] } else { raw };
        String::from_utf8_lossy(&body[..cap.min(body.len())]).into_owned()
    } else {
        cell_text(ty, raw)
    };
    one_line(&text, max_chars)
}

/// Troca caracteres de controle por espaço e corta em `max_chars` com `…`.
pub fn one_line(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4) + 3);
    for (n, ch) in s.chars().enumerate() {
        if n == max_chars {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

fn is_textual(ty: &Type) -> bool {
    matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::JSON | Type::JSONB | Type::XML)
        || matches!(ty.kind(), Kind::Enum(_))
}

fn decode(ty: &Type, r: &[u8]) -> Option<String> {
    match ty.kind() {
        Kind::Enum(_) => return Some(utf8(r)),
        Kind::Domain(base) => return decode(base, r),
        Kind::Array(elem) => return array(elem, r),
        _ => {}
    }
    Some(match *ty {
        Type::BOOL => (if *r.first()? != 0 { "true" } else { "false" }).to_string(),
        Type::INT2 => i16::from_be_bytes(be(r, 0)?).to_string(),
        Type::INT4 => i32::from_be_bytes(be(r, 0)?).to_string(),
        Type::INT8 => i64::from_be_bytes(be(r, 0)?).to_string(),
        Type::OID => u32::from_be_bytes(be(r, 0)?).to_string(),
        Type::FLOAT4 => float(f32::from_be_bytes(be(r, 0)?)),
        Type::FLOAT8 => float(f64::from_be_bytes(be(r, 0)?)),
        Type::NUMERIC => numeric(r)?,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::JSON | Type::XML | Type::UNKNOWN => utf8(r),
        Type::JSONB => utf8(r.get(1..)?),
        Type::CHAR => (*r.first()? as char).to_string(),
        Type::UUID => uuid(r)?,
        Type::BYTEA => format!("\\x{}", hex(r)),
        Type::DATE => date(i32::from_be_bytes(be(r, 0)?)),
        Type::TIME => time(i64::from_be_bytes(be(r, 0)?)),
        Type::TIMESTAMP => timestamp(i64::from_be_bytes(be(r, 0)?)),
        Type::TIMESTAMPTZ => {
            let t = timestamp(i64::from_be_bytes(be(r, 0)?));
            if t.ends_with("infinity") { t } else { t + "+00" }
        }
        Type::INTERVAL => interval(
            i64::from_be_bytes(be(r, 0)?),
            i32::from_be_bytes(be(r, 8)?),
            i32::from_be_bytes(be(r, 12)?),
        ),
        _ => return None,
    })
}

fn be<const N: usize>(r: &[u8], at: usize) -> Option<[u8; N]> {
    r.get(at..at + N)?.try_into().ok()
}

fn utf8(r: &[u8]) -> String {
    String::from_utf8_lossy(r).into_owned()
}

fn float<F: std::fmt::Display + Into<f64> + Copy>(f: F) -> String {
    let x: f64 = f.into();
    if x.is_infinite() {
        (if x > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
    } else if x.is_nan() {
        "NaN".to_string()
    } else {
        f.to_string()
    }
}

fn hex(r: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(r.len() * 2);
    for b in r {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

fn uuid(r: &[u8]) -> Option<String> {
    let h = hex(r.get(..16)?);
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Formato binário de numeric: ndigits, weight, sign, dscale, dígitos base 10000.
fn numeric(r: &[u8]) -> Option<String> {
    let ndigits = i16::from_be_bytes(be(r, 0)?) as i32;
    let weight = i16::from_be_bytes(be(r, 2)?) as i32;
    let sign = u16::from_be_bytes(be(r, 4)?);
    let dscale = u16::from_be_bytes(be(r, 6)?) as usize;
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => {}
    }
    let digit = |k: i32| -> Option<i16> {
        if k < 0 || k >= ndigits {
            return Some(0);
        }
        Some(i16::from_be_bytes(be(r, 8 + 2 * k as usize)?))
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for k in 0..=weight {
            let d = digit(k)?;
            if k == 0 { s += &d.to_string() } else { s += &format!("{d:04}") }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let mut k = weight + 1;
        while frac.len() < dscale {
            frac += &format!("{:04}", digit(k)?);
            k += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s += &frac;
    }
    Some(s)
}

const PG_EPOCH_UNIX_DAYS: i64 = 10_957; // 2000-01-01 - 1970-01-01
const DAY_US: i64 = 86_400_000_000;

/// Dias desde 1970-01-01 para (ano, mês, dia). Algoritmo "civil_from_days" de H. Hinnant.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn ymd(days_since_2000: i64) -> String {
    let (y, m, d) = civil(days_since_2000 + PG_EPOCH_UNIX_DAYS);
    format!("{y:04}-{m:02}-{d:02}")
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        d => ymd(d as i64),
    }
}

fn time(us: i64) -> String {
    let (s, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let mut out = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if frac != 0 {
        out += format!(".{frac:06}").trim_end_matches('0');
    }
    out
}

fn timestamp(us: i64) -> String {
    match us {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        us => format!("{} {}", ymd(us.div_euclid(DAY_US)), time(us.rem_euclid(DAY_US))),
    }
}

fn interval(us: i64, days: i32, months: i32) -> String {
    let plural = |n: i32, one: &str, many: &str| format!("{n} {}", if n.abs() == 1 { one } else { many });
    let mut parts = Vec::new();
    if months / 12 != 0 {
        parts.push(plural(months / 12, "year", "years"));
    }
    if months % 12 != 0 {
        parts.push(plural(months % 12, "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(days, "day", "days"));
    }
    if us != 0 || parts.is_empty() {
        let sign = if us < 0 { "-" } else { "" };
        parts.push(format!("{sign}{}", time(us.unsigned_abs() as i64)));
    }
    parts.join(" ")
}

fn array(elem: &Type, r: &[u8]) -> Option<String> {
    let ndim = i32::from_be_bytes(be(r, 0)?) as usize;
    if ndim == 0 {
        return Some("{}".into());
    }
    let mut dims = Vec::with_capacity(ndim);
    for d in 0..ndim {
        dims.push(i32::from_be_bytes(be(r, 12 + d * 8)?) as usize);
    }
    let mut pos = 12 + ndim * 8;
    let mut out = String::new();
    array_dim(elem, r, &dims, &mut pos, &mut out)?;
    Some(out)
}

fn array_dim(elem: &Type, r: &[u8], dims: &[usize], pos: &mut usize, out: &mut String) -> Option<()> {
    out.push('{');
    for i in 0..dims[0] {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            array_dim(elem, r, &dims[1..], pos, out)?;
            continue;
        }
        let len = i32::from_be_bytes(be(r, *pos)?);
        *pos += 4;
        if len < 0 {
            out.push_str("NULL");
            continue;
        }
        let v = r.get(*pos..*pos + len as usize)?;
        *pos += len as usize;
        let s = cell_text(elem, v);
        let needs_quotes = s.is_empty()
            || s.eq_ignore_ascii_case("null")
            || s.contains(|c: char| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
        if needs_quotes {
            out.push('"');
            for c in s.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out.push('}');
    Some(())
}
```

Nota: no teste de bytea grande, `max_chars = 10` → 6 bytes viram `\x` + 12 hex = 14 chars, cortados em 10 + `…` = `\xababababa…`.

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core decode`
Expected: PASS

- [ ] **Step 5: Testes de `batch` (falhando)**

`crates/rb-core/src/batch.rs`:
```rust
use std::ops::Range;
use std::sync::Arc;

use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, Type};

use crate::decode;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_: Type,
}

const NULL: u32 = u32::MAX;

/// Lote de linhas em formato binário cru: um buffer contíguo + (offset, len) por célula.
#[derive(Debug, Default)]
pub struct Batch {
    data: Vec<u8>,
    cells: Vec<(u32, u32)>,
    ncols: usize,
}

#[derive(Debug)]
pub struct ResultSet {
    pub columns: Arc<[ColumnInfo]>,
    pub more: bool,
    batches: Vec<Batch>,
    ends: Vec<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Arc<[ColumnInfo]> {
        Arc::from(vec![
            ColumnInfo { name: "id".into(), type_: Type::INT4 },
            ColumnInfo { name: "t".into(), type_: Type::TEXT },
        ])
    }

    fn batch(rows: &[(i32, Option<&str>)]) -> Batch {
        let mut b = Batch::empty(2);
        for (id, t) in rows {
            let id = id.to_be_bytes();
            b.push_row(&[Some(&id), t.map(str::as_bytes)]);
        }
        b
    }

    #[test]
    fn batch_cells_and_nulls() {
        let b = batch(&[(1, Some("a")), (2, None)]);
        assert_eq!(b.rows(), 2);
        assert_eq!(b.cell(0, 1), Some(&b"a"[..]));
        assert_eq!(b.cell(1, 1), None);
    }

    #[test]
    fn result_set_spans_batches() {
        let mut rs = ResultSet::new(cols(), batch(&[(1, Some("a")), (2, Some("b"))]), true);
        rs.push(batch(&[(3, Some("c"))]), true);
        rs.push(Batch::empty(2), false);
        assert_eq!(rs.rows(), 3);
        assert!(!rs.more);
        assert_eq!(rs.text(2, 0).as_deref(), Some("3"));
        assert_eq!(rs.text(1, 1).as_deref(), Some("b"));
    }

    #[test]
    fn tsv_quotes_special_fields() {
        let rs = ResultSet::new(cols(), batch(&[(1, Some("a\tb")), (2, Some("x\"y")), (3, None)]), false);
        assert_eq!(rs.to_tsv(0..3, &[0, 1]), "1\t\"a\tb\"\n2\t\"x\"\"y\"\n3\t\n");
    }

    #[test]
    fn display_marks_null_as_none() {
        let rs = ResultSet::new(cols(), batch(&[(1, None)]), false);
        assert_eq!(rs.display(0, 1, 10), None);
        assert_eq!(rs.display(0, 0, 10).as_deref(), Some("1"));
    }
}
```

- [ ] **Step 6: Rodar e ver falhar**

Run: `cargo test -p rb-core batch`
Expected: FAIL de compilação (`Batch::empty`, `push_row`, `ResultSet::new` não existem).

- [ ] **Step 7: Implementar `Batch` e `ResultSet`**

Acrescente em `batch.rs`, antes de `#[cfg(test)]`:

```rust
/// Aceita qualquer tipo e guarda os bytes crus, sem decodificar.
struct Raw<'a>(&'a [u8]);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

impl Batch {
    pub fn empty(ncols: usize) -> Batch {
        Batch { data: Vec::new(), cells: Vec::new(), ncols }
    }

    pub fn from_rows(rows: &[Row], ncols: usize) -> Batch {
        let mut b = Batch { data: Vec::new(), cells: Vec::with_capacity(rows.len() * ncols), ncols };
        for row in rows {
            for i in 0..ncols {
                // Raw aceita todos os tipos; erro aqui só seria coluna inexistente.
                let v = row.try_get::<_, Option<Raw>>(i).ok().flatten();
                b.push_cell(v.map(|r| r.0));
            }
        }
        b
    }

    pub fn push_row(&mut self, cells: &[Option<&[u8]>]) {
        debug_assert_eq!(cells.len(), self.ncols);
        for c in cells {
            self.push_cell(*c);
        }
    }

    fn push_cell(&mut self, v: Option<&[u8]>) {
        match v {
            Some(bytes) => {
                self.cells.push((self.data.len() as u32, bytes.len() as u32));
                self.data.extend_from_slice(bytes);
            }
            None => self.cells.push((0, NULL)),
        }
    }

    pub fn rows(&self) -> usize {
        if self.ncols == 0 { 0 } else { self.cells.len() / self.ncols }
    }

    /// Bytes crus da célula; `None` = NULL.
    pub fn cell(&self, row: usize, col: usize) -> Option<&[u8]> {
        let (off, len) = self.cells[row * self.ncols + col];
        (len != NULL).then(|| &self.data[off as usize..(off + len) as usize])
    }
}

impl ResultSet {
    pub fn new(columns: Arc<[ColumnInfo]>, first: Batch, more: bool) -> ResultSet {
        let mut rs = ResultSet { columns, more, batches: Vec::new(), ends: Vec::new() };
        rs.push(first, more);
        rs
    }

    pub fn push(&mut self, batch: Batch, more: bool) {
        self.more = more;
        if batch.rows() == 0 {
            return;
        }
        self.ends.push(self.rows() + batch.rows());
        self.batches.push(batch);
    }

    pub fn rows(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn raw(&self, row: usize, col: usize) -> Option<&[u8]> {
        let b = self.ends.partition_point(|&e| e <= row);
        let start = if b == 0 { 0 } else { self.ends[b - 1] };
        self.batches[b].cell(row - start, col)
    }

    /// Valor completo como texto; `None` = NULL.
    pub fn text(&self, row: usize, col: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::cell_text(&self.columns[col].type_, r))
    }

    /// Valor para exibição (uma linha, truncado); `None` = NULL.
    pub fn display(&self, row: usize, col: usize, max_chars: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::display_text(&self.columns[col].type_, r, max_chars))
    }

    /// TSV no formato que planilhas aceitam: NULL vira vazio, campos especiais vão entre aspas.
    pub fn to_tsv(&self, rows: Range<usize>, cols: &[usize]) -> String {
        let mut out = String::new();
        for row in rows {
            for (k, &c) in cols.iter().enumerate() {
                if k > 0 {
                    out.push('\t');
                }
                let v = self.text(row, c).unwrap_or_default();
                if v.contains(['\t', '\n', '\r', '"']) {
                    out.push('"');
                    out.push_str(&v.replace('"', "\"\""));
                    out.push('"');
                } else {
                    out.push_str(&v);
                }
            }
            out.push('\n');
        }
        out
    }
}
```

- [ ] **Step 8: Rodar e ver passar**

Run: `cargo test -p rb-core`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): decodificação binária preguiçosa e ResultSet paginado"
```

---

### Task 4: Configuração e Keychain

**Files:**
- Create: `crates/rb-core/src/config.rs`, `crates/rb-core/src/secrets.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod config; pub mod secrets;`)

**Interfaces:**
- Produces:
  - `config::SslMode { Disable (default), Prefer, Require }` (serde em minúsculas)
  - `config::ConnectionConfig { id, name, host, port: u16, database, user, ssl: SslMode }` + `pg_config(&self, password: Option<&str>) -> tokio_postgres::Config`
  - `config::ConfigFile { connections: Vec<ConnectionConfig> }` (TOML: `[[connection]]`)
  - `config::{app_dir() -> PathBuf, connections_path() -> PathBuf, scripts_dir() -> PathBuf, load(&Path) -> Result<ConfigFile, DbError>, save(&Path, &ConfigFile) -> Result<(), DbError>, new_id() -> String}`
  - `secrets::{get_password(id) -> Result<Option<String>, DbError>, set_password(id, pw) -> Result<(), DbError>, delete_password(id) -> Result<(), DbError>}`

- [ ] **Step 1: Testes de config (falhando)**

`crates/rb-core/src/config.rs`:
```rust
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    #[default]
    Disable,
    Prefer,
    /// TLS obrigatório, com certificado verificado pelas raízes do sistema.
    Require,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl: SslMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default, rename = "connection")]
    pub connections: Vec<ConnectionConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rb-config-{}-{name}", std::process::id())).join("connections.toml")
    }

    fn sample() -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(),
            name: "local".into(),
            host: "localhost".into(),
            port: 5432,
            database: "app".into(),
            user: "postgres".into(),
            ssl: SslMode::Require,
        }
    }

    #[test]
    fn missing_file_is_empty_config() {
        assert_eq!(load(&tmp("missing")).unwrap(), ConfigFile::default());
    }

    #[test]
    fn save_then_load_round_trips_without_password() {
        let path = tmp("roundtrip");
        let cfg = ConfigFile { connections: vec![sample()] };
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[connection]]") && text.contains("ssl = \"require\""));
        assert!(!text.contains("password"));
        assert_eq!(load(&path).unwrap(), cfg);
    }

    #[test]
    fn invalid_toml_is_config_error() {
        let path = tmp("invalid");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[[connection]]\nport = \"x\"").unwrap();
        assert!(matches!(load(&path), Err(DbError::Config(_))));
    }

    #[test]
    fn pg_config_carries_fields() {
        let pg = sample().pg_config(Some("pw"));
        assert_eq!(pg.get_user(), Some("postgres"));
        assert_eq!(pg.get_dbname(), Some("app"));
        assert_eq!(pg.get_ports(), &[5432]);
        assert_eq!(pg.get_password(), Some(&b"pw"[..]));
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core config`
Expected: FAIL de compilação (`load`, `save`, `pg_config`, `new_id` não existem).

- [ ] **Step 3: Implementar config**

Acrescente antes dos testes:

```rust
impl ConnectionConfig {
    pub fn pg_config(&self, password: Option<&str>) -> tokio_postgres::Config {
        use tokio_postgres::config::SslMode as Pg;
        let mut c = tokio_postgres::Config::new();
        c.host(&self.host)
            .port(self.port)
            .dbname(&self.database)
            .user(&self.user)
            .application_name("rustbeaver")
            .connect_timeout(Duration::from_secs(10))
            .ssl_mode(match self.ssl {
                SslMode::Disable => Pg::Disable,
                SslMode::Prefer => Pg::Prefer,
                SslMode::Require => Pg::Require,
            });
        if let Some(p) = password {
            c.password(p);
        }
        c
    }
}

pub fn app_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/rustbeaver")
}

pub fn connections_path() -> PathBuf {
    app_dir().join("connections.toml")
}

pub fn scripts_dir() -> PathBuf {
    app_dir().join("scripts")
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("c{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn io(e: std::io::Error) -> DbError {
    DbError::Io(e.to_string())
}

pub fn load(path: &Path) -> Result<ConfigFile, DbError> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| DbError::Config(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFile::default()),
        Err(e) => Err(io(e)),
    }
}

/// Grava de forma atômica (arquivo temporário + rename).
pub fn save(path: &Path, cfg: &ConfigFile) -> Result<(), DbError> {
    let text = toml::to_string_pretty(cfg).map_err(|e| DbError::Config(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core config`
Expected: PASS

- [ ] **Step 5: Keychain**

`crates/rb-core/src/secrets.rs`:
```rust
use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

use crate::DbError;

const SERVICE: &str = "dev.rustbeaver";
const NOT_FOUND: i32 = -25300; // errSecItemNotFound

fn err(e: security_framework::base::Error) -> DbError {
    DbError::Keychain(e.to_string())
}

pub fn get_password(id: &str) -> Result<Option<String>, DbError> {
    match get_generic_password(SERVICE, id) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|e| DbError::Keychain(e.to_string())),
        Err(e) if e.code() == NOT_FOUND => Ok(None),
        Err(e) => Err(err(e)),
    }
}

pub fn set_password(id: &str, password: &str) -> Result<(), DbError> {
    set_generic_password(SERVICE, id, password.as_bytes()).map_err(err)
}

pub fn delete_password(id: &str) -> Result<(), DbError> {
    match delete_generic_password(SERVICE, id) {
        Err(e) if e.code() != NOT_FOUND => Err(err(e)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mexe no Keychain real; rode manualmente: `cargo test -p rb-core keychain -- --ignored`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let id = format!("rb-test-{}", std::process::id());
        assert_eq!(get_password(&id).unwrap(), None);
        set_password(&id, "s3nha").unwrap();
        assert_eq!(get_password(&id).unwrap().as_deref(), Some("s3nha"));
        delete_password(&id).unwrap();
        assert_eq!(get_password(&id).unwrap(), None);
    }
}
```

- [ ] **Step 6: Rodar o teste do Keychain manualmente**

Run: `cargo test -p rb-core keychain -- --ignored`
Expected: PASS (o macOS pode pedir permissão; aceite).

- [ ] **Step 7: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): config TOML de conexões e senhas no Keychain"
```

---

### Task 5: Engine, sessão, execução paginada

**Files:**
- Create: `crates/rb-core/src/session.rs`, `crates/rb-core/tests/common/mod.rs`, `crates/rb-core/tests/session.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod session;`)

**Interfaces:**
- Consumes: `ConnectionConfig::pg_config`, `Batch::{empty, from_rows}`, `ColumnInfo`, `DbError`
- Produces:
  - `session::FETCH_ROWS: i32 = 1000`
  - `session::Engine` (`Clone`): `new() -> Result<Engine, DbError>`, `async connect(&self, cfg: &ConnectionConfig, password: Option<String>) -> Result<Session, DbError>`
  - `session::Session`: `async execute(&self, sql: String) -> Result<ExecOutcome, DbError>`, `async fetch_more(&self) -> Result<FetchOutcome, DbError>`, `async query_text(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, DbError>`, `async run_in_transaction(&self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError>`, `cancel(&self)` (cancel vem na Tarefa 6)
  - `session::ExecOutcome { columns: Arc<[ColumnInfo]>, batch: Batch, more: bool, affected: Option<u64>, elapsed: Duration }`
  - `session::FetchOutcome { batch: Batch, more: bool }`
  - `session::SqlWithParams { sql: String, params: Vec<Option<String>>, expect_one: bool }`

**Semântica (da spec, com o ajuste aprovado):** todo statement é preparado. Se retornar colunas, roda num portal dentro de `BEGIN` e busca lotes de 1.000; a transação faz `COMMIT` quando o portal esgota ou quando chega qualquer outro comando. Se não retornar colunas, roda em autocommit e devolve as linhas afetadas.

- [ ] **Step 1: Postgres de teste**

Run:
```bash
docker run -d --name rb-pg -e POSTGRES_PASSWORD=rb -p 55432:5432 postgres:17
export RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres
```
Expected: container rodando (`docker ps` mostra `rb-pg`).

- [ ] **Step 2: Helpers de integração**

`crates/rb-core/tests/common/mod.rs`:
```rust
#![allow(dead_code)]

use rb_core::config::{ConnectionConfig, SslMode};
use rb_core::session::{Engine, Session};

/// Conexão de teste a partir de RB_TEST_DATABASE_URL; `None` se a variável não existir.
pub fn open() -> Option<(Engine, Session)> {
    let url = std::env::var("RB_TEST_DATABASE_URL").ok()?;
    let pg: tokio_postgres::Config = url.parse().expect("RB_TEST_DATABASE_URL inválida");
    let host = match &pg.get_hosts()[0] {
        tokio_postgres::config::Host::Tcp(h) => h.clone(),
        _ => panic!("use host TCP em RB_TEST_DATABASE_URL"),
    };
    let cfg = ConnectionConfig {
        id: "test".into(),
        name: "test".into(),
        host,
        port: pg.get_ports().first().copied().unwrap_or(5432),
        database: pg.get_dbname().unwrap_or("postgres").into(),
        user: pg.get_user().unwrap_or("postgres").into(),
        ssl: SslMode::Disable,
    };
    let password = pg.get_password().map(|p| String::from_utf8_lossy(p).into_owned());
    let engine = Engine::new().expect("runtime");
    let session = pollster::block_on(engine.connect(&cfg, password)).expect("conectar ao Postgres de teste");
    Some((engine, session))
}

pub fn skip() {
    eprintln!("RB_TEST_DATABASE_URL ausente; teste de integração pulado");
}

pub fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{n}")
}
```

- [ ] **Step 3: Testes de integração (falhando)**

`crates/rb-core/tests/session.rs`:
```rust
mod common;

use pollster::block_on;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::session::SqlWithParams;

#[test]
fn select_pages_in_batches_of_1000() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g, g::text || 'x' from generate_series(1, 2500) g".into()).await.unwrap();
        assert_eq!(out.columns.len(), 2);
        assert_eq!(out.batch.rows(), 1000);
        assert!(out.more);
        assert_eq!(out.affected, None);
        let mut rs = ResultSet::new(out.columns, out.batch, out.more);
        while rs.more {
            let f = s.fetch_more().await.unwrap();
            rs.push(f.batch, f.more);
        }
        assert_eq!(rs.rows(), 2500);
        assert_eq!(rs.text(2499, 0).as_deref(), Some("2500"));
        assert_eq!(rs.text(0, 1).as_deref(), Some("1x"));
    });
}

#[test]
fn exact_multiple_of_page_size_terminates() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g from generate_series(1, 2000) g".into()).await.unwrap();
        assert!(out.more);
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (1000, true));
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (0, false));
        let f = s.fetch_more().await.unwrap(); // sem portal aberto: vazio
        assert_eq!((f.batch.rows(), f.more), (0, false));
    });
}

#[test]
fn million_rows_page_through() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g, md5(g::text) from generate_series(1, 1000000) g".into()).await.unwrap();
        let mut rs = ResultSet::new(out.columns, out.batch, out.more);
        while rs.more {
            let f = s.fetch_more().await.unwrap();
            rs.push(f.batch, f.more);
        }
        assert_eq!(rs.rows(), 1_000_000);
        assert_eq!(rs.text(999_999, 0).as_deref(), Some("1000000"));
    });
}

#[test]
fn ddl_and_dml_report_affected_rows() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        s.execute("create temp table t (a int)".into()).await.unwrap();
        let out = s.execute("insert into t select generate_series(1, 5)".into()).await.unwrap();
        assert_eq!(out.affected, Some(5));
        assert!(out.columns.is_empty());
        let out = s.execute("insert into t values (9) returning a * 2".into()).await.unwrap();
        assert_eq!(out.batch.rows(), 1);
    });
}

#[test]
fn postgres_errors_keep_sqlstate_and_position() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let err = s.execute("select nope from pg_class".into()).await.unwrap_err();
        assert!(matches!(err, DbError::Postgres { ref code, position: Some(8), .. } if code == "42703"), "{err:?}");
        // a sessão continua utilizável
        assert_eq!(s.execute("select 1".into()).await.unwrap().batch.rows(), 1);
    });
}

#[test]
fn query_text_returns_strings_and_nulls() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let rows = s.query_text("select $1::text, null::text", &["a"]).await.unwrap();
        assert_eq!(rows, vec![vec![Some("a".to_string()), None]]);
    });
}

#[test]
fn transaction_rolls_back_when_expect_one_fails() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        s.execute("create temp table tx (a int)".into()).await.unwrap();
        let stmts = vec![
            SqlWithParams { sql: "insert into tx values (($1::text)::int4)".into(), params: vec![Some("1".into())], expect_one: true },
            SqlWithParams { sql: "update tx set a = 2 where a = 999".into(), params: vec![], expect_one: true },
        ];
        assert!(matches!(s.run_in_transaction(stmts).await, Err(DbError::Edit(_))));
        let rows = s.query_text("select count(*)::text from tx", &[]).await.unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    });
}
```

- [ ] **Step 4: Rodar e ver falhar**

Run: `cargo test -p rb-core --test session`
Expected: FAIL de compilação (`rb_core::session` não existe).

- [ ] **Step 5: Implementar `session.rs`**

```rust
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::DbError;
use crate::batch::{Batch, ColumnInfo};
use crate::config::ConnectionConfig;

pub const FETCH_ROWS: i32 = 1000;

/// Runtime tokio dedicado ao I/O de banco. Barato de clonar.
#[derive(Clone)]
pub struct Engine {
    rt: Arc<Runtime>,
}

#[derive(Debug)]
pub struct ExecOutcome {
    pub columns: Arc<[ColumnInfo]>,
    pub batch: Batch,
    pub more: bool,
    /// Linhas afetadas, para statements que não retornam colunas.
    pub affected: Option<u64>,
    /// Até o primeiro lote.
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub batch: Batch,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlWithParams {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// Falha (e desfaz tudo) se o statement não afetar exatamente 1 linha.
    pub expect_one: bool,
}

type Reply<T> = oneshot::Sender<Result<T, DbError>>;

enum Cmd {
    Execute { sql: String, reply: Reply<ExecOutcome> },
    FetchMore { reply: Reply<FetchOutcome> },
    QueryText { sql: String, params: Vec<String>, reply: Reply<Vec<Vec<Option<String>>>> },
    Transaction { stmts: Vec<SqlWithParams>, reply: Reply<u64> },
}

impl Cmd {
    fn fail(self, e: DbError) {
        let _ = match self {
            Cmd::Execute { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::FetchMore { reply } => reply.send(Err(e)).is_ok(),
            Cmd::QueryText { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::Transaction { reply, .. } => reply.send(Err(e)).is_ok(),
        };
    }
}

/// Configuração TLS com as raízes do sistema, montada uma vez e fora da thread de UI.
fn tls() -> MakeRustlsConnect {
    static TLS: OnceLock<MakeRustlsConnect> = OnceLock::new();
    TLS.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        MakeRustlsConnect::new(cfg)
    })
    .clone()
}

async fn connect_client(pg: &tokio_postgres::Config) -> Result<Client, DbError> {
    let (client, conn) = pg.connect(tls()).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("conexão encerrada: {e}");
        }
    });
    Ok(client)
}

impl Engine {
    pub fn new() -> Result<Engine, DbError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rb-db")
            .enable_all()
            .build()
            .map_err(|e| DbError::Io(e.to_string()))?;
        Ok(Engine { rt: Arc::new(rt) })
    }

    pub async fn connect(&self, cfg: &ConnectionConfig, password: Option<String>) -> Result<Session, DbError> {
        let pg = cfg.pg_config(password.as_deref());
        let handle = self.rt.handle().clone();
        self.rt
            .spawn(async move { Session::open(pg, handle).await })
            .await
            .map_err(|e| DbError::Connection(e.to_string()))?
    }
}

/// Uma conexão Postgres. Os comandos são processados em ordem por um ator no runtime do Engine.
pub struct Session {
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: Arc<Mutex<CancelToken>>,
    rt: Handle,
}

fn closed() -> DbError {
    DbError::Connection("sessão encerrada".into())
}

impl Session {
    async fn open(pg: tokio_postgres::Config, rt: Handle) -> Result<Session, DbError> {
        let started = Instant::now();
        let client = connect_client(&pg).await?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "conectado");
        let cancel = Arc::new(Mutex::new(client.cancel_token()));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Actor { client, pg, cancel: cancel.clone() }.run(rx));
        Ok(Session { tx, cancel, rt })
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(make(reply)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }

    pub async fn execute(&self, sql: String) -> Result<ExecOutcome, DbError> {
        self.call(|reply| Cmd::Execute { sql, reply }).await
    }

    pub async fn fetch_more(&self) -> Result<FetchOutcome, DbError> {
        self.call(|reply| Cmd::FetchMore { reply }).await
    }

    /// Para consultas internas cujas colunas são todas texto (use `::text` no SQL).
    pub async fn query_text(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let (sql, params) = (sql.to_string(), params.iter().map(|p| p.to_string()).collect());
        self.call(|reply| Cmd::QueryText { sql, params, reply }).await
    }

    pub async fn run_in_transaction(&self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        self.call(|reply| Cmd::Transaction { stmts, reply }).await
    }
}

struct Actor {
    client: Client,
    pg: tokio_postgres::Config,
    cancel: Arc<Mutex<CancelToken>>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut next = rx.recv().await;
        while let Some(cmd) = next.take() {
            next = match cmd {
                Cmd::Execute { sql, reply } => self.execute(sql, reply, &mut rx).await,
                Cmd::FetchMore { reply } => {
                    // Nenhum portal aberto: nada mais a buscar.
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                    rx.recv().await
                }
                Cmd::QueryText { sql, params, reply } => {
                    let _ = reply.send(self.query_text(&sql, &params).await);
                    rx.recv().await
                }
                Cmd::Transaction { stmts, reply } => {
                    let _ = reply.send(self.transaction(stmts).await);
                    rx.recv().await
                }
            };
        }
    }

    /// Executa um statement. Se ele retorna linhas, mantém o portal aberto atendendo
    /// FetchMore até esgotar ou chegar outro comando — que é devolvido para o loop principal.
    async fn execute(&mut self, sql: String, reply: Reply<ExecOutcome>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Option<Cmd> {
        let started = Instant::now();
        let stmt = match self.client.prepare(&sql).await {
            Ok(s) => s,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let columns: Arc<[ColumnInfo]> = stmt
            .columns()
            .iter()
            .map(|c| ColumnInfo { name: c.name().to_string(), type_: c.type_().clone() })
            .collect();

        if columns.is_empty() {
            let res = self.client.execute(&stmt, &[]).await.map_err(DbError::from).map(|n| ExecOutcome {
                columns,
                batch: Batch::empty(0),
                more: false,
                affected: Some(n),
                elapsed: started.elapsed(),
            });
            let _ = reply.send(res);
            return rx.recv().await;
        }

        let ncols = columns.len();
        let tx = match self.client.transaction().await {
            Ok(t) => t,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        // Em caso de erro, `tx` é descartada e o tokio-postgres faz ROLLBACK.
        let portal = match tx.bind(&stmt, &[]).await {
            Ok(p) => p,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let first = match tx.query_portal(&portal, FETCH_ROWS).await {
            Ok(r) => r,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let mut more = first.len() == FETCH_ROWS as usize;
        let elapsed = started.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, rows = first.len(), "primeiro lote");
        let _ = reply.send(Ok(ExecOutcome { columns, batch: Batch::from_rows(&first, ncols), more, affected: None, elapsed }));

        let mut pending: Option<Option<Cmd>> = None;
        while more {
            match rx.recv().await {
                Some(Cmd::FetchMore { reply }) => match tx.query_portal(&portal, FETCH_ROWS).await {
                    Ok(rows) => {
                        more = rows.len() == FETCH_ROWS as usize;
                        let _ = reply.send(Ok(FetchOutcome { batch: Batch::from_rows(&rows, ncols), more }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.into()));
                        return rx.recv().await;
                    }
                },
                other => {
                    pending = Some(other);
                    break;
                }
            }
        }
        // Autocommit: o que o statement fez (ex.: nextval, função com escrita) fica valendo.
        if let Err(e) = tx.commit().await {
            tracing::warn!("commit após leitura falhou: {e}");
        }
        match pending {
            Some(next) => next,
            None => rx.recv().await,
        }
    }

    async fn query_text(&self, sql: &str, params: &[String]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let p: Vec<&(dyn ToSql + Sync)> = params.iter().map(|s| s as &(dyn ToSql + Sync)).collect();
        let rows = self.client.query(sql, &p).await?;
        rows.iter()
            .map(|r| (0..r.len()).map(|i| r.try_get::<_, Option<String>>(i)).collect::<Result<Vec<_>, _>>())
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    async fn transaction(&mut self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        let tx = self.client.transaction().await?;
        let mut total = 0;
        for s in &stmts {
            let p: Vec<&(dyn ToSql + Sync)> = s.params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            let n = tx.execute(s.sql.as_str(), &p).await?;
            if s.expect_one && n != 1 {
                return Err(DbError::Edit(format!("esperava alterar 1 linha, alterou {n}: {}", s.sql)));
            }
            total += n;
        }
        tx.commit().await?;
        Ok(total)
    }
}
```

Os campos `cancel` e `rt` de `Session`, e `pg`/`cancel` do `Actor`, só são lidos na Tarefa 6; se o compilador avisar de campo não usado, ignore o aviso nesta tarefa.

- [ ] **Step 6: Rodar e ver passar**

Run: `cargo test -p rb-core` (com `RB_TEST_DATABASE_URL` exportada)
Expected: PASS em todos, incluindo `million_rows_page_through`.

- [ ] **Step 7: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): Engine e Session com execução paginada por portal"
```

---

### Task 6: Cancelamento, reconexão e abandono de resultado

**Files:**
- Modify: `crates/rb-core/src/session.rs`
- Create: `crates/rb-core/tests/session_lifecycle.rs`

**Interfaces:**
- Produces: `Session::cancel(&self)` (não bloqueia; envia cancel request no runtime). Reconexão automática antes de qualquer comando quando a conexão caiu.

- [ ] **Step 1: Testes (falhando)**

`crates/rb-core/tests/session_lifecycle.rs`:
```rust
mod common;

use std::time::{Duration, Instant};

use pollster::block_on;
use rb_core::DbError;

#[test]
fn cancel_interrupts_running_query() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let started = Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            s.cancel();
        });
        block_on(s.execute("select pg_sleep(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Postgres { ref code, .. }) if code == "57014"), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
}

#[test]
fn reconnects_after_backend_is_killed() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    assert!(block_on(s.execute("select pg_terminate_backend(pg_backend_pid())".into())).is_err());
    std::thread::sleep(Duration::from_millis(200)); // o client percebe o socket fechado
    let out = block_on(s.execute("select 1".into())).unwrap();
    assert_eq!(out.batch.rows(), 1);
}

#[test]
fn abandoning_a_partial_result_closes_its_transaction() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g from generate_series(1, 5000) g".into()).await.unwrap();
        assert!(out.more);
        std::thread::sleep(Duration::from_millis(300));
        // Se a transação do portal continuasse aberta, xact_start seria de ~300 ms atrás.
        let r = s
            .query_text(
                "select (clock_timestamp() - xact_start < interval '200 ms')::text from pg_stat_activity where pid = pg_backend_pid()",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r[0][0].as_deref(), Some("true"));
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (0, false));
    });
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --test session_lifecycle`
Expected: FAIL de compilação (`cancel` não existe). `abandoning_...` já deve passar quando compilar (comportamento da Tarefa 5).

- [ ] **Step 3: Implementar cancelamento e reconexão**

Em `impl Session`, adicione:
```rust
    /// Pede ao servidor para cancelar o comando em andamento desta sessão.
    pub fn cancel(&self) {
        let token = self.cancel.lock().expect("mutex do cancel token").clone();
        self.rt.spawn(async move {
            if let Err(e) = token.cancel_query(tls()).await {
                tracing::warn!("cancelamento falhou: {e}");
            }
        });
    }
```

Em `impl Actor`, adicione:
```rust
    /// Se a conexão caiu, abre outra. O comando que falhou antes não é reenviado.
    async fn ensure_connected(&mut self) -> Result<(), DbError> {
        if !self.client.is_closed() {
            return Ok(());
        }
        tracing::info!("reconectando");
        self.client = connect_client(&self.pg).await?;
        *self.cancel.lock().expect("mutex do cancel token") = self.client.cancel_token();
        Ok(())
    }
```

E no começo do corpo do `while let Some(cmd) = next.take()` em `Actor::run`:
```rust
            if let Err(e) = self.ensure_connected().await {
                cmd.fail(e);
                next = rx.recv().await;
                continue;
            }
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core`
Expected: PASS, sem avisos de campo não usado.

- [ ] **Step 5: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): cancelamento de query e reconexão automática"
```

---

### Task 7: Metadados do catálogo e cache

**Files:**
- Create: `crates/rb-core/src/metadata.rs`, `crates/rb-core/tests/metadata.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod metadata;`)

**Interfaces:**
- Consumes: `Session::query_text`
- Produces:
  - `SchemaObjects { tables, views, matviews, functions: Vec<String> }` + `contains_relation(&str) -> bool`
  - `ColumnMeta { name, type_name /* format_type */, nullable: bool, default: Option<String>, is_pk: bool, generated: bool }`
  - `IndexMeta { name, definition }`
  - `MetaRequest { Objects(String), Columns(String, String), Indexes(String, String) }` (`Hash`, `Eq`, `Clone`)
  - `MetaCache { search_path: Vec<String>, schemas: Vec<String>, objects: HashMap<String, SchemaObjects>, columns: HashMap<(String, String), Vec<ColumnMeta>>, indexes: HashMap<(String, String), Vec<IndexMeta>> }` + `has(&MetaRequest) -> bool`, `invalidate_schema(&str)`, `invalidate_table(&str, &str)`
  - `async fn schemas(&Session)`, `search_path(&Session)`, `objects(&Session, schema)`, `columns(&Session, schema, table)`, `indexes(&Session, schema, table)`

- [ ] **Step 1: Teste de integração (falhando)**

`crates/rb-core/tests/metadata.rs`:
```rust
mod common;

use pollster::block_on;
use rb_core::lexer::quote_ident;
use rb_core::metadata;

#[test]
fn catalog_queries_cover_tree_and_autocomplete() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let schema = common::unique("RbMeta");
    let q = quote_ident(&schema);
    block_on(async {
        for sql in [
            format!("create schema {q}"),
            format!(
                "create table {q}.\"Order Items\" (id int primary key, \"Name\" text not null, total numeric default 0, twice int generated always as (id * 2) stored)"
            ),
            format!("create index items_name on {q}.\"Order Items\" (\"Name\")"),
            format!("create view {q}.v_items as select id from {q}.\"Order Items\""),
            format!("create materialized view {q}.mv_items as select id from {q}.\"Order Items\""),
            format!("create function {q}.add_one(a integer) returns integer language sql as $$ select a + 1 $$"),
        ] {
            s.execute(sql).await.unwrap();
        }

        assert!(metadata::schemas(&s).await.unwrap().contains(&schema));
        assert!(metadata::search_path(&s).await.unwrap().contains(&"public".to_string()));

        let o = metadata::objects(&s, &schema).await.unwrap();
        assert_eq!(o.tables, vec!["Order Items"]);
        assert_eq!(o.views, vec!["v_items"]);
        assert_eq!(o.matviews, vec!["mv_items"]);
        assert_eq!(o.functions, vec!["add_one(a integer)"]);

        let cols = metadata::columns(&s, &schema, "Order Items").await.unwrap();
        let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "Name", "total", "twice"]);
        assert!(cols[0].is_pk && !cols[0].nullable);
        assert_eq!(cols[0].type_name, "integer");
        assert!(!cols[1].nullable && !cols[1].is_pk);
        assert_eq!(cols[2].default.as_deref(), Some("0"));
        assert!(cols[3].generated);

        let idx = metadata::indexes(&s, &schema, "Order Items").await.unwrap();
        let names: Vec<_> = idx.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Order Items_pkey", "items_name"]);

        s.execute(format!("drop schema {q} cascade")).await.unwrap();
    });
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core --test metadata`
Expected: FAIL de compilação (`rb_core::metadata` não existe).

- [ ] **Step 3: Implementar `metadata.rs`**

```rust
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
       (a.attgenerated <> '')::text \
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
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core --test metadata`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): consultas de metadados no pg_catalog e MetaCache"
```

---

### Task 8: Autocomplete

**Files:**
- Create: `crates/rb-core/src/complete.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod complete;`)

**Interfaces:**
- Consumes: `lexer::{tokenize, ident_text, quote_ident, TokKind, Token}`, `metadata::{MetaCache, MetaRequest, SchemaObjects, ColumnMeta}`
- Produces:
  - `CandidateKind { Column, Table, View, Function, Schema, Keyword }` (`Copy`, `Eq`, `Hash`)
  - `Candidate { label: String, insert: String, kind: CandidateKind, detail: Option<String> }`
  - `Completion { replace: Range<usize>, candidates: Vec<Candidate>, missing: Vec<MetaRequest> }`
  - `complete(src: &str, cursor: usize, cache: &MetaCache) -> Completion` (cursor em bytes; `replace` = trecho já digitado a ser substituído)

- [ ] **Step 1: Testes (falhando)**

```rust
use std::collections::HashSet;
use std::ops::Range;

use crate::lexer::{TokKind, Token, ident_text, quote_ident, tokenize};
use crate::metadata::{MetaCache, MetaRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidateKind {
    Column,
    Table,
    View,
    Function,
    Schema,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    pub kind: CandidateKind,
    pub detail: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub replace: Range<usize>,
    pub candidates: Vec<Candidate>,
    pub missing: Vec<MetaRequest>,
}

pub fn complete(_src: &str, _cursor: usize, _cache: &MetaCache) -> Completion {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ColumnMeta, SchemaObjects};

    fn col(name: &str, ty: &str) -> ColumnMeta {
        ColumnMeta { name: name.into(), type_name: ty.into(), nullable: true, default: None, is_pk: false, generated: false }
    }

    fn cache() -> MetaCache {
        let mut c = MetaCache::default();
        c.search_path = vec!["public".into()];
        c.schemas = vec!["public".into(), "Sales".into()];
        c.objects.insert(
            "public".into(),
            SchemaObjects {
                tables: vec!["Order Items".into(), "orders".into(), "users".into()],
                views: vec!["active_users".into()],
                matviews: vec![],
                functions: vec!["calc_total(integer)".into()],
            },
        );
        c.columns.insert(("public".into(), "users".into()), vec![col("id", "integer"), col("email", "text")]);
        c
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.candidates.iter().map(|x| x.label.as_str()).collect()
    }

    #[test]
    fn tables_after_from() {
        let src = "select * from us";
        let c = complete(src, src.len(), &cache());
        assert_eq!(c.replace, 14..16);
        assert_eq!(labels(&c), ["users", "active_users"]);
    }

    #[test]
    fn columns_via_alias() {
        let src = "select u. from users u";
        let c = complete(src, 9, &cache());
        assert_eq!(c.replace, 9..9);
        assert_eq!(labels(&c), ["id", "email"]);
        assert_eq!(c.candidates[0].detail.as_deref(), Some("integer"));
    }

    #[test]
    fn columns_via_table_name_with_prefix() {
        let src = "select users.e from users";
        assert_eq!(labels(&complete(src, 14, &cache())), ["email"]);
    }

    #[test]
    fn missing_columns_are_requested() {
        let src = "select o. from orders o";
        let c = complete(src, 9, &cache());
        assert!(c.candidates.is_empty());
        assert_eq!(c.missing, vec![MetaRequest::Columns("public".into(), "orders".into())]);
    }

    #[test]
    fn schema_qualifier_lists_objects_or_requests_them() {
        let src = "select * from \"Sales\".";
        let c = complete(src, src.len(), &cache());
        assert_eq!(c.missing, vec![MetaRequest::Objects("Sales".into())]);

        let src = "select public.calc";
        let c = complete(src, src.len(), &cache());
        assert_eq!(c.candidates[0].insert, "calc_total(");
        assert_eq!(c.candidates[0].label, "calc_total(integer)");
    }

    #[test]
    fn general_context_offers_columns_first_then_keywords() {
        let src = "select  from users";
        let c = complete(src, 7, &cache());
        assert_eq!(labels(&c)[..2], ["id", "email"]);
        assert!(labels(&c).contains(&"SELECT"));
    }

    #[test]
    fn no_completion_inside_strings_or_comments() {
        let src = "select 'abc' -- fro";
        assert!(complete(src, src.len(), &cache()).candidates.is_empty());
        assert!(complete("select 'abc'", 9, &cache()).candidates.is_empty());
    }

    #[test]
    fn uses_current_statement_only() {
        let src = "select * from users u; select u.";
        let c = complete(src, src.len(), &cache());
        assert!(c.candidates.is_empty() && c.missing.is_empty());
    }

    #[test]
    fn mixed_case_identifiers_are_quoted_on_insert() {
        let src = "select * from ord";
        let c = complete(src, src.len(), &cache());
        let item = c.candidates.iter().find(|x| x.label == "Order Items").unwrap();
        assert_eq!(item.insert, "\"Order Items\"");
    }

    #[test]
    fn table_refs_parse_schema_alias_and_lists() {
        let src = r#"select * from public.users as u, orders o join "Sales"."Deals" d on true"#;
        let toks = tokenize(src);
        let refs = table_refs(src, &toks);
        assert_eq!(
            refs,
            vec![
                TableRef { schema: Some("public".into()), table: "users".into(), alias: Some("u".into()) },
                TableRef { schema: None, table: "orders".into(), alias: Some("o".into()) },
                TableRef { schema: Some("Sales".into()), table: "Deals".into(), alias: Some("d".into()) },
            ]
        );
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core complete`
Expected: FAIL de compilação (`table_refs`, `TableRef` não existem).

- [ ] **Step 3: Implementar**

Substitua `complete` e acrescente:

```rust
const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "ILIKE", "BETWEEN",
    "EXISTS", "JOIN", "LEFT JOIN", "INNER JOIN", "ON", "GROUP BY", "ORDER BY", "HAVING", "LIMIT",
    "OFFSET", "DISTINCT", "AS", "CASE", "WHEN", "THEN", "ELSE", "END", "INSERT INTO", "VALUES",
    "UPDATE", "SET", "DELETE FROM", "RETURNING", "WITH", "UNION ALL", "CREATE TABLE", "ALTER TABLE",
    "DROP TABLE", "CREATE INDEX", "EXPLAIN ANALYZE", "ASC", "DESC", "COUNT", "COALESCE", "NOW()",
];

const TABLE_CONTEXT: &[&str] = &["from", "join", "update", "into", "table"];

const NOT_ALIAS: &[&str] = &[
    "where", "join", "inner", "left", "right", "full", "cross", "on", "using", "natural", "lateral",
    "group", "order", "limit", "offset", "set", "values", "select", "having", "window", "union",
    "except", "intersect", "returning", "fetch", "for", "as",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

pub fn complete(src: &str, cursor: usize, cache: &MetaCache) -> Completion {
    let toks = tokenize(src);
    let inside_literal = toks.iter().any(|t| {
        let literal = matches!(t.kind, TokKind::Str | TokKind::DollarStr | TokKind::Comment | TokKind::QuotedIdent);
        let line_comment_end = t.kind == TokKind::Comment && cursor == t.end && src[t.start..].starts_with("--");
        literal && ((t.start < cursor && cursor < t.end) || line_comment_end)
    });
    if inside_literal {
        return Completion { replace: cursor..cursor, ..Default::default() };
    }

    // Tokens do statement atual (entre o `;` anterior e o próximo).
    let begin = toks.iter().rposition(|t| t.kind == TokKind::Semicolon && t.end <= cursor).map_or(0, |i| i + 1);
    let end = toks[begin..].iter().position(|t| t.kind == TokKind::Semicolon).map_or(toks.len(), |i| begin + i);
    let stmt: Vec<Token> = toks[begin..end].iter().copied().filter(|t| t.kind != TokKind::Comment).collect();

    let (replace, prefix) = match stmt.iter().find(|t| t.kind == TokKind::Word && t.start < cursor && cursor <= t.end) {
        Some(t) => (t.start..cursor, src[t.start..cursor].to_lowercase()),
        None => (cursor..cursor, String::new()),
    };
    let before: Vec<&Token> = stmt.iter().filter(|t| t.end <= replace.start).collect();
    let refs = table_refs(src, &stmt);
    let mut out = Completion { replace: replace.clone(), ..Default::default() };
    let is_punct = |t: &Token, p: &str| t.kind == TokKind::Punct && &src[t.start..t.end] == p;

    if let [.., q, dot] = before.as_slice()
        && is_punct(dot, ".")
        && dot.end == replace.start
        && matches!(q.kind, TokKind::Word | TokKind::QuotedIdent)
        && q.end == dot.start
    {
        let qual = ident_text(&src[q.start..q.end]);
        if let Some(r) = refs.iter().find(|r| r.alias.as_deref() == Some(qual.as_str())) {
            push_columns(&mut out, cache, r);
        } else if cache.schemas.contains(&qual) {
            push_objects(&mut out, cache, &qual);
        } else if let Some(r) = refs.iter().find(|r| r.table == qual) {
            push_columns(&mut out, cache, r);
        }
    } else if let Some(w) = before.last().filter(|t| t.kind == TokKind::Word)
        && TABLE_CONTEXT.contains(&ident_text(&src[w.start..w.end]).as_str())
    {
        for s in &cache.search_path {
            push_objects(&mut out, cache, s);
        }
        for s in &cache.schemas {
            out.candidates.push(Candidate { label: s.clone(), insert: quote_ident(s), kind: CandidateKind::Schema, detail: None });
        }
    } else {
        for r in &refs {
            push_columns(&mut out, cache, r);
        }
        for k in KEYWORDS {
            out.candidates.push(Candidate { label: k.to_string(), insert: k.to_string(), kind: CandidateKind::Keyword, detail: None });
        }
        for s in &cache.search_path {
            push_objects(&mut out, cache, s);
        }
    }

    filter_and_rank(&mut out.candidates, &prefix);
    let mut seen = HashSet::new();
    out.missing.retain(|m| seen.insert(m.clone()));
    out
}

fn resolve_schema(cache: &MetaCache, table: &str) -> String {
    cache
        .search_path
        .iter()
        .find(|s| cache.objects.get(*s).is_some_and(|o| o.contains_relation(table)))
        .or(cache.search_path.first())
        .cloned()
        .unwrap_or_else(|| "public".into())
}

fn push_columns(out: &mut Completion, cache: &MetaCache, r: &TableRef) {
    let schema = r.schema.clone().unwrap_or_else(|| resolve_schema(cache, &r.table));
    match cache.columns.get(&(schema.clone(), r.table.clone())) {
        Some(cols) => out.candidates.extend(cols.iter().map(|c| Candidate {
            label: c.name.clone(),
            insert: quote_ident(&c.name),
            kind: CandidateKind::Column,
            detail: Some(c.type_name.clone()),
        })),
        None => out.missing.push(MetaRequest::Columns(schema, r.table.clone())),
    }
}

fn push_objects(out: &mut Completion, cache: &MetaCache, schema: &str) {
    let Some(o) = cache.objects.get(schema) else {
        out.missing.push(MetaRequest::Objects(schema.to_string()));
        return;
    };
    let rel = |name: &String, kind| Candidate { label: name.clone(), insert: quote_ident(name), kind, detail: None };
    out.candidates.extend(o.tables.iter().map(|t| rel(t, CandidateKind::Table)));
    out.candidates.extend(o.views.iter().chain(&o.matviews).map(|v| rel(v, CandidateKind::View)));
    out.candidates.extend(o.functions.iter().map(|f| {
        let name = f.split('(').next().unwrap_or(f);
        Candidate { label: f.clone(), insert: format!("{}(", quote_ident(name)), kind: CandidateKind::Function, detail: None }
    }));
}

/// Mantém só quem contém o prefixo; quem começa com ele vem antes; depois por tipo.
fn filter_and_rank(c: &mut Vec<Candidate>, prefix: &str) {
    let mut seen = HashSet::new();
    c.retain(|x| seen.insert((x.label.clone(), x.kind)));
    c.retain(|x| x.label.to_lowercase().contains(prefix));
    c.sort_by_key(|x| (!x.label.to_lowercase().starts_with(prefix), x.kind));
    c.truncate(200);
}

fn table_refs(src: &str, toks: &[Token]) -> Vec<TableRef> {
    let word = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent).then(|| ident_text(&src[t.start..t.end]));
    let punct = |t: &Token, p: &str| t.kind == TokKind::Punct && &src[t.start..t.end] == p;
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let is_ctx = toks[i].kind == TokKind::Word && matches!(word(&toks[i]).as_deref(), Some("from" | "join" | "update" | "into"));
        i += 1;
        if !is_ctx {
            continue;
        }
        loop {
            let Some(first) = toks.get(i).and_then(word) else { break };
            if NOT_ALIAS.contains(&first.as_str()) {
                break;
            }
            let mut r = TableRef { schema: None, table: first, alias: None };
            i += 1;
            if toks.get(i).is_some_and(|t| punct(t, "."))
                && let Some(name) = toks.get(i + 1).and_then(word)
            {
                r.schema = Some(std::mem::replace(&mut r.table, name));
                i += 2;
            }
            if toks.get(i).and_then(word).as_deref() == Some("as") {
                i += 1;
            }
            if let Some(a) = toks.get(i).and_then(word)
                && !NOT_ALIAS.contains(&a.as_str())
            {
                r.alias = Some(a);
                i += 1;
            }
            out.push(r);
            if toks.get(i).is_some_and(|t| punct(t, ",")) {
                i += 1;
            } else {
                break;
            }
        }
    }
    out
}
```

Nota: o filtro de `QuotedIdent` em `inside_literal` impede completar *dentro* de `"..."` (o cursor depois do `"` de fechamento continua completando).

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core complete`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): autocomplete SQL sobre o cache de metadados"
```

---

### Task 9: SQL de tabela e edições pendentes

**Files:**
- Create: `crates/rb-core/src/table.rs`, `crates/rb-core/tests/table_edit.rs`
- Modify: `crates/rb-core/src/lib.rs` (adicionar `pub mod table;`)

**Interfaces:**
- Consumes: `quote_ident`, `ColumnMeta`, `SqlWithParams`, `DbError`
- Produces:
  - `select_sql(schema: &str, table: &str, filter: Option<&str>, order: Option<(&str, bool /*asc*/)>) -> String`
  - `EditTarget { schema: String, table: String, columns: Vec<ColumnMeta> }` + `pk(&self) -> Vec<usize>`
  - `RowEdit { Update { pk: Vec<Option<String>>, changes: Vec<(usize, Option<String>)> }, Insert { values: Vec<(usize, Option<String>)> }, Delete { pk: Vec<Option<String>> } }`
  - `build_sql(&EditTarget, &[RowEdit]) -> Result<Vec<SqlWithParams>, DbError>`
  - `PendingEdits` (`Default`): `is_empty()`, `inserted_rows()`, `set(row, col, Option<String>, loaded)`, `get(row, col, loaded) -> Option<&Option<String>>`, `add_row()`, `toggle_delete(row, loaded)`, `is_deleted(row)`, `to_row_edits(&EditTarget, original: impl Fn(usize, usize) -> Option<String>) -> Vec<RowEdit>`. Linhas `>= loaded` são as inseridas, na ordem.

- [ ] **Step 1: Testes unitários (falhando)**

```rust
use std::collections::{BTreeMap, BTreeSet};

use crate::DbError;
use crate::lexer::quote_ident;
use crate::metadata::ColumnMeta;
use crate::session::SqlWithParams;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEdit {
    Update { pk: Vec<Option<String>>, changes: Vec<(usize, Option<String>)> },
    Insert { values: Vec<(usize, Option<String>)> },
    Delete { pk: Vec<Option<String>> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingEdits {
    changed: BTreeMap<usize, BTreeMap<usize, Option<String>>>,
    deleted: BTreeSet<usize>,
    inserted: Vec<BTreeMap<usize, Option<String>>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool, generated: bool) -> ColumnMeta {
        ColumnMeta { name: name.into(), type_name: ty.into(), nullable: !pk, default: None, is_pk: pk, generated }
    }

    fn target() -> EditTarget {
        EditTarget {
            schema: "Sales".into(),
            table: "Order Items".into(),
            columns: vec![
                col("id", "integer", true, false),
                col("Name", "text", false, false),
                col("twice", "integer", false, true),
                col("mood", "\"Sales\".mood", false, false),
            ],
        }
    }

    fn s(v: &str) -> Option<String> {
        Some(v.into())
    }

    #[test]
    fn select_sql_quotes_identifiers() {
        assert_eq!(select_sql("Sales", "Order Items", None, None), r#"SELECT * FROM "Sales"."Order Items""#);
        assert_eq!(
            select_sql("public", "t", Some(" a > 1 -- x "), Some(("Name", false))),
            "SELECT * FROM public.t WHERE (a > 1 -- x\n) ORDER BY \"Name\" DESC"
        );
        assert_eq!(select_sql("public", "t", Some("   "), Some(("id", true))), "SELECT * FROM public.t ORDER BY id ASC");
    }

    #[test]
    fn update_quotes_mixed_case_identifiers() {
        let sql = build_sql(&target(), &[RowEdit::Update { pk: vec![s("7")], changes: vec![(1, s("Ana")), (3, None)] }]).unwrap();
        assert_eq!(
            sql,
            vec![SqlWithParams {
                sql: r#"UPDATE "Sales"."Order Items" SET "Name" = ($1::text)::text, mood = ($2::text)::"Sales".mood WHERE id = ($3::text)::integer"#.into(),
                params: vec![s("Ana"), None, s("7")],
                expect_one: true,
            }]
        );
    }

    #[test]
    fn insert_and_delete() {
        let sql = build_sql(
            &target(),
            &[RowEdit::Insert { values: vec![(0, s("1")), (1, s("x"))] }, RowEdit::Insert { values: vec![] }, RowEdit::Delete { pk: vec![s("9")] }],
        )
        .unwrap();
        assert_eq!(sql[0].sql, r#"INSERT INTO "Sales"."Order Items" (id, "Name") VALUES (($1::text)::integer, ($2::text)::text)"#);
        assert_eq!(sql[1].sql, r#"INSERT INTO "Sales"."Order Items" DEFAULT VALUES"#);
        assert_eq!(sql[2].sql, r#"DELETE FROM "Sales"."Order Items" WHERE id = ($1::text)::integer"#);
        assert_eq!(sql[2].params, vec![s("9")]);
    }

    #[test]
    fn composite_primary_key() {
        let mut t = target();
        t.columns[1].is_pk = true;
        let sql = build_sql(&t, &[RowEdit::Delete { pk: vec![s("1"), s("a")] }]).unwrap();
        assert_eq!(sql[0].sql, r#"DELETE FROM "Sales"."Order Items" WHERE id = ($1::text)::integer AND "Name" = ($2::text)::text"#);
    }

    #[test]
    fn refuses_tables_without_pk_and_generated_columns() {
        let mut t = target();
        t.columns[0].is_pk = false;
        assert!(matches!(build_sql(&t, &[RowEdit::Delete { pk: vec![] }]), Err(DbError::Edit(_))));
        let err = build_sql(&target(), &[RowEdit::Update { pk: vec![s("1")], changes: vec![(2, s("5"))] }]);
        assert!(matches!(err, Err(DbError::Edit(m)) if m.contains("twice")));
    }

    #[test]
    fn pending_edits_produce_deletes_updates_inserts() {
        let mut p = PendingEdits::default();
        assert!(p.is_empty());
        p.set(0, 1, s("novo"), 2);
        p.set(1, 1, s("vai sumir"), 2);
        p.toggle_delete(1, 2);
        p.add_row();
        p.set(2, 0, s("10"), 2); // linha 2 = primeira inserida
        p.add_row();
        p.toggle_delete(3, 2); // remove a segunda inserida
        assert_eq!(p.inserted_rows(), 1);
        assert_eq!(p.get(0, 1, 2), Some(&s("novo")));
        assert!(p.is_deleted(1));

        let original = |row: usize, _col: usize| s(&format!("pk{row}"));
        assert_eq!(
            p.to_row_edits(&target(), original),
            vec![
                RowEdit::Delete { pk: vec![s("pk1")] },
                RowEdit::Update { pk: vec![s("pk0")], changes: vec![(1, s("novo"))] },
                RowEdit::Insert { values: vec![(0, s("10"))] },
            ]
        );
        p.toggle_delete(1, 2);
        assert!(!p.is_deleted(1));
    }
}
```

- [ ] **Step 2: Rodar e ver falhar**

Run: `cargo test -p rb-core table`
Expected: FAIL de compilação (`select_sql`, `build_sql`, métodos de `PendingEdits` não existem).

- [ ] **Step 3: Implementar**

Acrescente antes dos testes:

```rust
pub fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(table))
}

/// SELECT da aba de tabela. O filtro é uma expressão WHERE crua digitada pelo usuário;
/// vai entre parênteses e com quebra de linha para que um `--` não engula o ORDER BY.
pub fn select_sql(schema: &str, table: &str, filter: Option<&str>, order: Option<(&str, bool)>) -> String {
    let mut sql = format!("SELECT * FROM {}", qualified(schema, table));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql += &format!(" WHERE ({f}\n)");
    }
    if let Some((col, asc)) = order {
        sql += &format!(" ORDER BY {} {}", quote_ident(col), if asc { "ASC" } else { "DESC" });
    }
    sql
}

impl EditTarget {
    pub fn pk(&self) -> Vec<usize> {
        self.columns.iter().enumerate().filter(|(_, c)| c.is_pk).map(|(i, _)| i).collect()
    }
}

fn bind(params: &mut Vec<Option<String>>, value: &Option<String>, type_name: &str) -> String {
    params.push(value.clone());
    format!("(${}::text)::{type_name}", params.len())
}

fn where_pk(t: &EditTarget, pk_cols: &[usize], pk: &[Option<String>], params: &mut Vec<Option<String>>) -> Result<String, DbError> {
    if pk.len() != pk_cols.len() {
        return Err(DbError::Edit("valores de chave primária incompletos".into()));
    }
    Ok(pk_cols
        .iter()
        .zip(pk)
        .map(|(&i, v)| {
            let c = &t.columns[i];
            format!("{} = {}", quote_ident(&c.name), bind(params, v, &c.type_name))
        })
        .collect::<Vec<_>>()
        .join(" AND "))
}

fn writable(t: &EditTarget, cols: &[(usize, Option<String>)]) -> Result<(), DbError> {
    match cols.iter().find(|(i, _)| t.columns[*i].generated) {
        Some((i, _)) => Err(DbError::Edit(format!("a coluna {} é gerada e não pode ser editada", t.columns[*i].name))),
        None => Ok(()),
    }
}

/// Gera os statements parametrizados. Valores vão como texto e o Postgres converte
/// com `($n::text)::<tipo>`, o que cobre enum, json, array, uuid etc.
pub fn build_sql(t: &EditTarget, edits: &[RowEdit]) -> Result<Vec<SqlWithParams>, DbError> {
    let pk_cols = t.pk();
    if pk_cols.is_empty() {
        return Err(DbError::Edit("a tabela não tem chave primária; edição desativada".into()));
    }
    let table = qualified(&t.schema, &t.table);
    let mut out = Vec::new();
    for e in edits {
        let mut params = Vec::new();
        let sql = match e {
            RowEdit::Update { pk, changes } => {
                writable(t, changes)?;
                if changes.is_empty() {
                    continue;
                }
                let set = changes
                    .iter()
                    .map(|(i, v)| {
                        let c = &t.columns[*i];
                        format!("{} = {}", quote_ident(&c.name), bind(&mut params, v, &c.type_name))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("UPDATE {table} SET {set} WHERE {}", where_pk(t, &pk_cols, pk, &mut params)?)
            }
            RowEdit::Insert { values } => {
                writable(t, values)?;
                if values.is_empty() {
                    format!("INSERT INTO {table} DEFAULT VALUES")
                } else {
                    let cols = values.iter().map(|(i, _)| quote_ident(&t.columns[*i].name)).collect::<Vec<_>>().join(", ");
                    let vals = values
                        .iter()
                        .map(|(i, v)| bind(&mut params, v, &t.columns[*i].type_name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("INSERT INTO {table} ({cols}) VALUES ({vals})")
                }
            }
            RowEdit::Delete { pk } => format!("DELETE FROM {table} WHERE {}", where_pk(t, &pk_cols, pk, &mut params)?),
        };
        out.push(SqlWithParams { sql, params, expect_one: true });
    }
    Ok(out)
}

impl PendingEdits {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    pub fn inserted_rows(&self) -> usize {
        self.inserted.len()
    }

    /// `loaded` = linhas vindas do banco; índices a partir dele são linhas inseridas.
    pub fn set(&mut self, row: usize, col: usize, value: Option<String>, loaded: usize) {
        if row >= loaded {
            if let Some(r) = self.inserted.get_mut(row - loaded) {
                r.insert(col, value);
            }
        } else {
            self.changed.entry(row).or_default().insert(col, value);
        }
    }

    pub fn get(&self, row: usize, col: usize, loaded: usize) -> Option<&Option<String>> {
        if row >= loaded { self.inserted.get(row - loaded)?.get(&col) } else { self.changed.get(&row)?.get(&col) }
    }

    pub fn add_row(&mut self) {
        self.inserted.push(BTreeMap::new());
    }

    /// Marca/desmarca exclusão; numa linha inserida, simplesmente a remove.
    pub fn toggle_delete(&mut self, row: usize, loaded: usize) {
        if row >= loaded {
            if row - loaded < self.inserted.len() {
                self.inserted.remove(row - loaded);
            }
        } else if !self.deleted.remove(&row) {
            self.deleted.insert(row);
        }
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deleted.contains(&row)
    }

    /// Ordem: exclusões, alterações, inserções. A PK vem do valor original (`original(row, col)`).
    pub fn to_row_edits(&self, target: &EditTarget, original: impl Fn(usize, usize) -> Option<String>) -> Vec<RowEdit> {
        let pk = target.pk();
        let pk_of = |row: usize| pk.iter().map(|&c| original(row, c)).collect::<Vec<_>>();
        let mut out: Vec<RowEdit> = self.deleted.iter().map(|&r| RowEdit::Delete { pk: pk_of(r) }).collect();
        out.extend(self.changed.iter().filter(|(r, _)| !self.deleted.contains(r)).map(|(&r, cols)| RowEdit::Update {
            pk: pk_of(r),
            changes: cols.iter().map(|(&c, v)| (c, v.clone())).collect(),
        }));
        out.extend(self.inserted.iter().map(|vals| RowEdit::Insert { values: vals.iter().map(|(&c, v)| (c, v.clone())).collect() }));
        out
    }
}
```

- [ ] **Step 4: Rodar e ver passar**

Run: `cargo test -p rb-core table`
Expected: PASS

- [ ] **Step 5: Teste de integração (ida e volta no Postgres)**

`crates/rb-core/tests/table_edit.rs`:
```rust
mod common;

use pollster::block_on;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::lexer::quote_ident;
use rb_core::metadata;
use rb_core::table::{EditTarget, PendingEdits, RowEdit, build_sql, select_sql};

#[test]
fn pending_edits_round_trip_through_postgres() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let schema = common::unique("RbEdit");
    let q = quote_ident(&schema);
    block_on(async {
        for sql in [
            format!("create schema {q}"),
            format!("create type {q}.mood as enum ('ok', 'sad')"),
            format!("create table {q}.\"Order Items\" (id int primary key, \"Name\" text, tags text[], mood {q}.mood)"),
        ] {
            s.execute(sql).await.unwrap();
        }
        let target = EditTarget {
            schema: schema.clone(),
            table: "Order Items".into(),
            columns: metadata::columns(&s, &schema, "Order Items").await.unwrap(),
        };
        let read = async || {
            let out = s.execute(select_sql(&schema, "Order Items", None, Some(("id", true)))).await.unwrap();
            ResultSet::new(out.columns, out.batch, out.more)
        };

        // inserir
        let mut p = PendingEdits::default();
        p.add_row();
        for (col, v) in [(0, "1"), (1, "Ana"), (2, "{a,\"b c\"}"), (3, "sad")] {
            p.set(0, col, Some(v.into()), 0);
        }
        let n = s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |_, _| None)).unwrap()).await.unwrap();
        assert_eq!(n, 1);
        let rs = read().await;
        assert_eq!(rs.text(0, 1).as_deref(), Some("Ana"));
        assert_eq!(rs.text(0, 2).as_deref(), Some("{a,\"b c\"}"));
        assert_eq!(rs.text(0, 3).as_deref(), Some("sad"));

        // alterar para NULL usando o texto decodificado como PK
        let mut p = PendingEdits::default();
        p.set(0, 1, None, rs.rows());
        s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |r, c| rs.text(r, c))).unwrap()).await.unwrap();
        assert_eq!(read().await.text(0, 1), None);

        // linha inexistente: erro e nada muda
        let ghost = [RowEdit::Update { pk: vec![Some("999".into())], changes: vec![(1, Some("x".into()))] }];
        assert!(matches!(s.run_in_transaction(build_sql(&target, &ghost).unwrap()).await, Err(DbError::Edit(_))));

        // excluir
        let mut p = PendingEdits::default();
        let rs = read().await;
        p.toggle_delete(0, rs.rows());
        s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |r, c| rs.text(r, c))).unwrap()).await.unwrap();
        assert_eq!(read().await.rows(), 0);

        s.execute(format!("drop schema {q} cascade")).await.unwrap();
    });
}
```

- [ ] **Step 6: Rodar e ver passar**

Run: `cargo test -p rb-core`
Expected: PASS em todos.

- [ ] **Step 7: Commit**

```bash
git add crates/rb-core
git commit -m "feat(core): SQL de tabela, edições pendentes e gravação parametrizada"
```

---

## Tarefas de UI (10–16)

O `rb-app` não tem testes automatizados (spec §6). Cada tarefa termina com `cargo build -p rb-app` limpo, uma verificação manual com o Postgres de teste e commit. Antes de começar, leia `docs/superpowers/plans/gpui-api-notes.md`. O código abaixo segue a API lida do código-fonte; se um nome não compilar, procure o equivalente no fonte do crate (`~/.cargo/registry/src/*/gpui-component-0.7.3/`) e corrija a nota.

---

### Task 10: App GPUI, ações e janela

**Files:**
- Create: `crates/rb-app/Cargo.toml`, `crates/rb-app/src/main.rs`, `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Produces:
  - Ações (`crate::`): `RunStatement, RunScript, CancelQuery, SaveEdits, NewSqlTab, CloseTab, RefreshNode, CopyCells, Quit`
  - `crate::Db(pub Engine)` global GPUI
  - `workspace::Workspace` (view raiz) com `new(window, cx)`

- [ ] **Step 1: Crate**

`crates/rb-app/Cargo.toml`:
```toml
[package]
name = "rb-app"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "rustbeaver"
path = "src/main.rs"

[dependencies]
anyhow = "1"
gpui-kit = { version = "=0.7.3", features = ["tree-sitter-sql"] }
lsp-types = "0.97"
rb-core = { path = "../rb-core" }
tracing = "0.1.44"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 2: `main.rs`**

```rust
mod workspace;

use std::time::Instant;

use gpui_kit::*;
use rb_core::session::Engine;

gpui_kit::actions!(rustbeaver, [RunStatement, RunScript, CancelQuery, SaveEdits, NewSqlTab, CloseTab, RefreshNode, CopyCells, Quit]);

/// Runtime de banco compartilhado pela aplicação.
pub struct Db(pub Engine);

impl Global for Db {}

fn main() {
    let started = Instant::now();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let engine = Engine::new().expect("criar runtime de banco");

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.set_global(Db(engine));
        cx.bind_keys([
            KeyBinding::new("cmd-enter", RunStatement, Some("SqlTab > Input")),
            KeyBinding::new("cmd-shift-enter", RunScript, Some("SqlTab > Input")),
            KeyBinding::new("cmd-.", CancelQuery, Some("SqlTab")),
            KeyBinding::new("cmd-.", CancelQuery, Some("TableTab")),
            KeyBinding::new("cmd-s", SaveEdits, Some("TableTab")),
            KeyBinding::new("cmd-t", NewSqlTab, Some("Workspace")),
            KeyBinding::new("cmd-w", CloseTab, Some("Workspace")),
            KeyBinding::new("f5", RefreshNode, Some("SchemaTree")),
            KeyBinding::new("cmd-c", CopyCells, Some("ResultGrid")),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| workspace::Workspace::new(window, cx)))
            .expect("abrir janela");
        cx.activate(true);
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "janela aberta");
    });
}
```

- [ ] **Step 3: `workspace.rs` mínimo**

```rust
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

use crate::{CloseTab, NewSqlTab};

pub struct Workspace {
    focus: FocusHandle,
}

impl Workspace {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Workspace { focus: cx.focus_handle() }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|_, _: &NewSqlTab, _, _| {}))
            .on_action(cx.listener(|_, _: &CloseTab, _, _| {}))
            .child(v_flex().w(px(260.)).h_full().border_r_1().border_color(cx.theme().border).p_2().child("Conexões"))
            .child(v_flex().flex_1().h_full().items_center().justify_center().child("Nenhuma conexão aberta"))
    }
}
```

- [ ] **Step 4: Compilar e rodar**

Run: `cargo build -p rb-app` e depois `RUST_LOG=info cargo run --release -p rb-app`
Expected: janela abre com barra lateral "Conexões"; o log mostra `janela aberta elapsed_ms=...`; `cmd-q` fecha. Anote o tempo (meta < 300 ms; se passar disso, registre para a Tarefa 17, não otimize agora).

- [ ] **Step 5: Commit**

```bash
git add crates/rb-app Cargo.lock
git commit -m "feat(app): janela GPUI, ações e atalhos"
```

---

### Task 11: Conexões e MetaStore

**Files:**
- Create: `crates/rb-app/src/conn_dialog.rs`, `crates/rb-app/src/meta.rs`
- Modify: `crates/rb-app/src/main.rs` (adicionar `mod conn_dialog; mod meta;`), `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `config::{load, save, connections_path, new_id, ConnectionConfig, SslMode, ConfigFile}`, `secrets::{get_password, set_password}`, `Engine::connect`, `metadata::*`
- Produces:
  - `meta::ConnTarget { cfg: ConnectionConfig, password: Option<String> }` (`Clone`)
  - `meta::MetaStore { pub session: Arc<Session>, pub cache: MetaCache, pub error: Option<String> }` (entidade GPUI) com `new(session, cx)`, `request(&mut self, MetaRequest, cx)`, `reload_roots(cx)`
  - `Workspace.connections: Vec<ConnectionConfig>`, `Workspace.active: Option<Active>` onde `Active { conn: ConnTarget, meta: Entity<MetaStore> }`
  - `conn_dialog::open(window, cx: &mut Context<Workspace>)`

- [ ] **Step 1: `meta.rs`**

```rust
use std::collections::HashSet;
use std::sync::Arc;

use gpui_kit::*;
use rb_core::config::ConnectionConfig;
use rb_core::metadata::{self, ColumnMeta, IndexMeta, MetaCache, MetaRequest, SchemaObjects};
use rb_core::session::Session;

#[derive(Clone)]
pub struct ConnTarget {
    pub cfg: ConnectionConfig,
    pub password: Option<String>,
}

/// Sessão dedicada a metadados + cache compartilhado por árvore e autocomplete.
pub struct MetaStore {
    pub session: Arc<Session>,
    pub cache: MetaCache,
    pub error: Option<String>,
    inflight: HashSet<MetaRequest>,
}

enum Loaded {
    Objects(SchemaObjects),
    Columns(Vec<ColumnMeta>),
    Indexes(Vec<IndexMeta>),
}

impl MetaStore {
    pub fn new(session: Arc<Session>, cx: &mut Context<Self>) -> Self {
        let mut this = MetaStore { session, cache: MetaCache::default(), error: None, inflight: HashSet::new() };
        this.reload_roots(cx);
        this
    }

    /// Recarrega schemas e search_path (raiz da árvore).
    pub fn reload_roots(&mut self, cx: &mut Context<Self>) {
        let s = self.session.clone();
        cx.spawn(async move |this, cx| {
            let res = async { Ok::<_, rb_core::DbError>((metadata::schemas(&s).await?, metadata::search_path(&s).await?)) }.await;
            this.update(cx, |m, cx| {
                match res {
                    Ok((schemas, path)) => {
                        m.cache.schemas = schemas;
                        m.cache.search_path = path;
                    }
                    Err(e) => m.error = Some(e.to_string()),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Carrega um pedaço do catálogo se ainda não estiver em cache nem a caminho.
    pub fn request(&mut self, req: MetaRequest, cx: &mut Context<Self>) {
        if self.cache.has(&req) || !self.inflight.insert(req.clone()) {
            return;
        }
        let s = self.session.clone();
        cx.spawn(async move |this, cx| {
            let res = match &req {
                MetaRequest::Objects(schema) => metadata::objects(&s, schema).await.map(Loaded::Objects),
                MetaRequest::Columns(schema, t) => metadata::columns(&s, schema, t).await.map(Loaded::Columns),
                MetaRequest::Indexes(schema, t) => metadata::indexes(&s, schema, t).await.map(Loaded::Indexes),
            };
            this.update(cx, |m, cx| {
                m.inflight.remove(&req);
                match (res, req) {
                    (Ok(Loaded::Objects(o)), MetaRequest::Objects(schema)) => {
                        m.cache.objects.insert(schema, o);
                    }
                    (Ok(Loaded::Columns(c)), MetaRequest::Columns(schema, t)) => {
                        m.cache.columns.insert((schema, t), c);
                    }
                    (Ok(Loaded::Indexes(i)), MetaRequest::Indexes(schema, t)) => {
                        m.cache.indexes.insert((schema, t), i);
                    }
                    (Err(e), _) => m.error = Some(e.to_string()),
                    _ => unreachable!("resposta sempre corresponde ao pedido"),
                }
                cx.notify();
            })
        })
        .detach();
    }
}
```

- [ ] **Step 2: `conn_dialog.rs`**

```rust
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::h_flex;
use gpui_kit::*;
use rb_core::config::{ConnectionConfig, SslMode, new_id};

use crate::workspace::Workspace;

struct Fields {
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    ssl: Entity<InputState>,
}

impl Fields {
    fn read(&self, cx: &App) -> Result<(ConnectionConfig, String), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let port = v(&self.port).parse::<u16>().map_err(|_| "porta inválida".to_string())?;
        let ssl = match v(&self.ssl).as_str() {
            "" | "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            other => return Err(format!("ssl deve ser disable, prefer ou require (não {other})")),
        };
        let cfg = ConnectionConfig {
            id: new_id(),
            name: v(&self.name),
            host: v(&self.host),
            port,
            database: v(&self.database),
            user: v(&self.user),
            ssl,
        };
        if cfg.name.is_empty() || cfg.host.is_empty() || cfg.database.is_empty() || cfg.user.is_empty() {
            return Err("preencha nome, host, banco e usuário".into());
        }
        Ok((cfg, self.password.read(cx).value().to_string()))
    }
}

pub fn open(window: &mut Window, cx: &mut Context<Workspace>) {
    let input = |window: &mut Window, cx: &mut Context<Workspace>, value: &str| {
        let value = value.to_string();
        cx.new(|cx| InputState::new(window, cx).default_value(value))
    };
    let f = std::rc::Rc::new(Fields {
        name: input(window, cx, "local"),
        host: input(window, cx, "localhost"),
        port: input(window, cx, "5432"),
        database: input(window, cx, "postgres"),
        user: input(window, cx, "postgres"),
        password: cx.new(|cx| InputState::new(window, cx).masked(true)),
        ssl: input(window, cx, "disable"),
    });
    let ws = cx.entity().downgrade();

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let (f, ws) = (f.clone(), ws.clone());
        dialog
            .title("Nova conexão")
            .w(px(460.))
            .child(
                v_form()
                    .child(field().label("Nome").child(Input::new(&f.name)))
                    .child(field().label("Host").child(Input::new(&f.host)))
                    .child(field().label("Porta").child(Input::new(&f.port)))
                    .child(field().label("Banco").child(Input::new(&f.database)))
                    .child(field().label("Usuário").child(Input::new(&f.user)))
                    .child(field().label("Senha (vai para o Keychain)").child(Input::new(&f.password).mask_toggle()))
                    .child(field().label("SSL (disable | prefer | require)").child(Input::new(&f.ssl))),
            )
            .footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("conn-cancel").label("Cancelar").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("conn-save").label("Salvar").primary().on_click(move |_, window, cx| {
                        match f.read(cx) {
                            Ok((cfg, password)) => {
                                ws.update(cx, |ws, cx| ws.add_connection(cfg, password, window, cx)).ok();
                                window.close_dialog(cx);
                            }
                            Err(msg) => window.push_notification(Notification::error(msg), cx),
                        }
                    })),
            )
    });
}
```

- [ ] **Step 3: Conexões no `Workspace`**

Substitua `workspace.rs` por:

```rust
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;
use rb_core::config::{self, ConfigFile, ConnectionConfig};
use rb_core::secrets;

use crate::meta::{ConnTarget, MetaStore};
use crate::{CloseTab, Db, NewSqlTab, conn_dialog};

pub struct Active {
    pub conn: ConnTarget,
    pub meta: Entity<MetaStore>,
}

pub struct Workspace {
    focus: FocusHandle,
    pub connections: Vec<ConnectionConfig>,
    pub active: Option<Active>,
    connecting: bool,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = match config::load(&config::connections_path()) {
            Ok(f) => f.connections,
            Err(e) => {
                window.push_notification(Notification::error(e.to_string()), cx);
                Vec::new()
            }
        };
        Workspace { focus: cx.focus_handle(), connections, active: None, connecting: false }
    }

    pub fn add_connection(&mut self, cfg: ConnectionConfig, password: String, window: &mut Window, cx: &mut Context<Self>) {
        self.connections.push(cfg.clone());
        let file = ConfigFile { connections: self.connections.clone() };
        let id = cfg.id.clone();
        cx.spawn_in(window, async move |_, cx| {
            let res = cx
                .background_spawn(async move {
                    config::save(&config::connections_path(), &file)?;
                    if !password.is_empty() {
                        secrets::set_password(&id, &password)?;
                    }
                    Ok::<_, rb_core::DbError>(())
                })
                .await;
            if let Err(e) = res {
                cx.update(|window, cx| window.push_notification(Notification::error(e.to_string()), cx))?;
            }
            anyhow::Ok(())
        })
        .detach();
        cx.notify();
    }

    fn connect(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting {
            return;
        }
        self.connecting = true;
        cx.notify();
        let cfg = self.connections[ix].clone();
        let engine = cx.global::<Db>().0.clone();
        cx.spawn_in(window, async move |this, cx| {
            let id = cfg.id.clone();
            let result = async {
                let password = cx.background_spawn(async move { secrets::get_password(&id) }).await?;
                let session = engine.connect(&cfg, password.clone()).await?;
                Ok::<_, rb_core::DbError>((ConnTarget { cfg, password }, session))
            }
            .await;
            this.update_in(cx, |ws, window, cx| {
                ws.connecting = false;
                match result {
                    Ok((conn, session)) => {
                        let meta = cx.new(|cx| MetaStore::new(Arc::new(session), cx));
                        ws.active = Some(Active { conn, meta });
                        ws.on_connected(window, cx);
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Falha ao conectar: {e}")), cx),
                }
                cx.notify();
            })
        })
        .detach();
    }

    /// Ponto de extensão: Tarefas 12 e 14 criam a árvore e as abas aqui.
    fn on_connected(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_1();
        for (ix, c) in self.connections.iter().enumerate() {
            let active = self.active.as_ref().is_some_and(|a| a.conn.cfg.id == c.id);
            let mut b = Button::new(("conn", ix)).label(c.name.clone()).ghost();
            if active {
                b = b.primary();
            }
            list = list.child(b.on_click(cx.listener(move |ws, _, window, cx| ws.connect(ix, window, cx))));
        }
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .child("Conexões")
                    .child(Button::new("new-conn").label("+").ghost().on_click(cx.listener(|_, _, window, cx| conn_dialog::open(window, cx)))),
            )
            .child(list)
            .when(self.connecting, |d| d.child("conectando…"))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.active {
            Some(a) => format!("{} · {} schemas", a.conn.cfg.name, a.meta.read(cx).cache.schemas.len()),
            None => "Nenhuma conexão aberta".into(),
        };
        h_flex()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|_, _: &NewSqlTab, _, _| {}))
            .on_action(cx.listener(|_, _: &CloseTab, _, _| {}))
            .child(div().w(px(260.)).h_full().border_r_1().border_color(cx.theme().border).child(self.render_sidebar(cx)))
            .child(v_flex().flex_1().h_full().items_center().justify_center().child(status))
    }
}
```

`.when(..)` vem de `gpui_kit::prelude::FluentBuilder` (incluído em `gpui_kit::*`; se não, adicione `use gpui_kit::prelude::*;`).

O render do `Workspace` lê `a.meta.read(cx)`, mas não observa o `MetaStore`. Para o status atualizar quando os schemas chegarem, no `connect`, logo depois de criar `meta`, adicione `cx.observe(&meta, |_, _, cx| cx.notify()).detach();`.

- [ ] **Step 4: Compilar e verificar manualmente**

Run: `cargo run -p rb-app`
Expected:
1. "+" abre o diálogo. Preencha host `localhost`, porta `55432`, banco `postgres`, usuário `postgres`, senha `rb`, e salve. O macOS pode pedir acesso ao Keychain: aceite.
2. A conexão aparece na lista e `~/Library/Application Support/rustbeaver/connections.toml` existe **sem** a senha.
3. Clique nela: o centro mostra `local · N schemas`.
4. Porta `abc` mostra a notificação "porta inválida".
5. Feche e reabra o app: a conexão continua listada.

- [ ] **Step 5: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): conexões salvas, Keychain e MetaStore"
```

---

### Task 12: Árvore de schema

**Files:**
- Create: `crates/rb-app/src/schema_tree.rs`
- Modify: `crates/rb-app/src/main.rs` (`mod schema_tree;`), `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `MetaStore::{request, cache}`, `MetaCache::{invalidate_schema, invalidate_table}`
- Produces:
  - `schema_tree::SchemaTree::new(meta: Entity<MetaStore>, on_open_table: Rc<dyn Fn(String, String, &mut Window, &mut App)>, cx: &mut Context<Self>) -> Self`
  - Ids dos nós: partes unidas por `\u{1f}`: `s␟schema`, `g␟schema␟tables|views|matviews|functions`, `r␟schema␟relação`, `cg␟schema␟relação`, `ig␟schema␟relação`, `c␟…`, `i␟…`, `f␟…`, `…␟loading`
  - `Workspace::open_table(schema, table, window, cx)` (corpo vazio até a Tarefa 16)

- [ ] **Step 1: `schema_tree.rs`**

```rust
use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::list::ListItem;
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState, tree};
use gpui_kit::*;
use rb_core::metadata::{MetaCache, MetaRequest};

use crate::RefreshNode;
use crate::meta::MetaStore;

const SEP: &str = "\u{1f}";

fn id(parts: &[&str]) -> SharedString {
    parts.join(SEP).into()
}

fn loading(parent: &SharedString) -> TreeItem {
    TreeItem::new(format!("{parent}{SEP}loading"), "carregando…")
}

type OpenTable = Rc<dyn Fn(String, String, &mut Window, &mut App)>;

pub struct SchemaTree {
    meta: Entity<MetaStore>,
    state: Entity<TreeState>,
    expanded: HashSet<SharedString>,
    on_open_table: OpenTable,
    _subs: Vec<Subscription>,
}

impl SchemaTree {
    pub fn new(meta: Entity<MetaStore>, on_open_table: OpenTable, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| TreeState::new(cx));
        let subs = vec![
            cx.observe(&meta, |this, _, cx| this.rebuild(cx)),
            cx.subscribe(&state, |this, _, ev: &TreeEvent, cx| this.on_tree_event(ev, cx)),
        ];
        let mut this = SchemaTree { meta, state, expanded: HashSet::new(), on_open_table, _subs: subs };
        this.rebuild(cx);
        this
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let items = build_items(&self.meta.read(cx).cache, &self.expanded);
        // ponytail: set_items zera a seleção; restaurar quando incomodar.
        self.state.update(cx, |st, cx| st.set_items(items, cx));
        cx.notify();
    }

    fn on_tree_event(&mut self, ev: &TreeEvent, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Expanded(node) => {
                self.expanded.insert(node.clone());
                let parts: Vec<&str> = node.split(SEP).collect();
                self.meta.update(cx, |m, cx| match parts.as_slice() {
                    ["s", schema] => m.request(MetaRequest::Objects(schema.to_string()), cx),
                    ["r", schema, rel] => {
                        m.request(MetaRequest::Columns(schema.to_string(), rel.to_string()), cx);
                        m.request(MetaRequest::Indexes(schema.to_string(), rel.to_string()), cx);
                    }
                    _ => {}
                });
            }
            TreeEvent::Collapsed(node) => {
                self.expanded.remove(node);
            }
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(node) = self.state.read(cx).selected_item().map(|i| i.id.clone()) else {
            self.meta.update(cx, |m, cx| m.reload_roots(cx));
            return;
        };
        let parts: Vec<String> = node.split(SEP).map(String::from).collect();
        self.meta.update(cx, |m, cx| match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["s", schema, ..] | ["g", schema, ..] | ["f", schema, ..] => {
                m.cache.invalidate_schema(schema);
                m.request(MetaRequest::Objects(schema.to_string()), cx);
            }
            [_, schema, rel, ..] => {
                m.cache.invalidate_table(schema, rel);
                m.request(MetaRequest::Columns(schema.to_string(), rel.to_string()), cx);
                m.request(MetaRequest::Indexes(schema.to_string(), rel.to_string()), cx);
            }
            _ => m.reload_roots(cx),
        });
    }
}

fn build_items(c: &MetaCache, expanded: &HashSet<SharedString>) -> Vec<TreeItem> {
    let open = |id: &SharedString| expanded.contains(id);
    c.schemas
        .iter()
        .map(|s| {
            let sid = id(&["s", s]);
            let children = match c.objects.get(s) {
                None => vec![loading(&sid)],
                Some(o) => {
                    let rel_group = |key: &str, label: &str, names: &[String]| {
                        let gid = id(&["g", s, key]);
                        let rels = names.iter().map(|n| relation(c, s, n, expanded)).collect::<Vec<_>>();
                        TreeItem::new(gid.clone(), format!("{label} ({})", names.len())).expanded(open(&gid)).children(rels)
                    };
                    let fid = id(&["g", s, "functions"]);
                    vec![
                        rel_group("tables", "Tabelas", &o.tables),
                        rel_group("views", "Views", &o.views),
                        rel_group("matviews", "Views materializadas", &o.matviews),
                        TreeItem::new(fid.clone(), format!("Funções ({})", o.functions.len()))
                            .expanded(open(&fid))
                            .children(o.functions.iter().map(|f| TreeItem::new(id(&["f", s, f]), f.clone()))),
                    ]
                }
            };
            TreeItem::new(sid.clone(), s.clone()).expanded(open(&sid)).children(children)
        })
        .collect()
}

fn relation(c: &MetaCache, schema: &str, rel: &str, expanded: &HashSet<SharedString>) -> TreeItem {
    let rid = id(&["r", schema, rel]);
    let key = (schema.to_string(), rel.to_string());
    let children = match (c.columns.get(&key), c.indexes.get(&key)) {
        (Some(cols), idx) => {
            let cg = id(&["cg", schema, rel]);
            let mut groups = vec![TreeItem::new(cg.clone(), format!("Colunas ({})", cols.len())).expanded(expanded.contains(&cg)).children(
                cols.iter().map(|col| {
                    let pk = if col.is_pk { " 🔑" } else { "" };
                    TreeItem::new(id(&["c", schema, rel, &col.name]), format!("{}  {}{pk}", col.name, col.type_name))
                }),
            )];
            if let Some(idx) = idx {
                let ig = id(&["ig", schema, rel]);
                groups.push(
                    TreeItem::new(ig.clone(), format!("Índices ({})", idx.len()))
                        .expanded(expanded.contains(&ig))
                        .children(idx.iter().map(|i| TreeItem::new(id(&["i", schema, rel, &i.name]), i.name.clone()))),
                );
            }
            groups
        }
        (None, _) => vec![loading(&rid)],
    };
    TreeItem::new(rid.clone(), rel.to_string()).expanded(expanded.contains(&rid)).children(children)
}

impl Render for SchemaTree {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.on_open_table.clone();
        div().key_context("SchemaTree").size_full().on_action(cx.listener(|t, _: &RefreshNode, _, cx| t.refresh(cx))).child(tree(
            &self.state,
            move |ix, entry, selected, _window, _cx| {
                let node = entry.item().id.clone();
                let open = open.clone();
                ListItem::new(ix).selected(selected).pl(px(8. + 14. * entry.depth() as f32)).child(entry.item().label.clone()).on_click(
                    move |ev, window, cx| {
                        let parts: Vec<&str> = node.split(SEP).collect();
                        if ev.click_count() == 2
                            && let ["r", schema, rel] = parts.as_slice()
                        {
                            open(schema.to_string(), rel.to_string(), window, cx);
                        }
                    },
                )
            },
        ))
    }
}
```

Verifique ao compilar: o nome do acessor do item em `TreeEntry` (`entry.item()`), se `ListItem` aceita `.pl(..)` (implementa `Styled`), e se o `on_click` do `ListItem` não impede o clique de expandir o nó. Se impedir, chame `TreeState` para alternar a expansão dentro do handler (procure o método `toggle_expand`/equivalente no `tree.rs` do gpui-base).

- [ ] **Step 2: Integrar no `Workspace`**

Em `workspace.rs`:
- `Active` ganha `pub tree: Entity<SchemaTree>`.
- No `connect`, depois de criar `meta`:

```rust
let ws_weak = cx.entity().downgrade();
let open: Rc<dyn Fn(String, String, &mut Window, &mut App)> = Rc::new(move |schema, table, window, cx| {
    ws_weak.update(cx, |ws, cx| ws.open_table(schema, table, window, cx)).ok();
});
let tree = cx.new(|cx| SchemaTree::new(meta.clone(), open, cx));
ws.active = Some(Active { conn, meta, tree });
```

- Adicione `pub fn open_table(&mut self, _schema: String, _table: String, _window: &mut Window, _cx: &mut Context<Self>) {}`.
- Em `render_sidebar`, depois da lista: `.when_some(self.active.as_ref().map(|a| a.tree.clone()), |d, tree| d.child(div().flex_1().child(tree)))`.
- Imports: `use std::rc::Rc;` e `use crate::schema_tree::SchemaTree;`.

- [ ] **Step 3: Verificar manualmente**

Antes, crie dados no Postgres de teste:
```bash
docker exec -i rb-pg psql -U postgres -c 'create table if not exists public."Order Items" (id int primary key, "Name" text); create index if not exists oi_name on public."Order Items" ("Name");'
```

Run: `cargo run -p rb-app`
Expected:
1. Ao conectar, a árvore lista `public`.
2. Expandir `public` mostra "carregando…" por um instante e depois Tabelas, Views, Views materializadas e Funções.
3. Expandir `Order Items` mostra Colunas (`id integer 🔑`, `Name text`) e Índices.
4. Crie outra tabela via `psql`, selecione `public` e aperte F5: ela aparece.

- [ ] **Step 4: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): árvore de schema com carga preguiçosa e F5"
```

---

### Task 13: Grade de resultados

**Files:**
- Create: `crates/rb-app/src/grid.rs`
- Modify: `crates/rb-app/src/main.rs` (`mod grid;`)

**Interfaces:**
- Consumes: `ResultSet`, `Session::fetch_more`, `PendingEdits`, `decode::one_line`
- Produces:
  - `grid::ResultGrid` (implementa `TableDelegate`) com campos públicos `rs: Option<ResultSet>`, `edits: Option<PendingEdits>` (Some = editável), `sortable: bool`, `on_sort: Option<Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>>`, `error: Option<String>`; métodos `new(sortable) -> Self`, `set_result(&mut self, rs, session: Arc<Session>)`, `loaded_rows() -> usize`, `copy_selection(&self, TableSelection) -> Option<String>`
  - `grid::copy_to_clipboard(table: &Entity<TableState<ResultGrid>>, cx: &mut App)`
  - `grid::MAX_CELL_CHARS = 200`

- [ ] **Step 1: `grid.rs`**

```rust
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableSelection, TableState};
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::decode::one_line;
use rb_core::session::Session;
use rb_core::table::PendingEdits;

pub const MAX_CELL_CHARS: usize = 200;

type OnSort = Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)>;

pub struct ResultGrid {
    pub rs: Option<ResultSet>,
    pub edits: Option<PendingEdits>,
    pub sortable: bool,
    pub on_sort: Option<OnSort>,
    pub error: Option<String>,
    session: Option<Arc<Session>>,
    widths: Vec<f32>,
    loading_more: bool,
    /// Muda a cada resultado novo; descarta lotes que chegam de um resultado antigo.
    generation: u64,
}

impl ResultGrid {
    pub fn new(sortable: bool) -> Self {
        ResultGrid { rs: None, edits: None, sortable, on_sort: None, error: None, session: None, widths: Vec::new(), loading_more: false, generation: 0 }
    }

    pub fn set_result(&mut self, rs: ResultSet, session: Arc<Session>) {
        self.generation += 1;
        self.widths = (0..rs.columns.len())
            .map(|c| {
                let sample = (0..rs.rows().min(100)).map(|r| rs.display(r, c, 60).map_or(4, |s| s.chars().count())).max().unwrap_or(0);
                let chars = sample.max(rs.columns[c].name.chars().count()).clamp(4, 60);
                chars as f32 * 7.5 + 24.
            })
            .collect();
        self.rs = Some(rs);
        self.session = Some(session);
        self.loading_more = false;
        self.error = None;
    }

    pub fn loaded_rows(&self) -> usize {
        self.rs.as_ref().map_or(0, ResultSet::rows)
    }

    /// (texto, é NULL, foi editada, linha marcada para excluir)
    fn cell(&self, row: usize, col: usize) -> (SharedString, bool, bool, bool) {
        let loaded = self.loaded_rows();
        let deleted = self.edits.as_ref().is_some_and(|e| e.is_deleted(row));
        if let Some(v) = self.edits.as_ref().and_then(|e| e.get(row, col, loaded)) {
            return match v {
                Some(s) => (one_line(s, MAX_CELL_CHARS).into(), false, true, deleted),
                None => ("NULL".into(), true, true, deleted),
            };
        }
        if row >= loaded {
            return ("DEFAULT".into(), true, false, false);
        }
        match self.rs.as_ref().and_then(|rs| rs.display(row, col, MAX_CELL_CHARS)) {
            Some(s) => (s.into(), false, false, deleted),
            None => ("NULL".into(), true, false, deleted),
        }
    }

    /// Valor completo de uma célula carregada (para cópia e edição).
    pub fn full_text(&self, row: usize, col: usize) -> Option<String> {
        let loaded = self.loaded_rows();
        if let Some(v) = self.edits.as_ref().and_then(|e| e.get(row, col, loaded)) {
            return v.clone();
        }
        self.rs.as_ref().filter(|_| row < loaded).and_then(|rs| rs.text(row, col))
    }

    pub fn copy_selection(&self, sel: TableSelection) -> Option<String> {
        let rs = self.rs.as_ref()?;
        let all: Vec<usize> = (0..rs.columns.len()).collect();
        match sel {
            TableSelection::Cell(r, c) => Some(self.full_text(r, c).unwrap_or_default()),
            TableSelection::Row(r) if r < rs.rows() => Some(rs.to_tsv(r..r + 1, &all)),
            TableSelection::Column(c) => Some(rs.to_tsv(0..rs.rows(), &[c])),
            _ => None,
        }
    }
}

pub fn copy_to_clipboard(table: &Entity<TableState<ResultGrid>>, cx: &mut App) {
    let t = table.read(cx);
    if let Some(text) = t.delegate().copy_selection(t.selection()) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.rs.as_ref().map_or(0, |rs| rs.columns.len())
    }

    fn rows_count(&self, _: &App) -> usize {
        self.loaded_rows() + self.edits.as_ref().map_or(0, PendingEdits::inserted_rows)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = self.rs.as_ref().map_or(String::new(), |rs| rs.columns[col_ix].name.clone());
        let col = Column::new(format!("c{col_ix}"), name).width(px(self.widths.get(col_ix).copied().unwrap_or(120.)));
        if self.sortable { col.sortable() } else { col }
    }

    fn render_td(&mut self, row: usize, col: usize, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let (text, null, edited, deleted) = self.cell(row, col);
        div()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .when(null, |d| d.italic().text_color(cx.theme().muted_foreground))
            .when(edited, |d| d.bg(cx.theme().accent))
            .when(deleted, |d| d.line_through().text_color(cx.theme().danger))
            .child(text)
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.full_text(row, col).unwrap_or_default()
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if let Some(f) = self.on_sort.clone() {
            f(col_ix, sort, window, cx);
        }
    }

    fn has_more(&self, _: &App) -> bool {
        self.rs.as_ref().is_some_and(|r| r.more)
    }

    fn load_more_threshold(&self) -> usize {
        200
    }

    fn load_more(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) {
        if self.loading_more {
            return;
        }
        let Some(session) = self.session.clone() else { return };
        self.loading_more = true;
        let generation = self.generation;
        cx.spawn(async move |table, cx| {
            let res = session.fetch_more().await;
            table.update(cx, |t, cx| {
                let g = t.delegate_mut();
                if g.generation != generation {
                    return;
                }
                g.loading_more = false;
                match res {
                    Ok(f) => {
                        if let Some(rs) = &mut g.rs {
                            rs.push(f.batch, f.more);
                        }
                    }
                    Err(e) => {
                        if let Some(rs) = &mut g.rs {
                            rs.more = false;
                        }
                        g.error = Some(e.to_string());
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }
}
```

Nomes de cor do tema (`muted_foreground`, `accent`, `danger`): confira em `gpui-component-0.7.3/src/theme/` e ajuste se algum não existir.

- [ ] **Step 2: Compilar**

Run: `cargo build -p rb-app`
Expected: compila (o `grid` ainda não está na tela; aviso de código não usado é esperado até a Tarefa 14).

- [ ] **Step 3: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): grade virtualizada com paginação e cópia"
```

---

### Task 14: Aba SQL

**Files:**
- Create: `crates/rb-app/src/sql_tab.rs`
- Modify: `crates/rb-app/src/main.rs` (`mod sql_tab;`), `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `split::{split_statements, statement_at}`, `error::error_byte_offset`, `Engine::connect`, `Session::{execute, cancel}`, `ResultGrid`, `config::scripts_dir`
- Produces:
  - `sql_tab::SqlTab::new(path: PathBuf, conn: ConnTarget, window, cx) -> Self`, `title(&self) -> SharedString`, `save_now(&self, cx: &App)`, `path: PathBuf`, `editor: Entity<EditorState>` (pública; a Tarefa 15 instala o provider nela)
  - `Workspace.tabs: Vec<TabView>`, `TabView::{Sql(Entity<SqlTab>), Table(..)}` (variante Table na Tarefa 16), `Workspace::new_sql_tab(window, cx)`

- [ ] **Step 1: `sql_tab.rs`**

```rust
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{RopeExt as _, WindowExt as _, h_flex, resizable_panel, v_flex, v_resizable};
use gpui_kit::*;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::error::error_byte_offset;
use rb_core::session::{ExecOutcome, Session};
use rb_core::split::{split_statements, statement_at};

use crate::grid::{self, ResultGrid};
use crate::meta::ConnTarget;
use crate::{CancelQuery, CopyCells, Db, RunScript, RunStatement};

pub struct SqlTab {
    focus: FocusHandle,
    pub path: PathBuf,
    pub editor: Entity<EditorState>,
    table: Entity<TableState<ResultGrid>>,
    conn: ConnTarget,
    session: Option<Arc<Session>>,
    running: bool,
    status: SharedString,
    _save: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl SqlTab {
    pub fn new(path: PathBuf, conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let editor = cx.new(|cx| EditorState::new(window, cx).language("sql").default_value(text));
        let table = cx.new(|cx| TableState::new(ResultGrid::new(false), window, cx).cell_selectable(true));
        let subs = vec![cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.schedule_save(cx);
            }
        })];
        SqlTab {
            focus: cx.focus_handle(),
            path,
            editor,
            table,
            conn,
            session: None,
            running: false,
            status: "".into(),
            _save: None,
            _subs: subs,
        }
    }

    pub fn title(&self) -> SharedString {
        self.path.file_stem().map_or("script".into(), |s| s.to_string_lossy().to_string().into())
    }

    /// Salva 1 s depois da última alteração. Trocar a task cancela a anterior.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let editor = self.editor.clone();
        self._save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let Ok(text) = cx.update(|cx| editor.read(cx).value().to_string()) else { return };
            let res = cx.background_spawn(async move { std::fs::write(&path, text) }).await;
            if let Err(e) = res {
                tracing::warn!("falha ao salvar script: {e}");
            }
        }));
    }

    /// Gravação síncrona ao fechar a aba (arquivo pequeno).
    pub fn save_now(&self, cx: &App) {
        if let Err(e) = std::fs::write(&self.path, self.editor.read(cx).value().as_ref()) {
            tracing::warn!("falha ao salvar script: {e}");
        }
    }

    fn run(&mut self, whole_script: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let state = self.editor.read(cx);
        let src = state.value().to_string();
        let cursor = state.cursor();
        let ranges: Vec<Range<usize>> = if whole_script { split_statements(&src) } else { statement_at(&src, cursor).into_iter().collect() };
        if ranges.is_empty() {
            return;
        }
        self.running = true;
        self.status = "executando…".into();
        cx.notify();

        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match engine.connect(&conn.cfg, conn.password.clone()).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, None, e, w, cx)),
                },
            };
            this.update(cx, |t, _| t.session = Some(session.clone()))?;
            let mut last = None;
            for r in ranges {
                match session.execute(src[r.clone()].to_string()).await {
                    Ok(out) => last = Some(out),
                    Err(e) => return this.update_in(cx, |t, w, cx| t.finish_error(&src, Some(r), e, w, cx)),
                }
            }
            this.update(cx, |t, cx| t.finish_ok(last, session, cx))
        })
        .detach();
    }

    fn finish_ok(&mut self, last: Option<ExecOutcome>, session: Arc<Session>, cx: &mut Context<Self>) {
        self.running = false;
        let Some(out) = last else { return };
        let ms = out.elapsed.as_millis();
        self.status = match out.affected {
            Some(n) => format!("{n} linha(s) afetada(s) · {ms} ms"),
            None => format!("{ms} ms"),
        }
        .into();
        let rs = ResultSet::new(out.columns, out.batch, out.more);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_result(rs, session);
            t.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn finish_error(&mut self, src: &str, range: Option<Range<usize>>, e: DbError, window: &mut Window, cx: &mut Context<Self>) {
        self.running = false;
        self.status = "erro".into();
        if let (Some(r), DbError::Postgres { position: Some(p), .. }) = (&range, &e) {
            let at = r.start + error_byte_offset(&src[r.clone()], *p);
            let word_end = src[at..].find(|c: char| c.is_whitespace() || c == ',' || c == ')' || c == ';').map_or(src.len(), |n| at + n);
            let end = if word_end > at { word_end } else { src[at..].chars().next().map_or(at, |c| at + c.len_utf8()) };
            let msg = e.to_string();
            self.editor.update(cx, |ed, cx| {
                let range = ed.text().offset_to_position(at)..ed.text().offset_to_position(end);
                if let Some(d) = ed.diagnostics_mut() {
                    d.clear();
                    d.push(Diagnostic::new(range, msg).with_severity(DiagnosticSeverity::Error));
                }
                cx.notify();
            });
        }
        window.push_notification(Notification::error(e.to_string()), cx);
        cx.notify();
    }

    fn cancel(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }
}

impl Render for SqlTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grid = self.table.read(cx).delegate();
        let rows = grid.loaded_rows();
        let more = grid.rs.as_ref().is_some_and(|r| r.more);
        let grid_error = grid.error.clone();
        v_flex()
            .key_context("SqlTab")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|t, _: &RunStatement, w, cx| t.run(false, w, cx)))
            .on_action(cx.listener(|t, _: &RunScript, w, cx| t.run(true, w, cx)))
            .on_action(cx.listener(|t, _: &CancelQuery, _, _| t.cancel()))
            .child(
                v_resizable("sql-split")
                    .child(resizable_panel().size(px(280.)).child(Editor::new(&self.editor).h_full()))
                    .child(resizable_panel().child(
                        div()
                            .key_context("ResultGrid")
                            .size_full()
                            .on_action(cx.listener(|t, _: &CopyCells, _, cx| grid::copy_to_clipboard(&t.table, cx)))
                            .child(DataTable::new(&self.table).stripe(true).bordered(true)),
                    )),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .child(format!("{rows}{} linha(s)", if more { "+" } else { "" }))
                    .child(self.status.clone())
                    .when_some(grid_error, |d, e| d.child(e))
                    .when(self.running, |d| {
                        d.child(Button::new("cancel").label("Cancelar ⌘.").ghost().on_click(cx.listener(|t, _, _, _| t.cancel())))
                    }),
            )
    }
}
```

A grade é uma entidade separada: para o rodapé atualizar quando chegam lotes novos, adicione em `new` `cx.observe(&table, |_, _, cx| cx.notify())` em `_subs`.

- [ ] **Step 2: Abas no `Workspace`**

Em `workspace.rs`:

```rust
#[derive(Clone)]
pub enum TabView {
    Sql(Entity<SqlTab>),
}

impl TabView {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            TabView::Sql(t) => t.read(cx).title(),
        }
    }
}
```

Campos novos em `Workspace`: `pub tabs: Vec<TabView>`, `pub active_tab: usize` (inicializar com `Vec::new()` e `0`).

Métodos:
```rust
    pub fn new_sql_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
        let dir = config::scripts_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            window.push_notification(Notification::error(e.to_string()), cx);
            return;
        }
        let n = (1..).find(|n| !dir.join(format!("script-{n}.sql")).exists()).unwrap_or(1);
        let path = dir.join(format!("script-{n}.sql"));
        self.open_sql_file(path, conn, window, cx);
    }

    fn open_sql_file(&mut self, path: PathBuf, conn: ConnTarget, window: &mut Window, cx: &mut Context<Self>) {
        let tab = cx.new(|cx| SqlTab::new(path, conn, window, cx));
        self.tabs.push(TabView::Sql(tab));
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    /// Fechar não apaga: o arquivo vai para scripts/closed/.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if let TabView::Sql(t) = self.tabs.remove(ix) {
            let t = t.read(cx);
            t.save_now(cx);
            let closed = config::scripts_dir().join("closed");
            let moved = std::fs::create_dir_all(&closed).and_then(|_| std::fs::rename(&t.path, closed.join(t.path.file_name().unwrap_or_default())));
            if let Err(e) = moved {
                tracing::warn!("falha ao arquivar script: {e}");
            }
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        cx.notify();
    }
```

`on_connected` reabre os scripts existentes (ordenados) ou cria um novo:
```rust
    fn on_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active.as_ref().map(|a| a.conn.clone()) else { return };
        self.tabs.clear();
        let mut files: Vec<PathBuf> = std::fs::read_dir(config::scripts_dir())
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "sql")).collect())
            .unwrap_or_default();
        files.sort();
        if files.is_empty() {
            self.new_sql_tab(window, cx);
        }
        for f in files {
            self.open_sql_file(f, conn.clone(), window, cx);
        }
    }
```

Ações: troque os listeners vazios por
```rust
            .on_action(cx.listener(|ws, _: &NewSqlTab, window, cx| ws.new_sql_tab(window, cx)))
            .on_action(cx.listener(|ws, _: &CloseTab, _, cx| ws.close_tab(ws.active_tab, cx)))
```

Área principal (substitui o filho central do `render`):
```rust
    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.tabs.is_empty() {
            return v_flex().flex_1().items_center().justify_center().child("Conecte e use ⌘T para abrir um script").into_any_element();
        }
        let bar = self.tabs.iter().enumerate().fold(
            TabBar::new("tabs").selected_index(self.active_tab).on_click(cx.listener(|ws, ix: &usize, _, cx| {
                ws.active_tab = *ix;
                cx.notify();
            })),
            |bar, (i, t)| {
                bar.child(
                    Tab::new()
                        .label(t.title(cx))
                        .suffix(Button::new(("close", i)).label("×").ghost().on_click(cx.listener(move |ws, _, _, cx| ws.close_tab(i, cx)))),
                )
            },
        );
        let content = match &self.tabs[self.active_tab] {
            TabView::Sql(t) => t.clone().into_any_element(),
        };
        v_flex().flex_1().h_full().child(bar).child(div().flex_1().child(content)).into_any_element()
    }
```

Imports: `use std::path::PathBuf;`, `use gpui_kit::component::tab::{Tab, TabBar};`, `use crate::sql_tab::SqlTab;`.

- [ ] **Step 3: Verificar manualmente**

Run: `RUST_LOG=info cargo run --release -p rb-app`
Expected:
1. Ao conectar, abre `script-1`.
2. `select g, md5(g::text) from generate_series(1, 1000000) g` + `⌘↩` mostra 1000+ linhas. O log tem `primeiro lote elapsed_ms` (meta < 200 ms). Rolar até o fim carrega mais lotes sem engasgar, e o rodapé conta as linhas.
3. Com `⌘↩` em `select pg_sleep(30)` e depois `⌘.`, o cancelamento aparece em menos de 1 s.
4. `select nope from pg_class` sublinha `nope` e mostra a notificação `[42703]`.
5. Com dois statements e o cursor no segundo, `⌘↩` roda só o segundo; `⌘⇧↩` roda os dois.
6. Digite algo, espere 2 s e confira que `scripts/script-1.sql` tem o texto.
7. `⌘T` abre `script-2`; `⌘W` fecha a aba e move o arquivo para `scripts/closed/`.
8. Selecione uma célula e aperte `⌘C`: o valor completo vai para a área de transferência.

Se `⌘↩` inserir uma nova linha em vez de executar, a ligação `"SqlTab > Input"` não venceu a do Input. Nesse caso troque o contexto por `"SqlTab > Input"` registrado **depois** de `gpui_kit::init` (já é) e, se ainda falhar, use `"Input && !Search"` + checagem do ancestral; registre a solução em `gpui-api-notes.md`.

- [ ] **Step 4: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): aba SQL com execução, cancelamento, erros e autosave"
```

---

### Task 15: Autocomplete no editor

**Files:**
- Create: `crates/rb-app/src/completion.rs`
- Modify: `crates/rb-app/src/main.rs` (`mod completion;`), `crates/rb-app/src/sql_tab.rs`, `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `rb_core::complete::{complete, CandidateKind}`, `MetaStore::{cache, request}`
- Produces: `completion::SqlCompletion::new(meta: Entity<MetaStore>)`, que implementa `CompletionProvider`. `SqlTab::new` ganha o parâmetro `meta: Entity<MetaStore>`.

- [ ] **Step 1: `completion.rs`**

```rust
use std::time::Instant;

use gpui_kit::component::input::CompletionProvider;
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, TextEdit};
use rb_core::complete::{CandidateKind, complete};

use crate::meta::MetaStore;

pub struct SqlCompletion {
    meta: Entity<MetaStore>,
}

impl SqlCompletion {
    pub fn new(meta: Entity<MetaStore>) -> Self {
        SqlCompletion { meta }
    }
}

fn kind(k: CandidateKind) -> CompletionItemKind {
    match k {
        CandidateKind::Column => CompletionItemKind::FIELD,
        CandidateKind::Table => CompletionItemKind::CLASS,
        CandidateKind::View => CompletionItemKind::INTERFACE,
        CandidateKind::Function => CompletionItemKind::FUNCTION,
        CandidateKind::Schema => CompletionItemKind::MODULE,
        CandidateKind::Keyword => CompletionItemKind::KEYWORD,
    }
}

impl CompletionProvider for SqlCompletion {
    fn completions(&self, text: &Rope, offset: usize, _: CompletionContext, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<CompletionResponse>> {
        let started = Instant::now();
        // ponytail: copia o script a cada tecla; ok para scripts de até centenas de KB.
        let src = text.to_string();
        let comp = complete(&src, offset, &self.meta.read(cx).cache);
        if !comp.missing.is_empty() {
            // Carrega em segundo plano; os itens aparecem na próxima tecla.
            let missing = comp.missing.clone();
            self.meta.update(cx, |m, cx| missing.into_iter().for_each(|r| m.request(r, cx)));
        }
        let range = lsp_types::Range { start: text.offset_to_position(comp.replace.start), end: text.offset_to_position(comp.replace.end) };
        let typed = src[comp.replace.clone()].to_string();
        let items = comp
            .candidates
            .into_iter()
            .map(|c| CompletionItem {
                label: c.label,
                kind: Some(kind(c.kind)),
                detail: c.detail,
                filter_text: Some(typed.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: c.insert })),
                ..Default::default()
            })
            .collect();
        tracing::debug!(us = started.elapsed().as_micros() as u64, "autocomplete");
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _: &mut App) -> bool {
        !new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}
```

O caminho de `Rope`/`RopeExt` é `gpui_kit::component::{Rope, RopeExt}` (reexport do `input`). Se o `completions` do trait usar outro tipo de `Result`, siga a assinatura do fonte (`gpui-base-0.7.1/src/input/editor/lsp/completions.rs:40`).

- [ ] **Step 2: Instalar o provider**

Em `SqlTab::new`, receba `meta: Entity<MetaStore>` e, depois de criar `editor`:
```rust
        editor.update(cx, |ed, _| ed.lsp_mut().completion_provider = Some(std::rc::Rc::new(SqlCompletion::new(meta))));
```
No `Workspace`, `open_sql_file` passa `active.meta.clone()` (pegue de `self.active` antes de chamar). Imports: `use crate::completion::SqlCompletion;` e `use crate::meta::MetaStore;`.

- [ ] **Step 3: Verificar manualmente**

Run: `RUST_LOG=rb_app=debug,info cargo run --release -p rb-app`
Expected:
1. `select * from Ord` sugere `Order Items` e insere `"Order Items"` com aspas.
2. `select oi. from "Order Items" oi` com o cursor depois de `oi.` sugere `id` e `Name`. Na primeira vez pode vir vazio; a próxima tecla mostra os itens.
3. `select  from "Order Items"`: o cursor depois de `select ` mostra primeiro as colunas e depois as palavras-chave.
4. Dentro de `'...'` ou depois de `--`, nenhum popup aparece.
5. O log `autocomplete us=` fica abaixo de 16000. Para medir com 1.000 tabelas, use `scripts/seed-perf.sql` da Tarefa 17.

- [ ] **Step 4: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): autocomplete SQL no editor"
```

---

### Task 16: Aba de tabela com filtro, ordenação e edição

**Files:**
- Create: `crates/rb-app/src/table_tab.rs`
- Modify: `crates/rb-app/src/main.rs` (`mod table_tab;`), `crates/rb-app/src/workspace.rs`

**Interfaces:**
- Consumes: `table::{select_sql, EditTarget, PendingEdits, build_sql}`, `MetaStore`, `ResultGrid`, `Session::run_in_transaction`
- Produces: `table_tab::TableTab::new(schema, table, conn: ConnTarget, meta: Entity<MetaStore>, window, cx)`, `title()`; `TabView::Table(Entity<TableTab>)`; `Workspace::open_table` implementado.

- [ ] **Step 1: `table_tab.rs`**

```rust
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::table::{ColumnSort, DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;
use rb_core::batch::ResultSet;
use rb_core::metadata::MetaRequest;
use rb_core::session::Session;
use rb_core::table::{EditTarget, PendingEdits, build_sql, select_sql};

use crate::grid::{self, ResultGrid};
use crate::meta::{ConnTarget, MetaStore};
use crate::{CancelQuery, CopyCells, Db, SaveEdits};

pub struct TableTab {
    focus: FocusHandle,
    schema: String,
    table: String,
    conn: ConnTarget,
    meta: Entity<MetaStore>,
    session: Option<Arc<Session>>,
    filter: Entity<InputState>,
    grid: Entity<TableState<ResultGrid>>,
    target: Option<EditTarget>,
    sort: Option<(usize, bool)>,
    status: SharedString,
    loaded_once: bool,
    _subs: Vec<Subscription>,
}

impl TableTab {
    pub fn new(schema: String, table: String, conn: ConnTarget, meta: Entity<MetaStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("filtro WHERE, ex.: id > 10 — Enter aplica"));
        let grid = cx.new(|cx| TableState::new(ResultGrid::new(true), window, cx).cell_selectable(true));

        let weak = cx.entity().downgrade();
        let on_sort: Rc<dyn Fn(usize, ColumnSort, &mut Window, &mut App)> = Rc::new(move |col, sort, _, cx| {
            let weak = weak.clone();
            // A grade está em update agora; recarregar depois evita empréstimo duplo.
            cx.defer(move |cx| {
                weak.update(cx, |tab, cx| tab.set_sort(col, sort, cx)).ok();
            });
        });
        grid.update(cx, |t, _| t.delegate_mut().on_sort = Some(on_sort));

        let subs = vec![
            cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.load(cx);
                }
            }),
            cx.subscribe_in(&grid, window, |this, _, ev: &TableEvent, window, cx| {
                if let TableEvent::DoubleClickedCell(r, c) = ev {
                    this.edit_cell(*r, *c, window, cx);
                }
            }),
            cx.observe(&grid, |_, _, cx| cx.notify()),
            cx.observe(&meta, |this, _, cx| this.on_meta(cx)),
        ];
        meta.update(cx, |m, cx| m.request(MetaRequest::Columns(schema.clone(), table.clone()), cx));
        let mut this = TableTab {
            focus: cx.focus_handle(),
            schema,
            table,
            conn,
            meta,
            session: None,
            filter,
            grid,
            target: None,
            sort: None,
            status: "carregando…".into(),
            loaded_once: false,
            _subs: subs,
        };
        this.on_meta(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        format!("{}.{}", self.schema, self.table).into()
    }

    /// Quando as colunas chegam: decide se a grade é editável e faz a primeira carga.
    fn on_meta(&mut self, cx: &mut Context<Self>) {
        if self.loaded_once {
            return;
        }
        let key = (self.schema.clone(), self.table.clone());
        let Some(cols) = self.meta.read(cx).cache.columns.get(&key).cloned() else { return };
        self.loaded_once = true;
        let target = EditTarget { schema: self.schema.clone(), table: self.table.clone(), columns: cols };
        let editable = !target.pk().is_empty();
        self.grid.update(cx, |t, _| t.delegate_mut().edits = editable.then(PendingEdits::default));
        if !editable {
            self.status = "somente leitura: tabela sem chave primária".into();
        }
        self.target = Some(target);
        self.load(cx);
    }

    fn has_pending(&self, cx: &App) -> bool {
        self.grid.read(cx).delegate().edits.as_ref().is_some_and(|e| !e.is_empty())
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if self.has_pending(cx) {
            self.status = "grave (⌘S) ou descarte as edições antes de recarregar".into();
            cx.notify();
            return;
        }
        let filter = self.filter.read(cx).value().to_string();
        let order_col = self.sort.and_then(|(c, asc)| {
            let name = self.target.as_ref()?.columns.get(c)?.name.clone();
            Some((name, asc))
        });
        let sql = select_sql(&self.schema, &self.table, Some(&filter), order_col.as_ref().map(|(n, a)| (n.as_str(), *a)));
        let conn = self.conn.clone();
        let existing = self.session.clone();
        let engine = cx.global::<Db>().0.clone();
        self.status = "carregando…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = async {
                let session = match existing {
                    Some(s) => s,
                    None => Arc::new(engine.connect(&conn.cfg, conn.password.clone()).await?),
                };
                let out = session.execute(sql).await?;
                Ok::<_, rb_core::DbError>((session, out))
            }
            .await;
            this.update(cx, |tab, cx| {
                match result {
                    Ok((session, out)) => {
                        tab.session = Some(session.clone());
                        tab.status = format!("{} ms", out.elapsed.as_millis()).into();
                        let rs = ResultSet::new(out.columns, out.batch, out.more);
                        tab.grid.update(cx, |t, cx| {
                            t.delegate_mut().set_result(rs, session);
                            t.refresh(cx);
                            cx.notify();
                        });
                    }
                    Err(e) => tab.status = e.to_string().into(),
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn set_sort(&mut self, col: usize, sort: ColumnSort, cx: &mut Context<Self>) {
        self.sort = match sort {
            ColumnSort::Ascending => Some((col, true)),
            ColumnSort::Descending => Some((col, false)),
            ColumnSort::Default => None,
        };
        self.load(cx);
    }

    fn edit_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = &self.target else { return };
        if self.grid.read(cx).delegate().edits.is_none() {
            return;
        }
        if target.columns.get(col).is_some_and(|c| c.generated) {
            self.status = "coluna gerada: não editável".into();
            cx.notify();
            return;
        }
        let current = self.grid.read(cx).delegate().full_text(row, col).unwrap_or_default();
        let col_name = target.columns.get(col).map_or(String::new(), |c| format!("{} ({})", c.name, c.type_name));
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(false).default_value(current));
        let grid = self.grid.clone();
        let set = Rc::new(move |value: Option<String>, window: &mut Window, cx: &mut App| {
            grid.update(cx, |t, cx| {
                let g = t.delegate_mut();
                let loaded = g.loaded_rows();
                if let Some(e) = &mut g.edits {
                    e.set(row, col, value, loaded);
                }
                cx.notify();
            });
            window.close_dialog(cx);
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let (ed, set_null, set_ok) = (editor.clone(), set.clone(), set.clone());
            dialog.title(col_name.clone()).w(px(560.)).child(Editor::new(&editor).h(px(220.))).footer(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cell-null").label("Definir NULL").on_click(move |_, w, cx| set_null(None, w, cx)))
                    .child(Button::new("cell-cancel").label("Cancelar").on_click(|_, w, cx| w.close_dialog(cx)))
                    .child(Button::new("cell-ok").label("OK").primary().on_click(move |_, w, cx| {
                        let v = ed.read(cx).value().to_string();
                        set_ok(Some(v), w, cx)
                    })),
            )
        });
    }

    fn selected_row(&self, cx: &App) -> Option<usize> {
        match self.grid.read(cx).selection() {
            TableSelection::Row(r) | TableSelection::Cell(r, _) => Some(r),
            _ => None,
        }
    }

    fn with_edits(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut PendingEdits, usize)) {
        self.grid.update(cx, |t, cx| {
            let g = t.delegate_mut();
            let loaded = g.loaded_rows();
            if let Some(e) = &mut g.edits {
                f(e, loaded);
            }
            cx.notify();
        });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(session)) = (self.target.clone(), self.session.clone()) else { return };
        let g = self.grid.read(cx).delegate();
        let (Some(edits), Some(rs)) = (&g.edits, &g.rs) else { return };
        if edits.is_empty() {
            return;
        }
        let stmts = match build_sql(&target, &edits.to_row_edits(&target, |r, c| rs.text(r, c))) {
            Ok(s) => s,
            Err(e) => {
                self.status = e.to_string().into();
                cx.notify();
                return;
            }
        };
        self.status = "gravando…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = session.run_in_transaction(stmts).await;
            this.update(cx, |tab, cx| match res {
                Ok(n) => {
                    tab.grid.update(cx, |t, _| t.delegate_mut().edits = Some(PendingEdits::default()));
                    tab.load(cx);
                    tab.status = format!("{n} linha(s) gravada(s)").into();
                }
                Err(e) => {
                    tab.status = format!("Erro ao gravar — nada foi alterado: {e}").into();
                    cx.notify();
                }
            })
        })
        .detach();
    }

    fn cancel(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }
}

impl Render for TableTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.grid.read(cx).delegate();
        let editable = g.edits.is_some();
        let pending = g.edits.as_ref().is_some_and(|e| !e.is_empty());
        let rows = g.loaded_rows();
        let more = g.rs.as_ref().is_some_and(|r| r.more);
        v_flex()
            .key_context("TableTab")
            .track_focus(&self.focus)
            .size_full()
            .on_action(cx.listener(|t, _: &SaveEdits, _, cx| t.save(cx)))
            .on_action(cx.listener(|t, _: &CancelQuery, _, _| t.cancel()))
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.filter)))
                    .child(Button::new("reload").label("Recarregar").ghost().on_click(cx.listener(|t, _, _, cx| t.load(cx))))
                    .when(editable, |d| {
                        d.child(Button::new("add-row").label("+ Linha").ghost().on_click(cx.listener(|t, _, _, cx| t.with_edits(cx, |e, _| e.add_row()))))
                            .child(Button::new("del-row").label("Excluir linha").ghost().on_click(cx.listener(|t, _, _, cx| {
                                if let Some(r) = t.selected_row(cx) {
                                    t.with_edits(cx, |e, loaded| e.toggle_delete(r, loaded));
                                }
                            })))
                            .child(Button::new("discard").label("Descartar").ghost().on_click(cx.listener(|t, _, _, cx| {
                                t.with_edits(cx, |e, _| *e = PendingEdits::default());
                            })))
                            .child(Button::new("save").label("Salvar ⌘S").primary().disabled(!pending).on_click(cx.listener(|t, _, _, cx| t.save(cx))))
                    }),
            )
            .child(
                div()
                    .key_context("ResultGrid")
                    .flex_1()
                    .on_action(cx.listener(|t, _: &CopyCells, _, cx| grid::copy_to_clipboard(&t.grid, cx)))
                    .child(DataTable::new(&self.grid).stripe(true).bordered(true)),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_3()
                    .text_sm()
                    .child(format!("{rows}{} linha(s)", if more { "+" } else { "" }))
                    .when(pending, |d| d.child(div().text_color(cx.theme().warning).child("edições pendentes")))
                    .child(self.status.clone()),
            )
    }
}
```

`Button::disabled` vem do trait `Disableable` (`use gpui_kit::component::Disableable as _;` se não estiver no escopo). Se `cx.theme().warning` não existir, use outra cor do tema.

- [ ] **Step 2: Integrar no `Workspace`**

- Adicione `Table(Entity<TableTab>)` em `TabView`, com `title` (`TabView::Table(t) => t.read(cx).title()`) e com o ramo correspondente no `match` de `render_main`.
- Implemente `open_table`. Se a aba da tabela já existir, ele só a ativa:

```rust
    pub fn open_table(&mut self, schema: String, table: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let title: SharedString = format!("{schema}.{table}").into();
        if let Some(ix) = self.tabs.iter().position(|t| matches!(t, TabView::Table(_)) && t.title(cx) == title) {
            self.active_tab = ix;
            cx.notify();
            return;
        }
        let (conn, meta) = (active.conn.clone(), active.meta.clone());
        let tab = cx.new(|cx| TableTab::new(schema, table, conn, meta, window, cx));
        self.tabs.push(TabView::Table(tab));
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }
```

- Em `close_tab`, a variante `Table` só sai da lista; não há arquivo a arquivar.

- [ ] **Step 3: Verificar manualmente**

Preparar:
```bash
docker exec -i rb-pg psql -U postgres -c "insert into public.\"Order Items\" select g, 'item ' || g from generate_series(1, 5000) g on conflict do nothing; create table if not exists public.sem_pk (a int); insert into public.sem_pk values (1);"
```

Run: `cargo run -p rb-app`
Expected:
1. Duplo clique em `Order Items` na árvore abre a aba `public.Order Items` com 1000+ linhas. Rolar carrega o resto.
2. Digitar `id > 4990` no filtro e apertar Enter deixa 10 linhas.
3. Clicar no cabeçalho `Name` ordena no servidor (o SQL usa `ORDER BY "Name"`).
4. Duplo clique numa célula `Name`, digitar `novo`, OK: a célula fica destacada e o rodapé mostra "edições pendentes".
5. "Recarregar" com edições pendentes é recusado com a mensagem.
6. `⌘S` grava, recarrega e mostra "1 linha(s) gravada(s)". Confira via `psql`.
7. "+ Linha" cria uma linha `DEFAULT`; edite `id`=`99999` e `Name`, depois `⌘S`, e a linha aparece no banco.
8. Selecione uma linha, clique "Excluir linha" (fica riscada) e `⌘S`: a linha some do banco.
9. Edite `id` de uma linha para um id já existente e `⌘S`: aparece o erro de chave duplicada com "nada foi alterado" e as edições continuam pendentes.
10. Abrir `sem_pk` mostra "somente leitura: tabela sem chave primária", sem botões de edição.

- [ ] **Step 4: Commit**

```bash
git add crates/rb-app
git commit -m "feat(app): aba de tabela com filtro, ordenação e edição"
```

---

### Task 17: Medição de performance e checklist manual

**Files:**
- Create: `scripts/seed-perf.sql`, `docs/perf.md`, `docs/manual-checklist.md`

**Interfaces:** nenhuma interface de código.

- [ ] **Step 1: `scripts/seed-perf.sql`**

```sql
-- Dados para medir as metas de performance (spec §8). Idempotente.
create table if not exists perf_big as
select g as id, md5(g::text) as a, md5((g * 2)::text) as b, g % 1000 as c, now() - g * interval '1 second' as d,
       g::numeric / 7 as e, (g % 2 = 0) as f, repeat('x', 20) as h, gen_random_uuid() as i, jsonb_build_object('g', g) as j
from generate_series(1, 1000000) g;

do $$
begin
  for n in 1..1000 loop
    execute format('create table if not exists perf_t%s (id int primary key, nome text)', n);
  end loop;
end $$;
```

- [ ] **Step 2: `docs/perf.md`**

````markdown
# Medindo as metas de performance

Preparação: `docker exec -i rb-pg psql -U postgres < scripts/seed-perf.sql`.
Sempre com build de release: `cargo build --release -p rb-app`.

| Métrica | Meta | Como medir |
|---|---|---|
| Abrir até janela visível | < 300 ms | `RUST_LOG=info target/release/rustbeaver` → log `janela aberta elapsed_ms` |
| Memória parada, 1 conexão | < 150 MB | conectar, esperar 10 s, `ps -o rss= -p $(pgrep rustbeaver)` (KB) |
| Primeiro lote de `select * from perf_big` | < 200 ms | log `primeiro lote elapsed_ms` |
| Rolagem com 1M linhas | nenhum frame > 16 ms | carregar tudo (rolar até o fim), rolar rápido com Instruments → template "Animation Hitches" |
| Autocomplete com 1.000 tabelas | < 16 ms | `RUST_LOG=rb_app=debug,info` → log `autocomplete us` digitando `select * from perf_t` |
| Memória com 1M × 10 colunas carregadas | < 400 MB | depois de carregar `perf_big` inteiro: `/usr/bin/time -l target/release/rustbeaver` ao sair → "maximum resident set size" |

Registre os resultados na tabela abaixo a cada medição.

| Data | Commit | Janela | Parado | 1º lote | Rolagem | Autocomplete | 1M×10 |
|---|---|---|---|---|---|---|---|
````

- [ ] **Step 3: `docs/manual-checklist.md`**

```markdown
# Checklist manual da UI

Rode antes de cada release, com o Postgres de teste (`docker start rb-pg`).

- [ ] Nova conexão salva no TOML sem senha; senha no Keychain
- [ ] Conectar lista schemas; expandir carrega objetos, colunas e índices; F5 recarrega
- [ ] ⌘↩ roda o statement sob o cursor; ⌘⇧↩ roda o script
- [ ] Resultado grande pagina ao rolar; contador mostra `+` enquanto há mais
- [ ] ⌘. cancela `select pg_sleep(30)` em < 1 s
- [ ] Erro com posição sublinha o trecho e mostra SQLSTATE
- [ ] Script salvo sozinho em ~1 s; ⌘W move para `scripts/closed/`
- [ ] ⌘C copia célula (valor completo), linha e coluna (TSV)
- [ ] Autocomplete: tabelas após FROM, colunas por alias, aspas em nomes mistos, nada dentro de strings
- [ ] Tabela: filtro, ordenação no servidor, editar, NULL, inserir, excluir, ⌘S
- [ ] Erro ao gravar não altera nada e mantém edições
- [ ] Tabela sem PK é somente leitura
- [ ] Derrubar o Postgres (`docker restart rb-pg`) e rodar de novo: reconecta
- [ ] Célula com 5 MB de texto rola sem travar (`select repeat('x', 5000000)`)
```

- [ ] **Step 4: Medir e registrar**

Run: siga `docs/perf.md` e preencha a primeira linha da tabela de resultados.
Expected: todas as metas atendidas. Se alguma falhar, **não otimize nesta tarefa**: registre o número e abra a investigação como trabalho separado.

- [ ] **Step 5: Commit**

```bash
git add scripts docs/perf.md docs/manual-checklist.md
git commit -m "docs: medição de performance e checklist manual"
```
