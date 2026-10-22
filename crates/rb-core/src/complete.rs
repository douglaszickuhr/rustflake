use std::collections::HashSet;
use std::ops::Range;

use crate::dialect::Dialect;
use crate::lexer::{TokKind, Token, ident_text, is_unterminated, tokenize};
use crate::metadata::{ColumnMeta, MetaCache, MetaRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CandidateKind {
    Column,
    Table,
    View,
    Function,
    Schema,
    Database,
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

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "ON",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "WITH",
    "UNION ALL",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "ASC",
    "DESC",
    "COUNT",
    "COALESCE",
    "CAST",
];

const PG_KEYWORDS: &[&str] = &["RETURNING", "ON CONFLICT", "CREATE INDEX", "EXPLAIN ANALYZE", "NOW()", "LATERAL"];

const SF_KEYWORDS: &[&str] = &[
    "QUALIFY",
    "LATERAL FLATTEN",
    "SAMPLE",
    "MERGE INTO",
    "CREATE OR REPLACE TABLE",
    "USE WAREHOUSE",
    "USE ROLE",
    "USE DATABASE",
    "USE SCHEMA",
    "SHOW TABLES",
    "DESCRIBE TABLE",
    "IFF",
    "TRY_CAST",
    "PARSE_JSON",
    "OBJECT_CONSTRUCT",
    "ARRAY_AGG",
    "DATEADD",
    "DATEDIFF",
    "CURRENT_TIMESTAMP()",
];

const TABLE_CONTEXT: &[&str] = &["from", "join", "update", "into", "table"];

const NOT_ALIAS: &[&str] = &[
    "where",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "on",
    "using",
    "natural",
    "lateral",
    "group",
    "order",
    "limit",
    "offset",
    "set",
    "values",
    "select",
    "having",
    "window",
    "union",
    "except",
    "intersect",
    "returning",
    "fetch",
    "for",
    "as",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
    /// A subquery in FROM (`from (select …) q`): byte range of its body; `table` and `alias` are `q`.
    sub: Option<(usize, usize)>,
}

pub fn complete(src: &str, cursor: usize, cache: &MetaCache) -> Completion {
    let toks = tokenize(src);
    let inside_literal = toks.iter().any(|t| {
        let literal = matches!(t.kind, TokKind::Str | TokKind::DollarStr | TokKind::Comment | TokKind::QuotedIdent);
        literal && ((t.start < cursor && cursor < t.end) || (cursor == t.end && is_unterminated(src, t)))
    });
    if inside_literal {
        return Completion { replace: cursor..cursor, ..Default::default() };
    }

    // Tokens of the current statement (between the previous `;` and the next).
    let begin = toks.iter().rposition(|t| t.kind == TokKind::Semicolon && t.end <= cursor).map_or(0, |i| i + 1);
    let end = toks[begin..].iter().position(|t| t.kind == TokKind::Semicolon).map_or(toks.len(), |i| begin + i);
    let stmt: Vec<Token> = toks[begin..end].iter().copied().filter(|t| t.kind != TokKind::Comment).collect();

    let (replace, prefix) = match stmt.iter().find(|t| t.kind == TokKind::Word && t.start < cursor && cursor <= t.end) {
        Some(t) => (t.start..cursor, src[t.start..cursor].to_lowercase()),
        None => (cursor..cursor, String::new()),
    };
    let before: Vec<&Token> = stmt.iter().filter(|t| t.end <= replace.start).collect();
    let refs = table_refs(src, &stmt, cache.dialect);
    let mut out = Completion { replace: replace.clone(), ..Default::default() };
    // Snowflake: schemas arrive per database; without those of the current database, `SCHEMA.` and the schema list
    // would stay empty until someone expands the tree.
    if let (Dialect::Snowflake, Some(db)) = (cache.dialect, cache.current_db())
        && !cache.loaded_dbs.contains(db)
    {
        out.missing.push(MetaRequest::Schemas(db.to_string()));
    }
    let is_punct = |t: &Token, p: &str| t.kind == TokKind::Punct && &src[t.start..t.end] == p;

    let ident = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent);
    if let (Dialect::Snowflake, [.., d, dot1, sc, dot2]) = (cache.dialect, before.as_slice())
        && is_punct(dot1, ".")
        && is_punct(dot2, ".")
        && dot2.end == replace.start
        && ident(d)
        && ident(sc)
        && d.end == dot1.start
        && dot1.end == sc.start
        && sc.end == dot2.start
    {
        // `DB.SCHEMA.`
        let key = format!("{}.{}", cache.dialect.fold(&src[d.start..d.end]), cache.dialect.fold(&src[sc.start..sc.end]));
        push_objects(&mut out, cache, &key);
    } else if let [.., q, dot] = before.as_slice()
        && is_punct(dot, ".")
        && dot.end == replace.start
        && matches!(q.kind, TokKind::Word | TokKind::QuotedIdent)
        && q.end == dot.start
    {
        let qual = cache.dialect.fold(&src[q.start..q.end]);
        let key = cache.schema_key(&qual);
        if let Some(r) = refs.iter().find(|r| r.alias.as_deref() == Some(qual.as_str())) {
            push_columns(&mut out, src, &stmt, cache, r);
        } else if cache.schemas.contains(&key) {
            push_objects(&mut out, cache, &key);
        } else if cache.dialect == Dialect::Snowflake && cache.databases.contains(&qual) {
            push_db_schemas(&mut out, cache, &qual);
        } else if let Some(r) = refs.iter().find(|r| r.table == qual) {
            push_columns(&mut out, src, &stmt, cache, r);
        }
    } else if let Some(w) = before.last().filter(|t| t.kind == TokKind::Word)
        && TABLE_CONTEXT.contains(&ident_text(&src[w.start..w.end]).as_str())
    {
        for s in &cache.search_path {
            push_objects(&mut out, cache, s);
        }
        for s in &cache.schemas {
            // On Snowflake, only the schemas of the current database, without the `DB.` prefix.
            let label = match (cache.dialect, cache.current_db()) {
                (Dialect::Snowflake, Some(db)) => match s.strip_prefix(db).and_then(|r| r.strip_prefix('.')) {
                    Some(rest) => rest.to_string(),
                    None => continue,
                },
                _ => s.clone(),
            };
            out.candidates.push(Candidate { insert: cache.dialect.quote(&label), label, kind: CandidateKind::Schema, detail: None });
        }
        if cache.dialect == Dialect::Snowflake {
            out.candidates.extend(cache.visible_databases().map(|d| Candidate { label: d.clone(), insert: cache.dialect.quote(d), kind: CandidateKind::Database, detail: None }));
        }
    } else {
        for r in &refs {
            push_columns(&mut out, src, &stmt, cache, r);
        }
        let extra = match cache.dialect {
            Dialect::Postgres => PG_KEYWORDS,
            Dialect::Snowflake => SF_KEYWORDS,
        };
        for k in KEYWORDS.iter().chain(extra) {
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

/// Table/view under the cursor (⌘-click in the editor): (name snippet, schema key in the cache,
/// name). A name with 1 or 2 parts only counts if the catalog knows it (otherwise `alias.column` would become
/// link); `DB.SCHEMA.TABLE` on Snowflake always counts.
pub fn relation_at(src: &str, cursor: usize, cache: &MetaCache) -> Option<(Range<usize>, String, String)> {
    let toks = tokenize(src);
    let is_name = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent);
    // Dot attached to both names: `a.b`, not a loose `a . b`.
    let joined = |l: &Token, dot: &Token, r: &Token| dot.kind == TokKind::Punct && &src[dot.start..dot.end] == "." && l.end == dot.start && dot.end == r.start;
    let i = toks.iter().position(|t| is_name(t) && t.start <= cursor && cursor < t.end)?;
    let (mut a, mut b) = (i, i);
    while a >= 2 && is_name(&toks[a - 2]) && joined(&toks[a - 2], &toks[a - 1], &toks[a]) {
        a -= 2;
    }
    while b + 2 < toks.len() && is_name(&toks[b + 2]) && joined(&toks[b], &toks[b + 1], &toks[b + 2]) {
        b += 2;
    }
    let parts: Vec<String> = (a..=b).step_by(2).map(|k| cache.dialect.fold(&src[toks[k].start..toks[k].end])).collect();
    let known = |s: &str, t: &str| cache.objects.get(s).is_some_and(|o| o.contains_relation(t));
    let (schema, table) = match parts.as_slice() {
        [t] => (resolve_schema(cache, t), t.clone()),
        [s, t] => (cache.schema_key(s), t.clone()),
        [db, s, t] if cache.dialect == Dialect::Snowflake => return Some((toks[a].start..toks[b].end, format!("{db}.{s}"), t.clone())),
        _ => return None,
    };
    known(&schema, &table).then(|| (toks[a].start..toks[b].end, schema, table))
}

/// What the editor hover shows: a table/view, or a column of one of the statement's tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverTarget {
    Table {
        range: Range<usize>,
        schema: String,
        table: String,
    },
    Column {
        range: Range<usize>,
        schema: String,
        table: String,
        column: ColumnMeta,
    },
    /// Column lists the cache doesn't have yet: request them and ask again.
    Missing(Vec<MetaRequest>),
}

/// Table or column under the cursor. A column resolves through the statement's FROM/JOIN tables
/// (by alias or name when qualified); ambiguous or unknown names give None rather than a guess.
pub fn hover_at(src: &str, cursor: usize, cache: &MetaCache) -> Option<HoverTarget> {
    if let Some((range, schema, table)) = relation_at(src, cursor, cache) {
        return Some(HoverTarget::Table { range, schema, table });
    }
    let toks = tokenize(src);
    let is_name = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent);
    let i = toks.iter().position(|t| is_name(t) && t.start <= cursor && cursor < t.end)?;
    let begin = toks[..i].iter().rposition(|t| t.kind == TokKind::Semicolon).map_or(0, |k| k + 1);
    let end = toks[i..].iter().position(|t| t.kind == TokKind::Semicolon).map_or(toks.len(), |k| i + k);
    let stmt: Vec<Token> = toks[begin..end].iter().copied().filter(|t| t.kind != TokKind::Comment).collect();
    // CTE names (`x as (`) aren't tables: their columns come from the real tables below them.
    let ctes: Vec<String> = stmt
        .windows(3)
        .filter(|w| is_name(&w[0]) && ident_text(&src[w[1].start..w[1].end]) == "as" && &src[w[2].start..w[2].end] == "(")
        .map(|w| cache.dialect.fold(&src[w[0].start..w[0].end]))
        .collect();
    let refs: Vec<TableRef> = table_refs(src, &stmt, cache.dialect).into_iter().filter(|r| r.sub.is_none() && (r.schema.is_some() || !ctes.contains(&r.table))).collect();
    let tok = toks[i];
    let name = cache.dialect.fold(&src[tok.start..tok.end]);
    // `q.col`: only the table with that alias (or name).
    let dot = |k: usize| toks[k].kind == TokKind::Punct && &src[toks[k].start..toks[k].end] == "." && toks[k].end == tok.start;
    let qual = (i >= 2 && dot(i - 1) && is_name(&toks[i - 2]) && toks[i - 2].end == toks[i - 1].start).then(|| cache.dialect.fold(&src[toks[i - 2].start..toks[i - 2].end]));
    let named = |q: &String| refs.iter().filter(|r| r.alias.as_ref() == Some(q) || (r.alias.is_none() && &r.table == q)).collect::<Vec<_>>();
    // A qualifier that names no real table (a subquery or CTE alias) searches all of them, like no qualifier.
    let scoped: Vec<&TableRef> = match qual.as_ref().map(named) {
        Some(t) if !t.is_empty() => t,
        _ => refs.iter().collect(),
    };
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for r in scoped {
        let schema = r.schema.as_deref().map(|s| cache.schema_key(s)).unwrap_or_else(|| resolve_schema(cache, &r.table));
        match cache.columns.get(&(schema.clone(), r.table.clone())) {
            Some(cols) => found.extend(cols.iter().filter(|c| c.name == name).map(|c| (schema.clone(), r.table.clone(), c.clone()))),
            None => missing.push(MetaRequest::Columns(schema, r.table.clone())),
        }
    }
    match (found.len(), missing.is_empty()) {
        (1, true) => found.pop().map(|(schema, table, column)| HoverTarget::Column { range: tok.start..tok.end, schema, table, column }),
        (0 | 1, false) => Some(HoverTarget::Missing(missing)),
        _ => None,
    }
}

