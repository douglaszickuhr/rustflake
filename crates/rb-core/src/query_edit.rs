//! Queries derived from a result: "count by these columns" and "filter by this value".
//!
//! Parses with `sqlparser` to edit the query where it is safe (drop the outer LIMIT, add to the
//! WHERE); when it doesn't parse or the shape is too complex, wraps the original text as a subquery.
//! Printing the tree drops the user's comments and layout, so the result goes through the formatter.

use sqlparser::ast::{Expr, GroupByExpr, Ident, Query, SelectItem, SetExpr, Statement};
use sqlparser::dialect::{Dialect as SqlDialect, PostgreSqlDialect, SnowflakeDialect};
use sqlparser::parser::Parser;

use crate::dialect::Dialect;
use crate::format::format_sql;

/// Condition on one result column, from a grid cell (the value as the grid shows it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    Eq(String),
    Ne(String),
    IsNull,
    NotNull,
}

fn parser_dialect(d: Dialect) -> Box<dyn SqlDialect> {
    match d {
        Dialect::Postgres => Box::new(PostgreSqlDialect {}),
        Dialect::Snowflake => Box::new(SnowflakeDialect {}),
    }
}

/// The text as one query, or None (several statements, DML, or something the parser doesn't know).
fn parse(sql: &str, d: Dialect) -> Option<Query> {
    let mut stmts = Parser::parse_sql(parser_dialect(d).as_ref(), sql).ok()?;
    match (stmts.pop(), stmts.is_empty()) {
        (Some(Statement::Query(q)), true) => Some(*q),
        _ => None,
    }
}

/// Without the trailing `;`, so it fits inside parentheses.
fn body(sql: &str) -> &str {
    sql.trim().trim_end_matches(';').trim_end()
}

/// The same rows without the outer LIMIT/FETCH/TOP (and ORDER BY, useless inside a subquery).
/// Unparseable text comes back as is: counting then covers only the rows the limit lets through.
pub fn without_limit(sql: &str, d: Dialect) -> String {
    let Some(mut q) = parse(sql, d) else { return body(sql).to_string() };
    q.limit_clause = None;
    q.fetch = None;
    q.order_by = None;
    if let SetExpr::Select(s) = q.body.as_mut() {
        s.top = None;
    }
    q.to_string()
}

/// `select <cols>, count(*) … group by … order by count desc` over every row of the query.
pub fn count_by(sql: &str, cols: &[String], d: Dialect) -> String {
    let names: Vec<String> = cols.iter().map(|c| d.quote(c)).collect();
    let ordinals: Vec<String> = (1..=cols.len()).map(|i| i.to_string()).collect();
    // The newline before ")" keeps a trailing `-- comment` of the original from eating it.
    format!("select {}, count(*) as n\nfrom (\n{}\n) q\ngroup by {}\norder by {} desc", names.join(", "), format_sql(&without_limit(sql, d)), ordinals.join(", "), cols.len() + 1)
}

/// Total rows, non-null and distinct values of one column, over every row of the query.
pub fn distinct(sql: &str, col: &str, d: Dialect) -> String {
    let c = d.quote(col);
    format!("select count(*) as total_rows, count({c}) as non_null, count(distinct {c}) as distinct_values\nfrom (\n{}\n) q", format_sql(&without_limit(sql, d)))
}

/// Smallest and largest value of one column, over every row of the query.
pub fn range(sql: &str, col: &str, d: Dialect) -> String {
    let c = d.quote(col);
    format!("select min({c}) as min_value, max({c}) as max_value\nfrom (\n{}\n) q", format_sql(&without_limit(sql, d)))
}

/// Does the identifier name this result column? Unquoted names fold like the database does.
fn names(i: &Ident, col: &str, d: Dialect) -> bool {
    match (i.quote_style, d) {
        (Some(_), _) => i.value == col,
        (None, Dialect::Snowflake) => i.value.to_uppercase() == col,
        (None, Dialect::Postgres) => i.value.to_lowercase() == col,
    }
}

fn is_column(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

/// What to write in the WHERE for result column `col`, when the outer SELECT is simple enough.
fn where_target(q: &Query, col: &str, d: Dialect) -> Option<String> {
    let SetExpr::Select(s) = q.body.as_ref() else { return None };
    // Grouped rows: the WHERE runs before grouping, so filter the result from outside instead.
    let grouped = !matches!(&s.group_by, GroupByExpr::Expressions(e, _) if e.is_empty());
    if grouped || s.having.is_some() || s.distinct.is_some() {
        return None;
    }
    let mut wildcard = false;
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e @ Expr::Identifier(i)) if names(i, col, d) => return Some(e.to_string()),
            SelectItem::UnnamedExpr(e @ Expr::CompoundIdentifier(p)) if p.last().is_some_and(|i| names(i, col, d)) => return Some(e.to_string()),
            // `a.x as col`: the WHERE can't see the alias, but it can use the column behind it.
            SelectItem::ExprWithAlias { expr, alias } if names(alias, col, d) => return is_column(expr).then(|| expr.to_string()),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => wildcard = true,
            _ => {}
        }
    }
    wildcard.then(|| d.quote(col))
}

fn condition(target: &str, f: &Filter) -> String {
    let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
    match f {
        // A quoted literal compares with numbers and dates too (both databases cast it).
        Filter::Eq(v) => format!("{target} = {}", lit(v)),
        Filter::Ne(v) => format!("{target} <> {}", lit(v)),
        Filter::IsNull => format!("{target} is null"),
        Filter::NotNull => format!("{target} is not null"),
    }
}

