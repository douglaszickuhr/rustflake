//! Formatador de SQL: palavras-chave em MAIÚSCULAS, 2 espaços (estilo fixo).

use sqlformat::{FormatOptions, Indent, QueryParams};

pub fn format_sql(sql: &str) -> String {
    let opts = FormatOptions { indent: Indent::Spaces(2), uppercase: Some(true), lines_between_queries: 2, ..FormatOptions::default() };
    let fmt = |s: &str| sqlformat::format(s, &QueryParams::None, &opts);
    if !sql.contains("$$") {
        return fmt(sql);
    }
    // O sqlformat quebra linhas dentro de corpos `$$`: formata statement a statement e deixa
    // como está o que tem `$$`. Cada pedaço vai do fim do anterior até o seu `;` (com comentários).
    let mut out = Vec::new();
    let mut from = 0;
    for r in crate::split::split_statements(sql) {
        let end = sql[r.end..].find(';').map_or(r.end, |i| r.end + i + 1);
        let chunk = sql[from..end].trim();
        out.push(if chunk.contains("$$") { chunk.to_string() } else { fmt(chunk) });
        from = end;
    }
    let rest = sql[from..].trim();
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out.join("\n\n")
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
    fn script_with_dollar_body_still_formats_the_other_statements() {
        let out = format_sql("-- cria\ncreate function f() returns int language sql as $$ select 1 $$; select a from t");
        assert!(out.starts_with("-- cria\ncreate function f() returns int language sql as $$ select 1 $$;"), "{out}");
        assert!(out.ends_with("SELECT\n  a\nFROM\n  t"), "{out}");
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        let once = format_sql("select a, b from t where x = 1; update t set a = 2");
        assert_eq!(format_sql(&once), once);
    }
}