fn resolve_schema(cache: &MetaCache, table: &str) -> String {
    cache.search_path.iter().find(|s| cache.objects.get(*s).is_some_and(|o| o.contains_relation(table))).or(cache.search_path.first()).cloned().unwrap_or_else(|| "public".into())
}

/// Body of CTE `name` in the statement: the tokens between `name as (` and its closing `)`.
fn cte_body<'a>(src: &str, stmt: &'a [Token], name: &str, d: Dialect) -> Option<&'a [Token]> {
    let at = stmt.windows(3).position(|w| {
        matches!(w[0].kind, TokKind::Word | TokKind::QuotedIdent) && d.fold(&src[w[0].start..w[0].end]) == name && ident_text(&src[w[1].start..w[1].end]) == "as" && &src[w[2].start..w[2].end] == "("
    })?;
    let mut depth = 0;
    for (k, t) in stmt.iter().enumerate().skip(at + 2) {
        if t.kind == TokKind::Punct {
            match &src[t.start..t.end] {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&stmt[at + 3..k]);
                    }
                }
                _ => {}
            }
        }
    }
    // Still being typed: everything after the `(`.
    Some(&stmt[at + 3..])
}

fn bare_column(name: String, type_name: String) -> ColumnMeta {
    ColumnMeta { cast_type: type_name.clone(), name, type_name, nullable: true, default: None, is_pk: false, generated: false }
}

