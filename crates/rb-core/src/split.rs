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
