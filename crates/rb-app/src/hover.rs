//! Editor hover: a column shows its type, table and a few sample values; a table shows its facts.
//! Values come from a small query (cached for the session), fetched only after the pointer rests.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::HoverProvider;
use gpui_kit::component::{Rope, RopeExt as _};
use gpui_kit::*;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
use rb_core::complete::{HoverTarget, hover_at};
use rb_core::decode::one_line;
use rb_core::dialect::Dialect;
use rb_core::explain::short_count;
use rb_core::metadata::ColumnMeta;
use rb_core::table_info;
use rb_core::user_view;
use rust_i18n::t;

use crate::Db;
use crate::meta::{ConnTarget, MetaStore};

/// The editor already waits 150 ms; this adds to it before any query, so passing over names costs nothing.
const REST: Duration = Duration::from_millis(450);
/// How long the hover waits for values; slower ones land in the cache for the next hover.
const VALUES_WAIT: Duration = Duration::from_secs(4);
const SAMPLE_ROWS: usize = 5000;
const SHOWN_VALUES: usize = 8;

type Rows = Vec<Vec<Option<String>>>;
/// Rows and which of the queries gave them (0 = the first).
type Answer = Result<(usize, Rows), String>;
/// First query text → its answer (None while running), for the whole session.
type Fetched = Rc<RefCell<HashMap<String, Option<Answer>>>>;

pub struct SqlHover {
    meta: Entity<MetaStore>,
    conn: ConnTarget,
    fetched: Fetched,
}

impl SqlHover {
    pub fn new(meta: Entity<MetaStore>, conn: ConnTarget) -> Self {
        SqlHover { meta, conn, fetched: Fetched::default() }
    }
}

/// Most common values of a column in a sample: random blocks first (cheap on any size; a small table
/// may give nothing), then the first rows. Rows are (value, count), most frequent first.
fn value_queries(col: &str, from: &str) -> Vec<String> {
    let top = |src: String| format!("select v, count(*) as n from ({src}) s group by v order by n desc limit {SHOWN_VALUES}");
    vec![
        top(format!("select {col} as v from {from} tablesample system (1) where {col} is not null limit {SAMPLE_ROWS}")),
        top(format!("select {col} as v from {from} where {col} is not null limit {SAMPLE_ROWS}")),
    ]
}

/// The answer is good enough to stop trying the next query.
fn enough(rows: &Rows, last: bool) -> bool {
    last || rows.len() >= 2
}

fn column_markdown(c: &ColumnMeta, table: &str, values: Option<&Answer>) -> String {
    let mut md = format!("**{}** `{}` · {table}", c.name, c.type_name);
    if !c.nullable {
        md += &format!(" · {}", t!("hover.not_null"));
    }
    md += "\n\n";
    md += &match values {
        None => format!("_{}_", t!("hover.values_loading")),
        Some(Err(e)) => format!("_{}_", one_line(e, 120)),
        Some(Ok((_, rows))) if rows.is_empty() => format!("_{}_", t!("hover.no_values")),
        Some(Ok((which, rows))) => {
            let lines: Vec<String> = rows
                .iter()
                .map(|r| {
                    let v = r.first().cloned().flatten().unwrap_or_default();
                    let n = r.get(1).cloned().flatten().unwrap_or_default();
                    format!("`{}` ×{n}", one_line(&v, 40).replace('`', "'"))
                })
                .collect();
            let from = if *which == 0 { t!("hover.from_sample") } else { t!("hover.from_first", n = SAMPLE_ROWS) };
            format!("{}\n\n_{from}_", lines.join("  \n"))
        }
    };
    md
}

fn table_markdown(schema: &str, table: &str, facts: Option<&Answer>, n_cols: Option<usize>) -> String {
    let mut md = format!("**{table}** · {schema}");
    let mut parts = Vec::new();
    match facts {
        Some(Ok((_, rows))) => {
            let names: Vec<String> = ["table_type", "row_count", "bytes", "created", "last_altered", "comment"].map(String::from).into();
            if let Some(f) = table_info::parse_facts(&names, rows) {
                parts.push(f.kind.to_lowercase());
                parts.extend(f.rows.map(|r| t!("tv.rows", n = short_count(r as f64)).to_string()));
                parts.extend(f.bytes.map(|b| format!("{}B", short_count(b as f64))));
                parts.extend(f.last_altered.as_deref().and_then(user_view::parse_ts).map(|s| t!("tv.changed", ago = crate::ui::ago(s)).to_string()));
                if let Some(c) = f.comment.filter(|c| !c.is_empty()) {
                    md += &format!("\n\n{}", one_line(&c, 300));
                }
            }
        }
        Some(Err(e)) => parts.push(one_line(e, 120)),
        None => {}
    }
    parts.extend(n_cols.map(|n| t!("hover.columns", n = n).to_string()));
    if !parts.is_empty() {
        // Facts go right under the name, before any comment.
        let (head, rest) = md.split_once("\n\n").map_or((md.as_str(), ""), |(h, r)| (h, r));
        md = format!("{head}\n\n{}{}", parts.join(" · "), if rest.is_empty() { String::new() } else { format!("\n\n{rest}") });
    }
    md
}

