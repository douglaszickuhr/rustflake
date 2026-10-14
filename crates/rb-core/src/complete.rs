use std::collections::HashSet;
use std::ops::Range;

use crate::lexer::{TokKind, Token, ident_text, is_unterminated, quote_ident, tokenize};
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
        literal && ((t.start < cursor && cursor < t.end) || (cursor == t.end && is_unterminated(src, t)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ColumnMeta, SchemaObjects};

    fn col(name: &str, ty: &str) -> ColumnMeta {
        ColumnMeta { name: name.into(), type_name: ty.into(), cast_type: ty.into(), nullable: true, default: None, is_pk: false, generated: false }
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
    fn no_completion_inside_unterminated_literals() {
        for src in ["select 'fro", "select * from \"Ord", "select $$ fro", "select /* fro"] {
            assert!(complete(src, src.len(), &cache()).candidates.is_empty(), "{src}");
        }
        // fechado: completa normalmente depois dele
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
