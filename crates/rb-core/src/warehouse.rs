//! Snowflake warehouses (read-only): list, configuration, who can use them, credits and usage.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::DbError;
use crate::admin::{RoleGraph, quote_ident, sql_str, unquote};
use crate::session::Session;
use crate::snowflake::field;

type Rows = [Vec<Option<String>>];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WarehouseInfo {
    pub name: String,
    pub state: String,
    pub kind: Option<String>,
    pub size: String,
    pub min_clusters: u32,
    pub max_clusters: u32,
    pub running: u64,
    pub queued: u64,
    /// Seconds; None/0 = never suspends.
    pub auto_suspend: Option<u64>,
    pub auto_resume: bool,
    pub scaling_policy: Option<String>,
    pub owner: Option<String>,
    pub comment: Option<String>,
    pub created_on: Option<String>,
    pub resumed_on: Option<String>,
    pub resource_monitor: Option<String>,
}

/// SHOW returns "null" as text in some columns.
fn opt(names: &[String], r: &[Option<String>], col: &str) -> Option<String> {
    field(names, r, col).filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("null")).map(String::from)
}

fn num<T: std::str::FromStr + Default>(names: &[String], r: &[Option<String>], col: &str) -> T {
    field(names, r, col).and_then(|v| v.parse().ok()).unwrap_or_default()
}