/// Runs `queries` once per session, in order, until one gives [`enough`]; the answer lands in `fetched`
/// under the first query's text.
fn start_fetch(fetched: &Fetched, conn: &ConnTarget, queries: &[String], cx: &mut App) {
    let Some(key) = queries.first().cloned() else { return };
    if fetched.borrow().contains_key(&key) {
        return;
    }
    fetched.borrow_mut().insert(key.clone(), None);
    let (fetched, conn, queries, engine) = (fetched.clone(), conn.clone(), queries.to_vec(), cx.global::<Db>().0.clone());
    cx.spawn(async move |_| {
        let answer = async {
            let s = engine.connect(&conn.cfg, conn.creds.clone()).await?;
            let mut last = (0, Vec::new());
            for (i, sql) in queries.iter().enumerate() {
                let (_, rows) = s.query_named(sql).await?;
                last = (i, rows);
                if enough(&last.1, i + 1 == queries.len()) {
                    break;
                }
            }
            Ok::<_, rb_core::DbError>(last)
        }
        .await;
        if let Err(e) = &answer {
            tracing::debug!("hover query failed: {e}");
        }
        fetched.borrow_mut().insert(key, Some(answer.map_err(|e| e.to_string())));
    })
    .detach();
}

impl HoverProvider for SqlHover {
    fn hover(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<anyhow::Result<Option<Hover>>> {
        let (meta, conn, fetched, text) = (self.meta.clone(), self.conn.clone(), self.fetched.clone(), text.clone());
        let src = text.to_string();
        cx.spawn(async move |cx| {
            // Column lists may still be missing: ask for them and look again (a few rounds at most).
            let mut target = None;
            for _ in 0..3 {
                match cx.update(|cx| hover_at(&src, offset, &meta.read(cx).cache)) {
                    Some(HoverTarget::Missing(reqs)) => {
                        cx.update(|cx| meta.update(cx, |m, cx| reqs.into_iter().for_each(|r| m.request(r, cx))));
                        let started = Instant::now();
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                        while cx.update(|cx| meta.read(cx).loading()) && started.elapsed() < Duration::from_secs(3) {
                            cx.background_executor().timer(Duration::from_millis(50)).await;
                        }
                    }
                    other => {
                        target = other;
                        break;
                    }
                }
            }
            let Some(target) = target else { return Ok(None) };
            let dialect = cx.update(|cx| meta.read(cx).cache.dialect);
            let (range, sql) = match &target {
                HoverTarget::Column { range, schema, table, column } => (range.clone(), value_queries(&dialect.quote(&column.name), &dialect.qualified(schema, table))),
                HoverTarget::Table { range, schema, table } => {
                    // Facts come from Snowflake's information_schema; Postgres shows what the cache has.
                    let sql = (dialect == Dialect::Snowflake).then(|| schema.split_once('.').map(|(db, s)| table_info::facts_sql(db, s, table))).flatten();
                    (range.clone(), sql.into_iter().collect())
                }
                HoverTarget::Missing(_) => return Ok(None),
            };
            let mut result = None;
            if let Some(key) = sql.first() {
                let cached = fetched.borrow().get(key).cloned().flatten();
                result = cached;
                if result.is_none() {
                    // Still resting on the name? (Moving away drops this task before any query.)
                    cx.background_executor().timer(REST).await;
                    cx.update(|cx| start_fetch(&fetched, &conn, &sql, cx));
                    let started = Instant::now();
                    while started.elapsed() < VALUES_WAIT {
                        if let Some(Some(r)) = fetched.borrow().get(key) {
                            result = Some(r.clone());
                            break;
                        }
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                }
            }
            let md = match &target {
                HoverTarget::Column { table, column, .. } => column_markdown(column, table, result.as_ref()),
                HoverTarget::Table { schema, table, .. } => {
                    let n_cols = cx.update(|cx| meta.read(cx).cache.columns.get(&(schema.clone(), table.clone())).map(Vec::len));
                    table_markdown(schema, table, result.as_ref(), n_cols)
                }
                HoverTarget::Missing(_) => unreachable!("returned above"),
            };
            let range = lsp_types::Range { start: text.offset_to_position(range.start), end: text.offset_to_position(range.end) };
            Ok(Some(Hover { contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: md }), range: Some(range) }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{column_markdown, enough, value_queries};
    use rb_core::metadata::ColumnMeta;

    #[test]
    fn samples_random_blocks_then_the_first_rows() {
        let q = value_queries("C", "T");
        assert!(q[0].contains("from T tablesample system (1) where C is not null"), "{}", q[0]);
        assert!(q[1].contains("from T where C is not null limit 5000"), "{}", q[1]);
        assert!(q.iter().all(|q| q.ends_with("group by v order by n desc limit 8")));
        let one = vec![vec![Some("x".to_string()), Some("9".to_string())]];
        assert!(!enough(&one, false), "one value from the sample: try the first rows too");
        assert!(enough(&one, true), "the last query's answer is the answer");
    }

    #[test]
    fn column_card_lists_values_with_counts_and_source() {
        let c = ColumnMeta { name: "USAGE_DATE".into(), type_name: "DATE".into(), cast_type: "DATE".into(), nullable: true, default: None, is_pk: false, generated: false };
        let rows = vec![vec![Some("2026-10-19".into()), Some("12".into())], vec![Some("2026-10-18".into()), Some("7".into())]];
        let md = column_markdown(&c, "FCT_AWS", Some(&Ok((0, rows))));
        assert!(md.starts_with("**USAGE_DATE** `DATE` · FCT_AWS"), "{md}");
        assert!(md.contains("`2026-10-19` ×12  \n`2026-10-18` ×7"), "{md}");
    }
}