/// Columns of a FROM entry: a real table from the cache (or the request for it), or a CTE's select list.
fn ref_columns(src: &str, stmt: &[Token], cache: &MetaCache, r: &TableRef, depth: u8) -> Result<Vec<ColumnMeta>, Vec<MetaRequest>> {
    if let Some((a, b)) = r.sub {
        let (from, to) = (stmt.iter().position(|t| t.start >= a), stmt.iter().rposition(|t| t.end <= b));
        return match (from, to, depth < 4) {
            (Some(from), Some(to), true) if from <= to => cte_columns(src, stmt, &stmt[from..=to], cache, depth + 1),
            _ => Ok(Vec::new()),
        };
    }
    if r.schema.is_none()
        && depth < 4
        && let Some(body) = cte_body(src, stmt, &r.table, cache.dialect)
    {
        return cte_columns(src, stmt, body, cache, depth + 1);
    }
    let schema = r.schema.as_deref().map(|s| cache.schema_key(s)).unwrap_or_else(|| resolve_schema(cache, &r.table));
    cache.columns.get(&(schema.clone(), r.table.clone())).cloned().ok_or_else(|| vec![MetaRequest::Columns(schema, r.table.clone())])
}

/// What a CTE returns: its select list, with `*` / `t.*` expanded from the tables in its FROM.
/// Expressions without an alias are left out (their name is the database's choice).
fn cte_columns(src: &str, stmt: &[Token], body: &[Token], cache: &MetaCache, depth: u8) -> Result<Vec<ColumnMeta>, Vec<MetaRequest>> {
    use sqlparser::ast::{Expr, SelectItem, SetExpr, Statement};
    let (Some(first), Some(last)) = (body.first(), body.last()) else { return Ok(Vec::new()) };
    let d = cache.dialect;
    let fold = |i: &sqlparser::ast::Ident| if i.quote_style.is_some() { i.value.clone() } else { d.fold(&i.value) };
    let mut stmts = sqlparser::parser::Parser::parse_sql(crate::query_edit::parser_dialect(d).as_ref(), &src[first.start..last.end]).map_err(|_| Vec::new())?;
    let Some(Statement::Query(q)) = stmts.pop() else { return Ok(Vec::new()) };
    let mut set = q.body.as_ref();
    // `a union b`: the names come from the first select.
    while let SetExpr::SetOperation { left, .. } = set {
        set = left;
    }
    let SetExpr::Select(sel) = set else { return Ok(Vec::new()) };
    let refs = table_refs(src, body, d);
    let mut by_ref = Vec::new();
    let mut missing = Vec::new();
    for r in &refs {
        match ref_columns(src, stmt, cache, r, depth) {
            Ok(cols) => by_ref.push((r, cols)),
            Err(m) => missing.extend(m),
        }
    }
    let type_of = |name: &str| by_ref.iter().flat_map(|(_, c)| c).find(|c| c.name == name).map(|c| c.type_name.clone()).unwrap_or_default();
    let mut out: Vec<ColumnMeta> = Vec::new();
    for item in &sel.projection {
        let found: Vec<ColumnMeta> = match item {
            SelectItem::Wildcard(_) => by_ref.iter().flat_map(|(_, c)| c.clone()).collect(),
            SelectItem::QualifiedWildcard(kind, _) => {
                let q = kind.to_string();
                let q = d.fold(q.trim_end_matches(".*").rsplit('.').next().unwrap_or_default());
                by_ref.iter().filter(|(r, _)| r.alias.as_ref() == Some(&q) || (r.alias.is_none() && r.table == q)).flat_map(|(_, c)| c.clone()).collect()
            }
            SelectItem::UnnamedExpr(Expr::Identifier(i)) => vec![bare_column(fold(i), type_of(&fold(i)))],
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(p)) => p.last().map(|i| bare_column(fold(i), type_of(&fold(i)))).into_iter().collect(),
            SelectItem::ExprWithAlias { expr, alias } => {
                let ty = match expr {
                    Expr::Identifier(i) => type_of(&fold(i)),
                    Expr::CompoundIdentifier(p) => p.last().map(|i| type_of(&fold(i))).unwrap_or_default(),
                    _ => String::new(),
                };
                vec![bare_column(fold(alias), ty)]
            }
            _ => Vec::new(),
        };
        for c in found {
            if !out.iter().any(|o| o.name == c.name) {
                out.push(c);
            }
        }
    }
    if out.is_empty() && !missing.is_empty() { Err(missing) } else { Ok(out) }
}

