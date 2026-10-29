//! SQL formatter: UPPERCASE keywords, 2 spaces (fixed style).

use sqlformat::{FormatOptions, Indent, QueryParams};

use crate::lexer::{TokKind, tokenize};

/// Formats statement by statement and only accepts the result if the tokens are the same (words
/// case-insensitive; strings, identifiers, `$$` and comments byte for byte). sqlformat
/// knows neither `standard_conforming_strings` nor `$tag$` bodies: when it gets it wrong, the statement stays as it was.
pub fn format_sql(sql: &str) -> String {
    let opts = FormatOptions { indent: Indent::Spaces(2), uppercase: Some(true), lines_between_queries: 2, ..FormatOptions::default() };
    let mut chunks = Vec::new();
    let mut from = 0;
    for t in tokenize(sql).iter().filter(|t| t.kind == TokKind::Semicolon) {
        chunks.push(&sql[from..t.end]);
        from = t.end;
    }
    if !sql[from..].trim().is_empty() {
        chunks.push(&sql[from..]);
    }
    chunks
        .into_iter()
        .map(|chunk| {
            let chunk = chunk.trim();
            let out = sqlformat::format(chunk, &QueryParams::None, &opts);
            if same_tokens(chunk, &out) { glue_colons(chunk, &out) } else { chunk.to_string() }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// sqlformat reads Snowflake's `col:path` as a `:name` parameter and writes `col :path`: next to a `:`,
/// whatever was glued in the source stays glued.
fn glue_colons(src: &str, out: &str) -> String {
    let (ts, to) = (tokenize(src), tokenize(out));
    let mut res = String::with_capacity(out.len());
    let mut from = 0;
    for i in 1..to.len() {
        let colon = |t: &crate::lexer::Token, s: &str| &s[t.start..t.end] == ":";
        let glued = ts[i - 1].end == ts[i].start && (colon(&ts[i - 1], src) || colon(&ts[i], src));
        let gap = &out[to[i - 1].end..to[i].start];
        if glued && !gap.is_empty() && gap.bytes().all(|c| c == b' ') {
            res.push_str(&out[from..to[i - 1].end]);
            from = to[i].start;
        }
    }
    res.push_str(&out[from..]);
    res
}

fn same_tokens(a: &str, b: &str) -> bool {
    let (ta, tb) = (tokenize(a), tokenize(b));
    ta.len() == tb.len()
        && ta.iter().zip(&tb).all(|(x, y)| {
            let (sx, sy) = (&a[x.start..x.end], &b[y.start..y.end]);
            x.kind == y.kind && if x.kind == TokKind::Word { sx.eq_ignore_ascii_case(sy) } else { sx == sy }
        })
}

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
            ("select * from t where a = :name", ":name"),
            ("select '-- not a comment' as x", "'-- not a comment'"),
            ("select \"Order Items\".id from \"Order Items\"", "\"Order Items\""),
            ("select 1 -- end\n", "-- end"),
            ("select /* middle */ 1", "/* middle */"),
        ] {
            assert!(format_sql(src).contains(keep), "{src} → {}", format_sql(src));
        }
    }

    #[test]
    fn dollar_quoted_bodies_stay_intact() {
        let body = "$$ begin raise notice 'hi'; end $$";
        let out = format_sql(&format!("create function f() returns void language plpgsql as {body};"));
        assert!(out.contains(body), "{out}");
    }

    #[test]
    fn script_with_dollar_body_still_formats_the_other_statements() {
        let out = format_sql("-- create\ncreate function f() returns int language sql as $$ select 1 $$; select a from t");
        assert!(out.starts_with("-- create\ncreate function f() returns int language sql as $$ select 1 $$;"), "{out}");
        assert!(out.ends_with("SELECT\n  a\nFROM\n  t"), "{out}");
    }

    #[test]
    fn backslashes_in_strings_never_change_literals() {
        // sqlformat reads `\'` as an escaped quote; on Postgres (standard_conforming_strings) `'\'` is a complete string.
        for src in [r"select 'a\' as x, 'b' as y from t", r"select * from t where a like 'a\_%' escape '\' and c = 'k'", r"select E'\\' as bs, 'x' as y"] {
            let out = format_sql(src);
            for lit in ["'b'", "'k'", "'x'"] {
                if src.contains(lit) {
                    assert!(out.contains(lit), "{src} → {out}");
                }
            }
        }
    }

    #[test]
    fn tagged_dollar_bodies_stay_intact() {
        let body = "$fn$ declare r t%rowtype; begin select * into r from t; end $fn$";
        let out = format_sql(&format!("create function f() returns void language plpgsql as {body}; select 1"));
        assert!(out.contains(body), "{out}");
        assert!(out.ends_with("SELECT\n  1"), "{out}");
    }

    #[test]
    fn semicolon_inside_a_comment_does_not_split_the_script() {
        let out = format_sql("select 1 -- a;b\n; select $$ x $$");
        assert!(out.contains("-- a;b"), "{out}");
        assert!(!out.lines().any(|l| l.trim() == "b;"), "comment became code: {out}");
    }

    #[test]
    fn snowflake_paths_and_casts_stay_glued() {
        let out = format_sql("select content:_bucket_start::TIMESTAMP as b, content:actor:email::VARCHAR as e from t where a = :name");
        assert!(out.contains("content:_bucket_start::TIMESTAMP AS b"), "{out}");
        assert!(out.contains("content:actor:email::VARCHAR AS e"), "{out}");
        assert!(out.contains("= :name"), "{out}");
        assert_eq!(format_sql(&out), out);
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = format_sql("select a, b from t where x = 1; update t set a = 2");
        assert_eq!(format_sql(&once), once);
    }
}
