use std::ops::Range;

use crate::lexer::{TokKind, tokenize};

/// Splits the script into statements via the lexer, respecting strings, dollar-quotes and comments.
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

/// Statement under the cursor; if the cursor is between statements, the previous one; otherwise the first.
pub fn statement_at(src: &str, offset: usize) -> Option<Range<usize>> {
    let stmts = split_statements(src);
    stmts.iter().find(|r| r.start <= offset && offset <= r.end).or_else(|| stmts.iter().rev().find(|r| r.end < offset)).or(stmts.first()).cloned()
}

/// Words of the statement, lowercase, skipping comments and literals.
pub(crate) fn words(stmt: &str) -> impl Iterator<Item = String> + '_ {
    tokenize(stmt).into_iter().filter(|t| t.kind == TokKind::Word).map(move |t| stmt[t.start..t.end].to_lowercase())
}

/// BEGIN, COMMIT, ROLLBACK, SAVEPOINT and friends: the app runs in autocommit only.
pub fn is_transaction_control(stmt: &str) -> bool {
    matches!(words(stmt).next().as_deref(), Some("begin" | "start" | "savepoint" | "commit" | "rollback" | "end" | "abort" | "release"))
}

/// `USE …` (Snowflake context switch), even after a comment.
pub fn is_use(stmt: &str) -> bool {
    words(stmt).next().as_deref() == Some("use")
}

/// Read-only? Reads may stay paginated on an open portal; writes are completed
/// and committed right away, even with RETURNING.
pub fn is_read_only_query(stmt: &str) -> bool {
    let mut w = words(stmt);
    match w.next().as_deref() {
        Some("select" | "values" | "table" | "show" | "explain") => true,
        Some("with") => !w.any(|x| matches!(x.as_str(), "insert" | "update" | "delete" | "merge")),
        _ => false,
    }
}

/// `EXPLAIN [ANALYZE] [VERBOSE] <query>` typed by hand → (analyze, query), to show the
/// diagram. With explicit options (`EXPLAIN (FORMAT …)`, `EXPLAIN USING …`) the database's text is kept.
pub fn explain_prefix(stmt: &str) -> Option<(bool, &str)> {
    let toks: Vec<_> = tokenize(stmt).into_iter().filter(|t| t.kind != TokKind::Comment).collect();
    let word = |i: usize| toks.get(i).filter(|t| t.kind == TokKind::Word).map(|t| stmt[t.start..t.end].to_ascii_lowercase());
    if word(0)? != "explain" {
        return None;
    }
    let mut i = 1;
    let analyze = matches!(word(i).as_deref(), Some("analyze" | "analyse"));
    if analyze {
        i += 1;
    }
    if word(i).as_deref() == Some("verbose") {
        i += 1;
    }
    let inner = &stmt[toks.get(i)?.start..];
    explainable(inner).then_some((analyze, inner))
}

/// Statements EXPLAIN accepts: queries and DML.
pub fn explainable(stmt: &str) -> bool {
    matches!(words(stmt).next().as_deref(), Some("select" | "with" | "insert" | "update" | "delete" | "merge" | "values" | "table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handwritten_explain_is_recognised() {
        let src = "EXPLAIN ANALYZE\nselect task_type, count(*) from t GROUP BY 1";
        assert_eq!(explain_prefix(src), Some((true, "select task_type, count(*) from t GROUP BY 1")));
        assert_eq!(explain_prefix("explain select 1"), Some((false, "select 1")));
        assert_eq!(explain_prefix("/* c */ explain analyse verbose update t set a = 1"), Some((true, "update t set a = 1")));
        assert_eq!(explain_prefix("explain (format text) select 1"), None, "explicit options: database text");
        assert_eq!(explain_prefix("explain using text select 1"), None, "Snowflake with explicit format");
        assert_eq!(explain_prefix("explain create table t (a int)"), None);
        assert_eq!(explain_prefix("select 1"), None);
    }

    #[test]
    fn explainable_accepts_queries_and_dml_only() {
        for s in [
            "select 1",
            "/* c */ WITH a AS (select 1) select * from a",
            "insert into t values (1)",
            "update t set a = 1",
            "delete from t",
            "merge into t using s on true when matched then delete",
            "values (1)",
            "table t",
        ] {
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
        assert!(is_transaction_control("COMMIT") && is_transaction_control("rollback to savepoint a"));
        assert!(!is_transaction_control("do $$ begin end $$"));
        assert!(is_use("-- pick\nuse role x") && is_use("USE WAREHOUSE w") && !is_use("select 'use'"));
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
        assert_eq!(texts(src), vec!["create function f() returns int as $$ begin; return 1; end $$ language plpgsql", "select f()"]);
    }

    #[test]
    fn ignores_empty_and_comment_only_statements() {
        assert_eq!(texts(";; -- nothing\n; /* x */ select 1 -- end\n;"), vec!["select 1"]);
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
        assert_eq!(at(19), Some("select 2")); // right after "select 2"
        assert_eq!(at(src.len()), Some("select 2")); // blank line at the end
        assert_eq!(at(10), Some("select 1")); // blank line between the two
        assert_eq!(statement_at("   ", 1), None);
    }
}