fn push_columns(out: &mut Completion, src: &str, stmt: &[Token], cache: &MetaCache, r: &TableRef) {
    match ref_columns(src, stmt, cache, r, 0) {
        Ok(cols) => out.candidates.extend(cols.into_iter().map(|c| Candidate {
            insert: cache.dialect.quote(&c.name),
            label: c.name,
            kind: CandidateKind::Column,
            detail: Some(c.type_name).filter(|t| !t.is_empty()),
        })),
        Err(m) => out.missing.extend(m),
    }
}

/// Schemas of a Snowflake database (without the `DB.` prefix), or the request for them.
fn push_db_schemas(out: &mut Completion, cache: &MetaCache, db: &str) {
    if !cache.loaded_dbs.contains(db) {
        out.missing.push(MetaRequest::Schemas(db.to_string()));
        return;
    }
    for s in &cache.schemas {
        if let Some(rest) = s.strip_prefix(db).and_then(|r| r.strip_prefix('.')) {
            out.candidates.push(Candidate { label: rest.to_string(), insert: cache.dialect.quote(rest), kind: CandidateKind::Schema, detail: None });
        }
    }
}

fn push_objects(out: &mut Completion, cache: &MetaCache, schema: &str) {
    let Some(o) = cache.objects.get(schema) else {
        out.missing.push(MetaRequest::Objects(schema.to_string()));
        return;
    };
    let rel = |name: &String, kind| Candidate { label: name.clone(), insert: cache.dialect.quote(name), kind, detail: None };
    out.candidates.extend(o.tables.iter().map(|t| rel(t, CandidateKind::Table)));
    out.candidates.extend(o.views.iter().chain(&o.matviews).map(|v| rel(v, CandidateKind::View)));
    out.candidates.extend(o.functions.iter().map(|f| {
        let name = f.split('(').next().unwrap_or(f);
        Candidate { label: f.clone(), insert: format!("{}(", cache.dialect.quote(name)), kind: CandidateKind::Function, detail: None }
    }));
}

