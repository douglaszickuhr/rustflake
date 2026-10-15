use std::ops::Range;

use crate::lexer::{TokKind, tokenize};

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

fn words(stmt: &str) -> impl Iterator<Item = String> + '_ {
    tokenize(stmt).into_iter().filter(|t| t.kind == TokKind::Word).map(move |t| stmt[t.start..t.end].to_lowercase())
}

/// BEGIN / START TRANSACTION / SAVEPOINT: o app roda só em autocommit.
pub fn is_transaction_control(stmt: &str) -> bool {
    matches!(words(stmt).next().as_deref(), Some("begin" | "start" | "savepoint"))
}

/// Só lê dados? Leituras podem ficar paginadas num portal aberto; escritas são concluídas
/// e confirmadas na hora, mesmo com RETURNING.
pub fn is_read_only_query(stmt: &str) -> bool {
    let mut w = words(stmt);
    match w.next().as_deref() {
        Some("select" | "values" | "table" | "show" | "explain") => true,
        Some("with") => !w.any(|x| matches!(x.as_str(), "insert" | "update" | "delete" | "merge")),
        _ => false,
    }
}

/// Statements que o EXPLAIN aceita: consultas e DML.
pub fn explainable(stmt: &str) -> bool {
    matches!(words(stmt).next().as_deref(), Some("select" | "with" | "insert" | "update" | "delete" | "merge" | "values" | "table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explainable_accepts_queries_and_dml_only() {
        for s in ["select 1", "/* c */ WITH a AS (select 1) select * from a", "insert into t values (1)", "update t set a = 1", "delete from t", "merge into t using s on true when matched then delete", "values (1)", "table t"] {
            assert!(explainable(s), "{s}");
        }
        for s in ["create table t (a int)", "drop table t", "alter table t add b int", "begin", "vacuum", "", "explain select 1"] {
            assert!(!explainable(s), "{s}");
        }
    }

    #[test]
    fn classifies_transaction_control_and_reads() {
        assert!(is_transaction_control("/* c */ BEGIN"));
        assert!(is_transaction_control("start transaction"));
        assert!(!is_transaction_control("do $$ begin end $$"));
        assert!(is_read_only_query("select 1"));
        assert!(is_read_only_query("with a as (select 1) select * from a"));
        assert!(!is_read_only_query("with a as (delete from t returning *) select * from a"));
        assert!(!is_read_only_query("update t set v = 1 returning id"));
        assert!(!is_read_only_query("insert into t values (1) returning *"));
    }

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