/// `SHOW WAREHOUSES`, ordered by name.
pub fn parse_warehouses(names: &[String], rows: &Rows) -> Vec<WarehouseInfo> {
    let mut out: Vec<WarehouseInfo> = rows
        .iter()
        .filter_map(|r| {
            Some(WarehouseInfo {
                name: opt(names, r, "name")?,
                state: opt(names, r, "state").unwrap_or_default(),
                kind: opt(names, r, "type"),
                size: opt(names, r, "size").unwrap_or_default(),
                min_clusters: num(names, r, "min_cluster_count"),
                max_clusters: num(names, r, "max_cluster_count"),
                running: num(names, r, "running"),
                queued: num(names, r, "queued"),
                auto_suspend: opt(names, r, "auto_suspend").and_then(|v| v.parse().ok()),
                auto_resume: field(names, r, "auto_resume").is_some_and(|v| v.eq_ignore_ascii_case("true")),
                scaling_policy: opt(names, r, "scaling_policy"),
                owner: opt(names, r, "owner"),
                comment: opt(names, r, "comment"),
                created_on: opt(names, r, "created_on"),
                resumed_on: opt(names, r, "resumed_on"),
                resource_monitor: opt(names, r, "resource_monitor"),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspend {
    Never,
    Secs(u64),
}

pub fn suspend_of(w: &WarehouseInfo) -> Suspend {
    match w.auto_suspend {
        None | Some(0) => Suspend::Never,
        Some(s) => Suspend::Secs(s),
    }
}

/// Multi-cluster: `1–3 · STANDARD`; a single cluster does not deserve a line.
pub fn clusters_label(w: &WarehouseInfo) -> Option<String> {
    (w.max_clusters > 1 || w.min_clusters > 1).then(|| match &w.scaling_policy {
        Some(p) => format!("{}–{} · {p}", w.min_clusters, w.max_clusters),
        None => format!("{}–{}", w.min_clusters, w.max_clusters),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhGrant {
    pub privilege: String,
    pub role: String,
}

/// `SHOW GRANTS ON WAREHOUSE`: only those granted to roles.
pub fn parse_wh_grants(names: &[String], rows: &Rows) -> Vec<WhGrant> {
    rows.iter()
        .filter(|r| field(names, r, "granted_to").is_some_and(|g| g.eq_ignore_ascii_case("ROLE")))
        .filter_map(|r| Some(WhGrant { privilege: opt(names, r, "privilege")?, role: unquote(opt(names, r, "grantee_name")?) }))
        .collect()
}

/// Strongest to weakest, as in "who rules and who uses".
pub fn grants_by_privilege(g: &[WhGrant]) -> Vec<(String, Vec<String>)> {
    const ORDER: [&str; 5] = ["OWNERSHIP", "MODIFY", "OPERATE", "MONITOR", "USAGE"];
    let rank = |p: &str| ORDER.iter().position(|o| *o == p).unwrap_or(ORDER.len());
    let mut by: Vec<(String, Vec<String>)> = Vec::new();
    for x in g {
        match by.iter_mut().find(|(p, _)| *p == x.privilege) {
            Some((_, v)) => v.push(x.role.clone()),
            None => by.push((x.privilege.clone(), vec![x.role.clone()])),
        }
    }
    by.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
    for (_, v) in &mut by {
        v.sort_unstable();
        v.dedup();
    }
    by
}

pub async fn warehouses(s: &Session) -> Result<Vec<WarehouseInfo>, DbError> {
    let (n, r) = s.query_named("SHOW WAREHOUSES").await?;
    Ok(parse_warehouses(&n, &r))
}

pub async fn wh_grants(s: &Session, name: &str) -> Result<Vec<WhGrant>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS ON WAREHOUSE {}", quote_ident(name))).await?;
    Ok(parse_wh_grants(&n, &r))
}

/// Users who reach the warehouse: they hold (directly or by inheritance) some role from the grants.
/// `graph`: role → roles it inherits; here the search walks backwards (who inherits whom).
pub fn users_reaching(granted: &[String], graph: &RoleGraph, user_roles: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut heirs: HashMap<&str, Vec<&str>> = HashMap::new();
    for (parent, kids) in graph {
        for k in kids {
            heirs.entry(k.as_str()).or_default().push(parent.as_str());
        }
    }
    let mut reach: HashSet<&str> = granted.iter().map(String::as_str).collect();
    let mut queue: VecDeque<&str> = reach.iter().copied().collect();
    while let Some(r) = queue.pop_front() {
        for h in heirs.get(r).into_iter().flatten() {
            if reach.insert(h) {
                queue.push_back(h);
            }
        }
    }
    let mut out: Vec<String> = user_roles.iter().filter(|(_, roles)| roles.iter().any(|r| reach.contains(r.as_str()))).map(|(u, _)| u.clone()).collect();
    out.sort_unstable();
    out
}

/// Roles of all users (ACCOUNT_USAGE: lags up to ~2 h).
pub const USER_GRANTS_SQL: &str = "select grantee_name, role from snowflake.account_usage.grants_to_users where deleted_on is null";

pub fn parse_user_grants_all(names: &[String], rows: &Rows) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows {
        if let (Some(u), Some(role)) = (opt(names, r, "grantee_name"), opt(names, r, "role")) {
            m.entry(u).or_default().push(role);
        }
    }
    m
}

pub async fn all_user_grants(s: &Session) -> Result<HashMap<String, Vec<String>>, DbError> {
    let (n, r) = s.query_named(USER_GRANTS_SQL).await?;
    Ok(parse_user_grants_all(&n, &r))
}

/// Credits per day (14 days, generated on the server: a day without use comes as 0). ACCOUNT_USAGE (up to ~3 h lag):
/// the INFORMATION_SCHEMA version requires a current database and MONITOR USAGE, and matches the name as an identifier.
pub fn credits_sql(wh: &str) -> String {
    format!(
        "with d as (select dateadd(day, -(row_number() over (order by seq4()) - 1), current_date()) as day from table(generator(rowcount => 14))),
m as (
  select start_time::date as day, sum(credits_used) as credits
  from snowflake.account_usage.warehouse_metering_history
  where warehouse_name = {w} and start_time >= dateadd('day', -13, current_date())
  group by 1
)
select to_char(d.day, 'YYYY-MM-DD') as day, coalesce(m.credits, 0)::string as credits from d left join m on m.day = d.day",
        w = sql_str(wh)
    )
}

pub fn parse_credits(names: &[String], rows: &Rows) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = rows.iter().filter_map(|r| Some((opt(names, r, "day")?, num(names, r, "credits")))).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageUser {
    pub name: String,
    pub queries: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlowQuery {
    pub query_id: String,
    pub user: String,
    pub elapsed_ms: u64,
    pub start: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WhUsage {
    pub total: u64,
    /// Queries that waited for lack of capacity (queued_overload_time > 0).
    pub queued: u64,
    pub avg_queue_ms: f64,
    pub max_queue_ms: f64,
    pub spilled_local: u64,
    pub spilled_remote: u64,
    pub top_users: Vec<UsageUser>,
    pub slow: Vec<SlowQuery>,
}

/// Linhas `kind, a..e` (text): several aggregations in a single round trip to the server. ACCOUNT_USAGE: up to ~2 h lag.
pub fn usage_sql(wh: &str) -> String {
    format!(
        "with q as (
  select query_id, user_name, total_elapsed_time, queued_overload_time, bytes_spilled_to_local_storage, bytes_spilled_to_remote_storage, start_time, query_text
  from snowflake.account_usage.query_history
  where warehouse_name = {w} and start_time >= dateadd(day, -14, current_timestamp())
)
select 'total' as kind, count(*)::string as a, count_if(queued_overload_time > 0)::string as b,
  coalesce(avg(iff(queued_overload_time > 0, queued_overload_time, null)), 0)::string as c, coalesce(max(queued_overload_time), 0)::string as d, null::string as e from q
union all select 'spill', coalesce(sum(bytes_spilled_to_local_storage), 0)::string, coalesce(sum(bytes_spilled_to_remote_storage), 0)::string, null, null, null from q
union all select * from (select 'user', user_name, count(*)::string, sum(total_elapsed_time)::string, null, null from q group by user_name order by count(*) desc limit 5)
union all select * from (select 'slow', query_id, user_name, total_elapsed_time::string, to_char(start_time, 'YYYY-MM-DD HH24:MI'), left(query_text, 200) from q order by total_elapsed_time desc limit 10)",
        w = sql_str(wh)
    )
}

pub fn parse_usage(names: &[String], rows: &Rows) -> WhUsage {
    let mut u = WhUsage::default();
    let f = |r: &[Option<String>], c: &str| num::<f64>(names, r, c);
    for r in rows {
        let s = |c: &str| opt(names, r, c).unwrap_or_default();
        match field(names, r, "kind") {
            Some("total") => {
                (u.total, u.queued) = (f(r, "a") as u64, f(r, "b") as u64);
                (u.avg_queue_ms, u.max_queue_ms) = (f(r, "c"), f(r, "d"));
            }
            Some("spill") => (u.spilled_local, u.spilled_remote) = (f(r, "a") as u64, f(r, "b") as u64),
            Some("user") => u.top_users.push(UsageUser { name: s("a"), queries: f(r, "b") as u64, elapsed_ms: f(r, "c") as u64 }),
            Some("slow") => u.slow.push(SlowQuery { query_id: s("a"), user: s("b"), elapsed_ms: f(r, "c") as u64, start: s("d"), text: s("e") }),
            _ => {}
        }
    }
    // UNION ALL does not guarantee order.
    u.top_users.sort_by(|a, b| b.queries.cmp(&a.queries).then_with(|| a.name.cmp(&b.name)));
    u.slow.sort_by(|a, b| b.elapsed_ms.cmp(&a.elapsed_ms));
    u
}

/// More than 5% of queries queued, or spill to remote disk: sign of a warehouse that is too small.
pub fn undersized(u: &WhUsage) -> bool {
    u.total > 0 && (u.queued * 20 > u.total || u.spilled_remote > 0)
}

pub async fn credits(s: &Session, wh: &str) -> Result<Vec<(String, f64)>, DbError> {
    let (n, r) = s.query_named(&credits_sql(wh)).await?;
    Ok(parse_credits(&n, &r))
}

pub async fn usage(s: &Session, wh: &str) -> Result<WhUsage, DbError> {
    let (n, r) = s.query_named(&usage_sql(wh)).await?;
    Ok(parse_usage(&n, &r))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(names: &[&str], rows: &[&[Option<&str>]]) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        (names.iter().map(|s| s.to_string()).collect(), rows.iter().map(|r| r.iter().map(|v| v.map(String::from)).collect()).collect())
    }

    #[test]
    fn warehouses_by_column_name_sorted() {
        let (n, r) = t(
            &["state", "NAME", "size", "min_cluster_count", "max_cluster_count", "running", "queued", "auto_suspend", "auto_resume", "scaling_policy", "owner", "resource_monitor", "type"],
            &[
                &[Some("SUSPENDED"), Some("Z_WH"), Some("X-Small"), Some("1"), Some("1"), Some("0"), Some("0"), None, Some("true"), Some("STANDARD"), Some("SYSADMIN"), Some("null"), Some("STANDARD")],
                &[Some("STARTED"), Some("A_WH"), Some("Medium"), Some("1"), Some("3"), Some("3"), Some("1"), Some("600"), Some("false"), Some("ECONOMY"), Some("SYSADMIN"), Some("RM1"), Some("STANDARD")],
            ],
        );
        let w = parse_warehouses(&n, &r);
        assert_eq!(w.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), vec!["A_WH", "Z_WH"]);
        assert_eq!((w[0].running, w[0].queued, w[0].auto_resume), (3, 1, false));
        assert_eq!(w[0].resource_monitor.as_deref(), Some("RM1"));
        assert_eq!(w[1].resource_monitor, None, "\"null\" from SHOW becomes None");
        assert_eq!(suspend_of(&w[0]), Suspend::Secs(600));
        assert_eq!(suspend_of(&w[1]), Suspend::Never);
        assert_eq!(clusters_label(&w[0]).as_deref(), Some("1–3 · ECONOMY"));
        assert_eq!(clusters_label(&w[1]), None);
        let zero = WarehouseInfo { auto_suspend: Some(0), ..w[0].clone() };
        assert_eq!(suspend_of(&zero), Suspend::Never);
    }

    #[test]
    fn grants_keep_roles_grouped_by_privilege() {
        let (n, r) = t(
            &["privilege", "granted_on", "name", "granted_to", "grantee_name"],
            &[
                &[Some("USAGE"), Some("WAREHOUSE"), Some("WH"), Some("ROLE"), Some("ANALYST")],
                &[Some("OWNERSHIP"), Some("WAREHOUSE"), Some("WH"), Some("ROLE"), Some("SYSADMIN")],
                &[Some("USAGE"), Some("WAREHOUSE"), Some("WH"), Some("ROLE"), Some("\"low\"")],
                &[Some("OPERATE"), Some("WAREHOUSE"), Some("WH"), Some("ROLE"), Some("DATA_ENGINEER")],
                &[Some("USAGE"), Some("WAREHOUSE"), Some("WH"), Some("SHARE"), Some("S1")],
            ],
        );
        let g = parse_wh_grants(&n, &r);
        assert_eq!(g.len(), 4, "share is left out");
        let by = grants_by_privilege(&g);
        assert_eq!(by.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["OWNERSHIP", "OPERATE", "USAGE"]);
        assert_eq!(by[2].1, vec!["ANALYST", "low"]);
    }

    fn graph(edges: &[(&str, &str)]) -> RoleGraph {
        let mut g = RoleGraph::new();
        for (a, b) in edges {
            g.entry(a.to_string()).or_default().push(b.to_string());
        }
        g
    }

    fn users(u: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        u.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect()
    }

    #[test]
    fn users_reach_through_inheritance() {
        let g = graph(&[("A", "WH_ROLE"), ("B", "A"), ("C", "B"), ("X", "Y")]);
        let u = users(&[("u1", &["C"]), ("u2", &["X"]), ("u3", &["WH_ROLE"]), ("u4", &["C", "A"])]);
        assert_eq!(users_reaching(&["WH_ROLE".to_string()], &g, &u), vec!["u1", "u3", "u4"]);
    }

    #[test]
    fn users_reaching_survives_cycles() {
        let g = graph(&[("A", "B"), ("B", "A")]);
        let u = users(&[("u", &["B"])]);
        assert_eq!(users_reaching(&["A".to_string()], &g, &u), vec!["u"]);
    }

    #[test]
    fn user_grants_are_grouped_by_user() {
        let (n, r) = t(&["GRANTEE_NAME", "ROLE"], &[&[Some("u1"), Some("A")], &[Some("u1"), Some("B")], &[Some("u2"), Some("A")], &[None, Some("Z")]]);
        let m = parse_user_grants_all(&n, &r);
        assert_eq!(m["u1"], vec!["A", "B"]);
        assert_eq!(m.len(), 2);
        assert!(USER_GRANTS_SQL.contains("deleted_on is null"));
    }

    #[test]
    fn sql_quotes_the_warehouse_name() {
        let c = credits_sql("a'b");
        assert!(c.contains("warehouse_name = 'a''b'") && c.contains("rowcount => 14"), "{c}");
        assert!(c.contains("account_usage.warehouse_metering_history") && !c.contains("information_schema"), "does not depend on a current database or MONITOR USAGE");
        assert!(usage_sql("X").contains("warehouse_name = 'X'"));
    }

    #[test]
    fn credits_come_sorted_by_day() {
        let (n, r) = t(&["DAY", "CREDITS"], &[&[Some("2026-10-16"), Some("1.5")], &[Some("2026-10-15"), Some("0")]]);
        assert_eq!(parse_credits(&n, &r), vec![("2026-10-15".to_string(), 0.0), ("2026-10-16".to_string(), 1.5)]);
    }

    #[test]
    fn usage_rows_are_aggregated() {
        let (n, r) = t(
            &["KIND", "A", "B", "C", "D", "E"],
            &[
                &[Some("user"), Some("bob"), Some("3"), Some("900"), None, None],
                &[Some("total"), Some("100"), Some("6"), Some("1500.5"), Some("9000"), None],
                &[Some("slow"), Some("q2"), Some("bob"), Some("500"), Some("2026-10-15 10:00"), Some("select 2")],
                &[Some("user"), Some("ana"), Some("10"), Some("100"), None, None],
                &[Some("spill"), Some("10"), Some("0"), None, None, None],
                &[Some("slow"), Some("q1"), Some("ana"), Some("9000"), Some("2026-10-14 09:00"), Some("select 1")],
            ],
        );
        let u = parse_usage(&n, &r);
        assert_eq!((u.total, u.queued, u.avg_queue_ms, u.max_queue_ms, u.spilled_local, u.spilled_remote), (100, 6, 1500.5, 9000.0, 10, 0));
        assert_eq!(u.top_users.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["ana", "bob"]);
        assert_eq!(u.slow[0], SlowQuery { query_id: "q1".into(), user: "ana".into(), elapsed_ms: 9000, start: "2026-10-14 09:00".into(), text: "select 1".into() });
        assert!(undersized(&u), "6% queued");
    }

    #[test]
    fn undersized_rule() {
        let u = |total, queued, remote| WhUsage { total, queued, spilled_remote: remote, ..Default::default() };
        assert!(!undersized(&u(100, 5, 0)));
        assert!(undersized(&u(100, 6, 0)));
        assert!(undersized(&u(100, 0, 1)));
        assert!(!undersized(&u(0, 0, 0)));
    }
}