/// Keeps only those containing the prefix; those starting with it come first; then by type.
fn filter_and_rank(c: &mut Vec<Candidate>, prefix: &str) {
    let mut seen = HashSet::new();
    let mut ranked: Vec<(bool, CandidateKind, Candidate)> = c
        .drain(..)
        .filter(|x| seen.insert((x.label.clone(), x.kind)))
        .filter_map(|x| {
            let lower = x.label.to_lowercase();
            lower.contains(prefix).then(|| (!lower.starts_with(prefix), x.kind, x))
        })
        .collect();
    // Stable: equal keys keep the order they were offered in.
    ranked.sort_by_key(|(later, kind, _)| (*later, *kind));
    c.extend(ranked.into_iter().take(200).map(|(_, _, x)| x));
}

/// Keywords are compared in lowercase (`word`); stored names follow the dialect (`name`).
fn table_refs(src: &str, toks: &[Token], d: Dialect) -> Vec<TableRef> {
    let raw = |t: &Token| matches!(t.kind, TokKind::Word | TokKind::QuotedIdent).then(|| &src[t.start..t.end]);
    let word = |t: &Token| raw(t).map(ident_text);
    let name = |t: &Token| raw(t).map(|r| d.fold(r));
    let punct = |t: &Token, p: &str| t.kind == TokKind::Punct && &src[t.start..t.end] == p;
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let is_ctx = toks[i].kind == TokKind::Word && matches!(word(&toks[i]).as_deref(), Some("from" | "join" | "update" | "into"));
        i += 1;
        if !is_ctx {
            continue;
        }
        while let Some(first) = toks.get(i).and_then(word) {
            if NOT_ALIAS.contains(&first.as_str()) {
                break;
            }
            let mut r = TableRef { schema: None, table: toks.get(i).and_then(name).unwrap_or(first), alias: None, sub: None };
            i += 1;
            if toks.get(i).is_some_and(|t| punct(t, "."))
                && let Some(n) = toks.get(i + 1).and_then(name)
            {
                r.schema = Some(std::mem::replace(&mut r.table, n));
                i += 2;
                // `DB.SCHEMA.TABLE` (Snowflake): the schema becomes the cache key `DB.SCHEMA`.
                if toks.get(i).is_some_and(|t| punct(t, "."))
                    && let Some(n) = toks.get(i + 1).and_then(name)
                {
                    let db = r.schema.take().unwrap_or_default();
                    r.schema = Some(format!("{db}.{}", std::mem::replace(&mut r.table, n)));
                    i += 2;
                }
            }
            if toks.get(i).and_then(word).as_deref() == Some("as") {
                i += 1;
            }
            if let Some(a) = toks.get(i).and_then(word)
                && !NOT_ALIAS.contains(&a.as_str())
            {
                r.alias = toks.get(i).and_then(name);
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
    // `from (select …) q` / `join (…) as q`: the alias stands for the subquery's select list.
    for (k, t) in toks.iter().enumerate() {
        let opens = punct(t, "(") && k > 0 && matches!(word(&toks[k - 1]).as_deref(), Some("from" | "join")) && matches!(toks.get(k + 1).and_then(word).as_deref(), Some("select" | "with"));
        if !opens {
            continue;
        }
        let mut depth = 0;
        let Some(close) = toks[k..].iter().position(|t| {
            depth += i32::from(punct(t, "(")) - i32::from(punct(t, ")"));
            depth == 0
        }) else {
            continue;
        };
        let close = k + close;
        let mut a = close + 1;
        if toks.get(a).and_then(word).as_deref() == Some("as") {
            a += 1;
        }
        if let Some(alias) = toks.get(a).and_then(|t| word(t).filter(|w| !NOT_ALIAS.contains(&w.as_str())).and(name(t))) {
            out.push(TableRef { schema: None, table: alias.clone(), alias: Some(alias), sub: Some((toks[k + 1].start, toks[close - 1].end)) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ColumnMeta, SchemaObjects};

    fn col(name: &str, ty: &str) -> ColumnMeta {
        ColumnMeta { name: name.into(), type_name: ty.into(), cast_type: ty.into(), nullable: true, default: None, is_pk: false, generated: false }
    }

    fn cache() -> MetaCache {
        let mut c = MetaCache { search_path: vec!["public".into()], schemas: vec!["public".into(), "Sales".into()], ..Default::default() };
        c.objects.insert(
            "public".into(),
            SchemaObjects {
                tables: vec!["Order Items".into(), "orders".into(), "users".into()],
                views: vec!["active_users".into()],
                matviews: vec![],
                functions: vec!["calc_total(integer)".into()],
                procedures: vec![],
                tasks: vec![],
                pipes: vec![],
                stages: vec![],
            },
        );
        c.columns.insert(("public".into(), "users".into()), vec![col("id", "integer"), col("email", "text")]);
        c
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.candidates.iter().map(|x| x.label.as_str()).collect()
    }

    fn hovered(src: &str, at: &str, cache: &MetaCache) -> Option<HoverTarget> {
        hover_at(src, src.find(at).unwrap() + 1, cache)
    }

    #[test]
    fn hover_finds_the_table_and_the_column_through_aliases() {
        let mut c = cache();
        c.columns.insert(("public".into(), "orders".into()), vec![col("id", "integer"), col("total", "numeric")]);
        let src = "select u.email, o.id, total from users u join orders o on o.id = u.id";
        assert!(matches!(hovered(src, "email", &c), Some(HoverTarget::Column { table, column, .. }) if table == "users" && column.name == "email"));
        assert!(matches!(hovered(src, "id, total", &c), Some(HoverTarget::Column { table, .. }) if table == "orders"), "o.id is the orders one");
        assert!(matches!(hovered(src, "total", &c), Some(HoverTarget::Column { table, .. }) if table == "orders"), "unqualified, only orders has it");
        assert!(matches!(hovered(src, "orders", &c), Some(HoverTarget::Table { table, .. }) if table == "orders"));
        assert_eq!(hovered("select id from users join orders on true", "id ", &c), None, "id is in both: no guess");
        assert_eq!(hovered(src, "select", &c), None, "keywords are nothing");
    }

    #[test]
    fn hover_resolves_snowflake_three_part_names() {
        let mut c = MetaCache { dialect: Dialect::Snowflake, search_path: vec!["ANALYTICS.PUBLIC".into()], ..Default::default() };
        c.columns.insert(("MARTS_PROD.MART_AWS".into(), "FCT_AWS_COST_USAGE_DAILY".into()), vec![col("USAGE_DATE", "DATE")]);
        let src = "select * from MARTS_PROD.MART_AWS.FCT_AWS_COST_USAGE_DAILY where usage_date = '2026-";
        let hit = hovered(src, "usage_date", &c);
        assert!(matches!(hit, Some(HoverTarget::Column { ref schema, ref column, .. }) if schema == "MARTS_PROD.MART_AWS" && column.name == "USAGE_DATE"), "{hit:?}");
        assert!(matches!(hovered(src, "FCT_AWS", &c), Some(HoverTarget::Table { .. })));
    }

    #[test]
    fn hover_sees_through_ctes_and_subqueries() {
        let mut c = MetaCache { dialect: Dialect::Snowflake, search_path: vec!["ANALYTICS.PUBLIC".into()], ..Default::default() };
        let k = |t: &str| ("MARTS_PROD.MART_AWS".to_string(), t.to_string());
        c.columns.insert(k("FCT"), vec![col("USAGE_ACCOUNT_ID", "TEXT"), col("USAGE_DATE", "DATE"), col("COST", "NUMBER")]);
        c.columns.insert(k("DIM"), vec![col("ACCOUNT_ID", "TEXT"), col("ACCOUNT_NAME", "TEXT"), col("COST", "NUMBER")]);
        let at = |src: &str, at: &str| match hover_at(src, src.find(at).unwrap() + 1, &c) {
            Some(HoverTarget::Column { table, column, .. }) => format!("{table}.{}", column.name),
            other => format!("{other:?}"),
        };
        let q = "select *, USAGE_ACCOUNT_ID from MARTS_PROD.MART_AWS.FCT u left join MARTS_PROD.MART_AWS.DIM a on u.USAGE_ACCOUNT_ID = a.ACCOUNT_ID";
        assert_eq!(at(q, "USAGE_ACCOUNT_ID from"), "FCT.USAGE_ACCOUNT_ID");
        let off = q.find("a.ACCOUNT_ID").unwrap() + 3;
        assert!(matches!(hover_at(q, off, &c), Some(HoverTarget::Column { ref table, .. }) if table == "DIM"), "a.ACCOUNT_ID is the dimension's");
        assert_eq!(at("select u.COST from MARTS_PROD.MART_AWS.FCT u join MARTS_PROD.MART_AWS.DIM a on true", "COST"), "FCT.COST");
        assert_eq!(at("select COST from MARTS_PROD.MART_AWS.FCT u join MARTS_PROD.MART_AWS.DIM a on true", "COST"), "None", "in both: no guess");
        assert_eq!(at("with x as (select USAGE_DATE from MARTS_PROD.MART_AWS.FCT) select USAGE_DATE from x", "USAGE_DATE from x"), "FCT.USAGE_DATE");
        assert_eq!(at("select s.ACCOUNT_NAME from (select ACCOUNT_NAME from MARTS_PROD.MART_AWS.DIM) s", "ACCOUNT_NAME from ("), "DIM.ACCOUNT_NAME");
    }

    #[test]
    fn completes_subquery_columns_through_its_alias() {
        let mut c = MetaCache { dialect: Dialect::Snowflake, search_path: vec!["ANALYTICS.PUBLIC".into()], ..Default::default() };
        c.columns.insert(("MARTS_PROD.MART_CLAUDE".into(), "CLAUDE_USAGE_REPORT".into()), vec![col("PRODUCT", "TEXT"), col("TOKENS", "NUMBER")]);
        let src = "select PRODUCT, count(*), sum(q.) as n\nfrom (\nSELECT\n  *\nFROM\n  \"MARTS_PROD\".\"MART_CLAUDE\".\"CLAUDE_USAGE_REPORT\"\n) q\ngroup by 1\norder by 2 desc;";
        let comp = complete(src, src.find("q.)").unwrap() + 2, &c);
        assert_eq!(labels(&comp), ["PRODUCT", "TOKENS"], "{:?}", comp.missing);
        assert!(!comp.missing.iter().any(|m| matches!(m, MetaRequest::Columns(_, t) if t == "Q")), "no table called Q is requested");
        let src = "select x. from MARTS_PROD.MART_CLAUDE.CLAUDE_USAGE_REPORT r join (select PRODUCT as p from MARTS_PROD.MART_CLAUDE.CLAUDE_USAGE_REPORT) as x on true";
        assert_eq!(labels(&complete(src, src.find("x. ").unwrap() + 2, &c)), ["P"]);
    }

    #[test]
    fn completes_cte_columns_with_star_expanded() {
        let mut c = MetaCache { dialect: Dialect::Snowflake, search_path: vec!["ANALYTICS.PUBLIC".into()], ..Default::default() };
        let k = |t: &str| ("MARTS_PROD.MART_AWS".to_string(), t.to_string());
        c.columns.insert(k("FCT"), vec![col("USAGE_ACCOUNT_ID", "TEXT"), col("USAGE_DATE", "DATE")]);
        c.columns.insert(k("DIM"), vec![col("ACCOUNT_ID", "TEXT"), col("ACCOUNT_NAME", "TEXT")]);
        let src = "with x as (\nselect *, USAGE_ACCOUNT_ID, a.ACCOUNT_NAME as name, upper(ACCOUNT_ID)\nfrom MARTS_PROD.MART_AWS.FCT u\nleft join MARTS_PROD.MART_AWS.DIM a on u.USAGE_ACCOUNT_ID = a.ACCOUNT_ID\nwhere u.USAGE_DATE = '2026-10-15' and ACCOUNT_NAME\nlimit 100)\nselect *, x. from x;";
        let comp = complete(src, src.find("x. from").unwrap() + 2, &c);
        assert_eq!(labels(&comp), ["USAGE_ACCOUNT_ID", "USAGE_DATE", "ACCOUNT_ID", "ACCOUNT_NAME", "NAME"], "{:?}", comp.missing);
        assert!(!comp.missing.iter().any(|m| matches!(m, MetaRequest::Columns(..))), "no table called X is requested: {:?}", comp.missing);
        // Only some columns, one CTE on top of another.
        let src = "with a as (select USAGE_DATE as d from MARTS_PROD.MART_AWS.FCT), b as (select d from a) select b. from b";
        assert_eq!(labels(&complete(src, src.len() - 7, &c)), ["D"]);
    }

    #[test]
    fn hover_asks_for_columns_it_does_not_have() {
        let src = "select total from orders";
        assert_eq!(hovered(src, "total", &cache()), Some(HoverTarget::Missing(vec![MetaRequest::Columns("public".into(), "orders".into())])));
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

    fn snowflake_cache() -> MetaCache {
        let mut c = MetaCache { dialect: crate::dialect::Dialect::Snowflake, ..Default::default() };
        c.search_path = vec!["DB.PUBLIC".into()];
        c.schemas = vec!["DB.PUBLIC".into(), "DB.RAW".into()];
        c.objects.insert("DB.PUBLIC".into(), SchemaObjects { tables: vec!["ORDERS".into(), "Order Items".into()], ..Default::default() });
        c.objects.insert("DB.RAW".into(), SchemaObjects { tables: vec!["EVENTS".into()], ..Default::default() });
        c.columns.insert(("DB.PUBLIC".into(), "ORDERS".into()), vec![col("ID", "NUMBER(38,0)"), col("status", "VARCHAR")]);
        c
    }

    #[test]
    fn relation_under_the_cursor() {
        let c = snowflake_cache();
        let src = "select o.id from orders o join raw.events e on 1=1 join X.Y.Z;";
        let at = |needle: &str| relation_at(src, src.find(needle).unwrap(), &c).map(|(r, s, t)| (src[r].to_string(), s, t));
        assert_eq!(at("orders"), Some(("orders".into(), "DB.PUBLIC".into(), "ORDERS".into())));
        assert_eq!(at("events"), Some(("raw.events".into(), "DB.RAW".into(), "EVENTS".into())), "whole name underlined");
        assert_eq!(at("raw."), Some(("raw.events".into(), "DB.RAW".into(), "EVENTS".into())));
        assert_eq!(at("X.Y"), Some(("X.Y.Z".into(), "X.Y".into(), "Z".into())), "3 parts count without a catalog");
        assert_eq!(at("o.id"), None, "alias.column is not a table");
        assert_eq!(at("select"), None);
        assert_eq!(relation_at("from orders", 11, &c), None, "end of text");
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
    fn snowflake_requests_schemas_of_current_database_when_unknown() {
        let mut c = snowflake_cache();
        c.schemas.clear();
        let src = "select * from raw.";
        let got = complete(src, src.len(), &c);
        assert!(got.missing.contains(&MetaRequest::Schemas("DB".into())), "{:?}", got.missing);
        let src = "select * from ";
        assert!(complete(src, src.len(), &c).missing.contains(&MetaRequest::Schemas("DB".into())));
        c.loaded_dbs.insert("DB".into());
        assert!(!complete(src, src.len(), &c).missing.contains(&MetaRequest::Schemas("DB".into())));
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

    #[test]
    fn snowflake_completes_three_part_names() {
        let mut c = snowflake_cache();
        c.databases = vec!["DB".into(), "LEARN".into()];
        c.loaded_dbs.insert("DB".into());
        // `DB.` of another database: asks for its schemas, then lists.
        let src = "select * from LEARN.";
        assert!(complete(src, src.len(), &c).missing.contains(&MetaRequest::Schemas("LEARN".into())));
        c.schemas.push("LEARN.STAGE".into());
        c.loaded_dbs.insert("LEARN".into());
        assert_eq!(complete(src, src.len(), &c).candidates.iter().map(|x| x.insert.as_str()).collect::<Vec<_>>(), vec!["STAGE"]);
        // `DB.SCHEMA.`: schema objects.
        let src = "select * from learn.stage.";
        assert_eq!(complete(src, src.len(), &c).missing, vec![MetaRequest::Objects("LEARN.STAGE".into())]);
        c.objects.insert("LEARN.STAGE".into(), SchemaObjects { tables: vec!["MENU".into()], ..Default::default() });
        assert_eq!(labels(&complete(src, src.len(), &c)), vec!["MENU"]);
        // Columns of a table with a three-part name.
        let src = "select m. from LEARN.STAGE.MENU m";
        assert_eq!(complete(src, "select m.".len(), &c).missing, vec![MetaRequest::Columns("LEARN.STAGE".into(), "MENU".into())]);
        // After `from`, databases also show up.
        let src = "select * from ";
        let dbs: Vec<String> = complete(src, src.len(), &c).candidates.iter().filter(|x| x.kind == CandidateKind::Database).map(|x| x.label.clone()).collect();
        assert_eq!(dbs, vec!["DB".to_string(), "LEARN".to_string()]);
        // The connection filter hides databases from the list, but a qualified `LEARN.` still works.
        c.db_filter = vec!["d*".into()];
        let dbs: Vec<String> = complete(src, src.len(), &c).candidates.iter().filter(|x| x.kind == CandidateKind::Database).map(|x| x.label.clone()).collect();
        assert_eq!(dbs, vec!["DB".to_string()]);
        let src = "select * from LEARN.";
        assert_eq!(complete(src, src.len(), &c).candidates.iter().map(|x| x.insert.as_str()).collect::<Vec<_>>(), vec!["STAGE"]);
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
    fn keywords_follow_the_dialect() {
        let pg = labels(&complete("select 1 ", 9, &cache())).into_iter().map(String::from).collect::<Vec<_>>();
        let sf = labels(&complete("select 1 ", 9, &snowflake_cache())).into_iter().map(String::from).collect::<Vec<_>>();
        assert!(pg.contains(&"RETURNING".into()) && !pg.contains(&"QUALIFY".into()));
        assert!(sf.contains(&"QUALIFY".into()) && sf.contains(&"USE WAREHOUSE".into()) && !sf.contains(&"RETURNING".into()));
        assert!(pg.contains(&"SELECT".into()) && sf.contains(&"SELECT".into()));
    }

    #[test]
    fn no_completion_inside_strings_or_comments() {
        let src = "select 'abc' -- fro";
        assert!(complete(src, src.len(), &cache()).candidates.is_empty());
        assert!(complete("select 'abc'", 9, &cache()).candidates.is_empty());
    }

    #[test]
    fn no_completion_inside_unterminated_literals() {
        for src in ["select 'fro", "select * from \"Ord", "select $$ fro", "select /* fro"] {
            assert!(complete(src, src.len(), &cache()).candidates.is_empty(), "{src}");
        }
        // closed: completes normally after it
        let src = "select 'x' from us";
        assert_eq!(labels(&complete(src, src.len(), &cache()))[0], "users");
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
        let refs = table_refs(src, &toks, Dialect::Postgres);
        assert_eq!(
            refs,
            vec![
                TableRef { schema: Some("public".into()), table: "users".into(), alias: Some("u".into()), sub: None },
                TableRef { schema: None, table: "orders".into(), alias: Some("o".into()), sub: None },
                TableRef { schema: Some("Sales".into()), table: "Deals".into(), alias: Some("d".into()), sub: None },
            ]
        );
    }
}