/// The condition on result column `col` alone, for a filter field over a known table.
pub fn condition_on(col: &str, f: &Filter, d: Dialect) -> String {
    condition(&d.quote(col), f)
}

/// The query keeping only the rows where `col` matches `f`. Adds to the outer WHERE when it can
/// (so LIMIT and ORDER BY still apply after the filter); otherwise filters the result from outside.
pub fn with_filter(sql: &str, col: &str, f: &Filter, d: Dialect) -> String {
    if let Some(mut q) = parse(sql, d)
        && let Some(target) = where_target(&q, col, d)
        && let Ok(cond) = Parser::new(parser_dialect(d).as_ref()).try_with_sql(&condition(&target, f)).and_then(|mut p| p.parse_expr())
        && let SetExpr::Select(s) = q.body.as_mut()
    {
        s.selection = Some(match s.selection.take() {
            // Parentheses keep an `a or b` already there from swallowing the new condition.
            Some(old) => Expr::BinaryOp { left: Box::new(Expr::Nested(Box::new(old))), op: sqlparser::ast::BinaryOperator::And, right: Box::new(cond) },
            None => cond,
        });
        return format_sql(&q.to_string());
    }
    // ponytail: the wrapped query keeps its own LIMIT, so this filters only the rows it returned.
    format!("select *\nfrom (\n{}\n) q\nwhere {}", body(sql), condition(&d.quote(col), f))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SF: Dialect = Dialect::Snowflake;

    fn flat(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
    }

    #[test]
    fn count_by_drops_the_outer_limit_and_order() {
        let sql = flat(&count_by("select * from MARTS.HUB.COMPANY order by 1 limit 100;", &["COUNTRY".into()], SF));
        assert_eq!(sql, "select country, count(*) as n from ( select * from marts.hub.company ) q group by 1 order by 2 desc");
    }

    #[test]
    fn count_by_several_columns_quotes_names_that_need_it() {
        let sql = count_by("select * from t", &["COUNTRY".into(), "Mixed Case".into()], SF);
        assert!(sql.starts_with("select COUNTRY, \"Mixed Case\", count(*) as n"), "{sql}");
        assert!(sql.ends_with("group by 1, 2\norder by 3 desc"), "{sql}");
    }

    #[test]
    fn without_limit_keeps_inner_limits_and_snowflake_syntax() {
        let sql = flat(&without_limit("select * from (select * from t limit 5) x qualify row_number() over (partition by a order by b) = 1 limit 10", SF));
        assert!(sql.contains("limit 5") && sql.contains("qualify") && !sql.contains("limit 10"), "{sql}");
        let top = flat(&without_limit("select top 10 a from t", SF));
        assert!(!top.contains("top"), "{top}");
    }

    #[test]
    fn unparseable_text_is_wrapped_as_is() {
        let weird = "select * from t match_recognize (whatever) -- note";
        assert_eq!(without_limit(weird, SF), weird);
        let sql = count_by(weird, &["A".into()], SF);
        assert!(sql.contains("-- note\n) q"), "the comment must not swallow the parenthesis: {sql}");
    }

    #[test]
    fn column_stats_cover_every_row() {
        let sql = flat(&distinct("select * from t limit 100", "COUNTRY", SF));
        assert_eq!(sql, "select count(*) as total_rows, count(country) as non_null, count(distinct country) as distinct_values from ( select * from t ) q");
        let sql = flat(&range("select * from t limit 100", "Created At", SF));
        assert_eq!(sql, "select min(\"created at\") as min_value, max(\"created at\") as max_value from ( select * from t ) q");
    }

    #[test]
    fn filter_goes_into_the_where_before_the_limit() {
        let sql = flat(&with_filter("select * from t where a = 1 or b = 2 order by c limit 100", "COUNTRY", &Filter::Eq("Germany".into()), SF));
        assert_eq!(sql, "select * from t where ( a = 1 or b = 2 ) and country = 'germany' order by c limit 100");
    }

    #[test]
    fn filter_uses_the_column_behind_an_alias_or_qualifier() {
        let sql = flat(&with_filter("select c.name as company, c.country from companies c", "COMPANY", &Filter::Ne("O'Neil".into()), SF));
        assert!(sql.contains("where c.name <> 'o''neil'"), "{sql}");
        let sql = flat(&with_filter("select c.country from companies c", "COUNTRY", &Filter::IsNull, SF));
        assert!(sql.contains("where c.country is null"), "{sql}");
    }

    #[test]
    fn grouped_or_computed_columns_filter_from_outside() {
        let grouped = flat(&with_filter("select country, count(*) as n from t group by 1", "N", &Filter::Eq("3".into()), SF));
        assert_eq!(grouped, "select * from ( select country, count(*) as n from t group by 1 ) q where n = '3'");
        let computed = flat(&with_filter("select upper(a) as x from t", "X", &Filter::NotNull, SF));
        assert!(computed.starts_with("select * from ("), "{computed}");
        let union = flat(&with_filter("select a from t union all select a from u", "A", &Filter::IsNull, SF));
        assert!(union.ends_with(") q where a is null"), "{union}");
    }

    #[test]
    fn postgres_folds_unquoted_names_to_lower_case() {
        let sql = flat(&with_filter("select Name from people", "name", &Filter::Eq("x".into()), Dialect::Postgres));
        assert!(sql.contains("where name = 'x'"), "{sql}");
    }
}
