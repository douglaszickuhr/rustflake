//! Snowflake users and roles (read-only): SQL, `SHOW` parsers and role inheritance.

use crate::DbError;
use crate::lexer::{quote_always, sql_str};
use crate::session::Session;
use crate::snowflake::{Rows, named};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserInfo {
    pub name: String,
    pub login_name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub disabled: bool,
    pub locked: bool,
    pub default_role: Option<String>,
    pub default_warehouse: Option<String>,
    pub default_namespace: Option<String>,
    pub created_on: Option<String>,
    pub last_success_login: Option<String>,
    pub has_password: bool,
    pub has_rsa_public_key: bool,
    pub has_mfa: bool,
    pub user_type: Option<String>,
    pub comment: Option<String>,
    /// Role that owns the user; `*_PROVISIONER` (SCIM) = provisioned externally.
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleInfo {
    pub name: String,
    pub owner: Option<String>,
    pub comment: Option<String>,
    pub assigned_to_users: u64,
    pub granted_to_roles: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub privilege: String,
    pub granted_on: String,
    pub name: String,
    pub grant_option: bool,
}

/// Who received a role: `kind` = USER or ROLE.
#[derive(Debug, Clone, PartialEq)]
pub struct Grantee {
    pub kind: String,
    pub name: String,
}

/// SHOW may return a role name already quoted (`"lowercase"`): back to the raw name.
pub(crate) fn unquote(s: String) -> String {
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(inner) if !inner.is_empty() => inner.replace("\"\"", "\""),
        _ => s,
    }
}

/// `SHOW USERS`.
pub fn parse_users(names: &[String], rows: &Rows) -> Vec<UserInfo> {
    named(names, rows)
        .filter_map(|r| {
            Some(UserInfo {
                name: r.text("name")?,
                login_name: r.text("login_name"),
                display_name: r.text("display_name"),
                email: r.text("email"),
                disabled: r.flag("disabled"),
                locked: r.flag("snowflake_lock"),
                default_role: r.text("default_role"),
                default_warehouse: r.text("default_warehouse"),
                default_namespace: r.text("default_namespace"),
                created_on: r.text("created_on"),
                last_success_login: r.text("last_success_login"),
                has_password: r.flag("has_password"),
                has_rsa_public_key: r.flag("has_rsa_public_key"),
                has_mfa: r.flag("has_mfa"),
                user_type: r.text("type"),
                comment: r.text("comment"),
                owner: r.text("owner"),
            })
        })
        .collect()
}

/// `SHOW ROLES`.
pub fn parse_roles(names: &[String], rows: &Rows) -> Vec<RoleInfo> {
    named(names, rows)
        .filter_map(|r| {
            Some(RoleInfo { name: r.text("name")?, owner: r.text("owner"), comment: r.text("comment"), assigned_to_users: r.num("assigned_to_users"), granted_to_roles: r.num("granted_to_roles") })
        })
        .collect()
}

/// `SHOW GRANTS TO USER`: direct roles only.
/// Role granted directly to the user, with who granted it and when.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectGrant {
    pub role: String,
    pub granted_by: Option<String>,
    pub created_on: Option<String>,
}

impl DirectGrant {
    /// Came from SCIM provisioning (IdP group), not from someone granting it by hand.
    pub fn from_scim(&self) -> bool {
        crate::user_view::scim_managed(self.granted_by.as_deref())
    }
}

/// `SHOW GRANTS TO USER` with the origin of each role.
pub fn parse_user_grants(names: &[String], rows: &Rows) -> Vec<DirectGrant> {
    named(names, rows).filter_map(|r| Some(DirectGrant { role: unquote(r.text("role")?), granted_by: r.text("granted_by"), created_on: r.text("created_on") })).collect()
}

/// `SHOW GRANTS TO ROLE`.
pub fn parse_role_grants(names: &[String], rows: &Rows) -> Vec<Grant> {
    named(names, rows)
        .filter_map(|r| {
            let granted_on = r.text("granted_on").unwrap_or_default();
            let name = r.text("name").unwrap_or_default();
            Some(Grant {
                privilege: r.text("privilege")?,
                // Role only: in a table the quotes are part of the qualified name.
                name: if granted_on == "ROLE" { unquote(name) } else { name },
                granted_on,
                grant_option: r.flag("grant_option"),
            })
        })
        .collect()
}

/// `SHOW GRANTS OF ROLE`.
pub fn parse_grantees(names: &[String], rows: &Rows) -> Vec<Grantee> {
    named(names, rows).filter_map(|r| Some(Grantee { kind: r.text("granted_to")?, name: unquote(r.text("grantee_name")?) })).collect()
}

/// Inheriting a role = having USAGE on it.
pub fn inherited_roles(grants: &[Grant]) -> Vec<String> {
    grants.iter().filter(|g| g.privilege == "USAGE" && g.granted_on == "ROLE").map(|g| g.name.clone()).collect()
}

/// Permission error (worth a hint about MANAGE GRANTS / IMPORTED PRIVILEGES).
pub fn privilege_hint(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("insufficient privileges") || m.contains("does not exist or not authorized")
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRole {
    pub name: String,
    /// From the direct role down to this one (inclusive).
    pub path: Vec<String>,
}

/// Inherited roles (excluding direct ones), by level: each one once, with the shortest path.
/// `seen` cuts cycles. Role names are exact (case-sensitive) on Snowflake.
pub async fn effective_roles<E>(direct: &[String], mut inherits: impl AsyncFnMut(&str) -> Result<Vec<String>, E>) -> Result<Vec<EffectiveRole>, E> {
    let mut seen: std::collections::HashSet<String> = direct.iter().cloned().collect();
    let mut frontier: Vec<Vec<String>> = direct.iter().map(|r| vec![r.clone()]).collect();
    let mut out = Vec::new();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for path in frontier {
            let last = path.last().expect("path is never empty").clone();
            for child in inherits(&last).await? {
                if seen.insert(child.clone()) {
                    let mut p = path.clone();
                    p.push(child.clone());
                    out.push(EffectiveRole { name: child, path: p.clone() });
                    next.push(p);
                }
            }
        }
        frontier = next;
    }
    Ok(out)
}

/// Whole hierarchy in a single query (SHOW per role takes minutes with ACCOUNTADMIN).
/// ponytail: ACCOUNT_USAGE lags up to ~2 h; a recent grant may be missing.
pub const ROLE_GRAPH_SQL: &str = "select grantee_name, name from snowflake.account_usage.grants_to_roles \
where granted_on = 'ROLE' and granted_to = 'ROLE' and privilege = 'USAGE' and deleted_on is null";

/// Databases where each role has USAGE: groups access roles by database.
pub const ROLE_DATABASES_SQL: &str = "select grantee_name, name from snowflake.account_usage.grants_to_roles \
where granted_on = 'DATABASE' and granted_to = 'ROLE' and privilege = 'USAGE' and deleted_on is null";

/// role → databases (same format as the graph: grantee_name → name).
pub async fn role_databases(s: &Session) -> Result<RoleGraph, DbError> {
    let (n, r) = s.query_named(ROLE_DATABASES_SQL).await?;
    Ok(parse_role_graph(&n, &r))
}

/// role → roles it inherits.
pub type RoleGraph = std::collections::HashMap<String, Vec<String>>;

pub fn parse_role_graph(names: &[String], rows: &Rows) -> RoleGraph {
    let mut g = RoleGraph::new();
    for r in named(names, rows) {
        if let (Some(from), Some(to)) = (r.text("grantee_name"), r.text("name")) {
            g.entry(from).or_default().push(to);
        }
    }
    g
}

pub async fn role_graph(s: &Session) -> Result<RoleGraph, DbError> {
    let (n, r) = s.query_named(ROLE_GRAPH_SQL).await?;
    Ok(parse_role_graph(&n, &r))
}

/// Same search as `effective_roles`, over the in-memory graph.
pub fn effective_from_graph(direct: &[String], graph: &RoleGraph) -> Vec<EffectiveRole> {
    let mut seen: std::collections::HashSet<&str> = direct.iter().map(String::as_str).collect();
    let mut frontier: Vec<Vec<String>> = direct.iter().map(|r| vec![r.clone()]).collect();
    let mut out = Vec::new();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for path in frontier {
            for child in graph.get(path.last().expect("path is never empty")).into_iter().flatten() {
                if seen.insert(child) {
                    let mut p = path.clone();
                    p.push(child.clone());
                    out.push(EffectiveRole { name: child.clone(), path: p.clone() });
                    next.push(p);
                }
            }
        }
        frontier = next;
    }
    out
}

/// Role kind; who maps name → kind is the connection configuration. Variant order is the on-screen order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleKind {
    /// Access to a warehouse.
    Warehouse,
    /// Functional role.
    Functional,
    /// A department's base role.
    Base,
    /// Security classification `C<n>`.
    Classification(u8),
    /// Access role.
    Access,
    Other,
}

pub async fn users(s: &Session) -> Result<Vec<UserInfo>, DbError> {
    let (n, r) = s.query_named("SHOW USERS").await?;
    Ok(parse_users(&n, &r))
}

pub async fn roles(s: &Session) -> Result<Vec<RoleInfo>, DbError> {
    let (n, r) = s.query_named("SHOW ROLES").await?;
    Ok(parse_roles(&n, &r))
}

pub async fn user_grants(s: &Session, user: &str) -> Result<Vec<DirectGrant>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS TO USER {}", quote_always(user))).await?;
    Ok(parse_user_grants(&n, &r))
}

pub async fn role_grants(s: &Session, role: &str) -> Result<Vec<Grant>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS TO ROLE {}", quote_always(role))).await?;
    Ok(parse_role_grants(&n, &r))
}

pub async fn grantees(s: &Session, role: &str) -> Result<Vec<Grantee>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS OF ROLE {}", quote_always(role))).await?;
    Ok(parse_grantees(&n, &r))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryActivity {
    pub total: u64,
    pub last: Option<String>,
    pub elapsed_ms: u64,
    /// 30 days, oldest to newest (the server generates the empty days).
    pub per_day: Vec<(String, u64)>,
    pub top_warehouses: Vec<(String, u64)>,
    pub top_roles: Vec<(String, u64)>,
    /// Distinct roles used in the period's queries.
    pub roles_used: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginEvent {
    pub at: String,
    pub success: bool,
    pub factor: Option<String>,
    pub client: Option<String>,
    pub ip: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoginActivity {
    pub ok: u64,
    pub failed: u64,
    /// Most recent first.
    pub recent: Vec<LoginEvent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Activity {
    pub logins: LoginActivity,
    pub queries: QueryActivity,
    pub usage: Usage,
}

/// Last use in 90 days. Only warehouse and primary role can be measured: QUERY_HISTORY does not
/// record the inherited roles that granted the access.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub warehouses: std::collections::HashMap<String, String>,
    pub roles: std::collections::HashMap<String, String>,
}

pub fn usage_sql(names: &[&str]) -> String {
    format!(
        "with q as (
  select warehouse_name, role_name, start_time from snowflake.account_usage.query_history
  where user_name in ({u}) and start_time >= dateadd(day, -90, current_timestamp())
)
select 'wh' as kind, warehouse_name as k, max(start_time)::string as last from q where warehouse_name is not null group by 2
union all select 'role', role_name, max(start_time)::string from q where role_name is not null group by 2",
        u = in_list(names)
    )
}

pub fn parse_usage(names: &[String], rows: &Rows) -> Usage {
    let mut u = Usage::default();
    for r in named(names, rows) {
        let (Some(k), Some(last)) = (r.text("k"), r.text("last")) else { continue };
        match r.get("kind") {
            Some("wh") => u.warehouses.insert(k, last),
            Some("role") => u.roles.insert(k, last),
            _ => None,
        };
    }
    u
}

fn in_list(names: &[&str]) -> String {
    names.iter().map(|n| sql_str(n)).collect::<Vec<_>>().join(", ")
}

/// Rows `kind, k, n, extra`: several aggregations in a single trip to the server.
pub fn query_activity_sql(names: &[&str]) -> String {
    format!(
        "with q as (
  select start_time, warehouse_name, role_name, total_elapsed_time
  from snowflake.account_usage.query_history
  where user_name in ({u}) and start_time >= dateadd(day, -29, current_date())
),
d as (select dateadd(day, -(row_number() over (order by seq4()) - 1), current_date()) as day from table(generator(rowcount => 30))),
per_day as (select d.day, count(q.start_time) as n from d left join q on q.start_time::date = d.day group by d.day),
wh as (select warehouse_name as k, count(*) as n from q where warehouse_name is not null group by 1 order by n desc limit 3),
rl as (select role_name as k, count(*) as n from q where role_name is not null group by 1 order by n desc limit 3)
select 'total' as kind, null as k, count(*)::string as n, max(start_time)::string as extra from q
union all select 'ms', null, coalesce(sum(total_elapsed_time), 0)::string, null from q
union all select 'day', day::string, n::string, null from per_day
union all select 'wh', k, n::string, null from wh
union all select 'role', k, n::string, null from rl
union all select 'roles', null, count(distinct role_name)::string, null from q",
        u = in_list(names)
    )
}

/// Rows `kind, a..f`: totals + the 5 most recent events.
pub fn login_activity_sql(names: &[&str]) -> String {
    format!(
        "with l as (
  select event_timestamp, is_success, first_authentication_factor, reported_client_type, client_ip, error_message
  from snowflake.account_usage.login_history
  where user_name in ({u}) and event_timestamp >= dateadd(day, -30, current_timestamp())
)
select 'ok' as kind, count_if(is_success = 'YES')::string as a, null as b, null as c, null as d, null as e, null as f from l
union all select 'failed', count_if(is_success = 'NO')::string, null, null, null, null, null from l
union all select * from (
  select 'event', event_timestamp::string, is_success, first_authentication_factor, reported_client_type, client_ip, error_message
  from l order by event_timestamp desc limit 5
)",
        u = in_list(names)
    )
}

fn top(mut v: Vec<(String, u64)>) -> Vec<(String, u64)> {
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

pub fn parse_query_activity(names: &[String], rows: &Rows) -> QueryActivity {
    let mut a = QueryActivity::default();
    let (mut wh, mut rl) = (Vec::new(), Vec::new());
    for r in named(names, rows) {
        let k = r.text("k");
        let n = r.num("n");
        match r.get("kind") {
            Some("total") => (a.total, a.last) = (n, r.text("extra")),
            Some("ms") => a.elapsed_ms = n,
            Some("day") => a.per_day.extend(k.map(|k| (k, n))),
            Some("wh") => wh.extend(k.map(|k| (k, n))),
            Some("role") => rl.extend(k.map(|k| (k, n))),
            Some("roles") => a.roles_used = n,
            _ => {}
        }
    }
    // UNION ALL does not guarantee order: sort here.
    a.per_day.sort();
    (a.top_warehouses, a.top_roles) = (top(wh), top(rl));
    a
}

pub fn parse_login_activity(names: &[String], rows: &Rows) -> LoginActivity {
    let mut l = LoginActivity::default();
    for r in named(names, rows) {
        match r.get("kind") {
            Some("ok") => l.ok = r.num("a"),
            Some("failed") => l.failed = r.num("a"),
            Some("event") => {
                l.recent.push(LoginEvent { at: r.text("a").unwrap_or_default(), success: r.get("b") == Some("YES"), factor: r.text("c"), client: r.text("d"), ip: r.text("e"), error: r.text("f") })
            }
            _ => {}
        }
    }
    // Same timezone in the session: text order is time order.
    l.recent.sort_by(|a, b| b.at.cmp(&a.at));
    l
}

pub async fn activity(s: &Session, u: &UserInfo) -> Result<Activity, DbError> {
    let mut names = vec![u.name.as_str()];
    if let Some(l) = u.login_name.as_deref().filter(|l| *l != u.name) {
        names.push(l);
    }
    // Three ACCOUNT_USAGE scans, each seconds long: at the same time on Snowflake.
    let (s2, s3) = (s.sibling(), s.sibling());
    let (q1, q2, q3) = (login_activity_sql(&names), query_activity_sql(&names), usage_sql(&names));
    let (logins, queries, usage) = tokio::join!(s.query_named(&q1), s2.as_ref().unwrap_or(s).query_named(&q2), s3.as_ref().unwrap_or(s).query_named(&q3));
    let ((ln, lr), (qn, qr), (un, ur)) = (logins?, queries?, usage?);
    Ok(Activity { logins: parse_login_activity(&ln, &lr), queries: parse_query_activity(&qn, &qr), usage: parse_usage(&un, &ur) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(names: &[&str], rows: &[&[Option<&str>]]) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        (names.iter().map(|s| s.to_string()).collect(), rows.iter().map(|r| r.iter().map(|v| v.map(String::from)).collect()).collect())
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_always("my role"), "\"my role\"");
        assert_eq!(sql_str("o'brien"), "'o''brien'");
    }

    #[test]
    fn users_by_column_name_any_order_and_missing_columns() {
        let (n, r) = t(
            &["last_success_login", "NAME", "disabled", "default_role", "has_rsa_public_key", "snowflake_lock", "email"],
            &[
                &[Some("2026-10-14 09:00:00"), Some("JDOE"), Some("false"), Some("ANALYST"), Some("true"), Some("false"), Some("j@x.io")],
                &[None, Some("OLD"), Some("TRUE"), None, Some("false"), Some("true"), None],
                &[None, None, None, None, None, None, None],
            ],
        );
        let u = parse_users(&n, &r);
        assert_eq!(u.len(), 2, "row without a name is discarded");
        assert_eq!(u[0].name, "JDOE");
        assert_eq!(u[0].default_role.as_deref(), Some("ANALYST"));
        assert_eq!(u[0].last_success_login.as_deref(), Some("2026-10-14 09:00:00"));
        assert!(!u[0].disabled && u[0].has_rsa_public_key && !u[0].locked);
        assert_eq!(u[0].default_warehouse, None, "missing column becomes None");
        assert!(u[1].disabled && u[1].locked);
    }

    #[test]
    fn roles_and_user_roles() {
        let (n, r) = t(&["name", "owner", "assigned_to_users", "granted_to_roles", "comment"], &[&[Some("ANALYST"), Some("SECURITYADMIN"), Some("12"), Some("1"), None]]);
        let roles = parse_roles(&n, &r);
        assert_eq!(roles[0], RoleInfo { name: "ANALYST".into(), owner: Some("SECURITYADMIN".into()), comment: None, assigned_to_users: 12, granted_to_roles: 1 });
        let (n, r) = t(&["created_on", "role", "granted_to", "grantee_name"], &[&[None, Some("ANALYST"), Some("USER"), Some("JDOE")], &[None, Some("PUBLIC"), Some("USER"), Some("JDOE")]]);
        assert_eq!(parse_user_grants(&n, &r).into_iter().map(|g| g.role).collect::<Vec<_>>(), vec!["ANALYST", "PUBLIC"]);
    }

    #[test]
    fn role_grants_split_inherited_roles_from_privileges() {
        let (n, r) = t(
            &["privilege", "granted_on", "name", "grant_option"],
            &[
                &[Some("USAGE"), Some("ROLE"), Some("READER"), Some("false")],
                &[Some("USAGE"), Some("WAREHOUSE"), Some("WH_XS"), Some("false")],
                &[Some("SELECT"), Some("TABLE"), Some("DB.S.\"t\""), Some("true")],
            ],
        );
        let g = parse_role_grants(&n, &r);
        assert_eq!(g.len(), 3);
        assert!(g[2].grant_option);
        assert_eq!(inherited_roles(&g), vec!["READER"]);
    }

    #[test]
    fn grantees_of_role() {
        let (n, r) = t(&["role", "granted_to", "grantee_name"], &[&[Some("READER"), Some("ROLE"), Some("ANALYST")], &[Some("READER"), Some("USER"), Some("JDOE")]]);
        assert_eq!(parse_grantees(&n, &r), vec![Grantee { kind: "ROLE".into(), name: "ANALYST".into() }, Grantee { kind: "USER".into(), name: "JDOE".into() }]);
    }

    #[test]
    fn privilege_errors_are_recognised() {
        assert!(privilege_hint("SQL compilation error: Insufficient privileges to operate on account"));
        assert!(privilege_hint("Database 'SNOWFLAKE' does not exist or not authorized."));
        assert!(!privilege_hint("No active warehouse selected in the current session."));
    }

    fn graph(edges: &[(&str, &str)]) -> impl AsyncFnMut(&str) -> Result<Vec<String>, ()> {
        let edges: Vec<(String, String)> = edges.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        async move |r: &str| Ok(edges.iter().filter(|(a, _)| a == r).map(|(_, b)| b.clone()).collect())
    }

    fn eff(direct: &[&str], edges: &[(&str, &str)]) -> Vec<(String, String)> {
        let direct: Vec<String> = direct.iter().map(|s| s.to_string()).collect();
        pollster::block_on(effective_roles(&direct, graph(edges))).unwrap().into_iter().map(|e| (e.name, e.path.join(" → "))).collect()
    }

    #[test]
    fn effective_chain() {
        assert_eq!(eff(&["ANALYST"], &[("ANALYST", "READER"), ("READER", "PUBLIC")]), vec![("READER".into(), "ANALYST → READER".into()), ("PUBLIC".into(), "ANALYST → READER → PUBLIC".into()),]);
    }

    #[test]
    fn effective_diamond_keeps_shortest_path_once() {
        let r = eff(&["A"], &[("A", "B"), ("A", "C"), ("B", "D"), ("C", "D"), ("B", "X"), ("X", "D")]);
        assert_eq!(r.iter().filter(|(n, _)| n == "D").count(), 1);
        assert_eq!(r.iter().find(|(n, _)| n == "D").unwrap().1, "A → B → D");
    }

    #[test]
    fn effective_cycle_terminates_and_skips_direct_roles() {
        let r = eff(&["A", "B"], &[("A", "B"), ("B", "A"), ("B", "C")]);
        assert_eq!(r, vec![("C".into(), "B → C".into())]);
    }

    #[test]
    fn effective_propagates_errors() {
        let direct = vec!["A".to_string()];
        let r = pollster::block_on(effective_roles(&direct, async |_: &str| Err::<Vec<String>, _>("boom")));
        assert_eq!(r.unwrap_err(), "boom");
    }

    #[test]
    fn activity_sql_filters_both_names_escaped() {
        let q = query_activity_sql(&["JDOE", "jdoe@x.io"]);
        assert!(q.contains("user_name in ('JDOE', 'jdoe@x.io')"), "{q}");
        assert!(q.contains("rowcount => 30"));
        assert!(login_activity_sql(&["O'B"]).contains("user_name in ('O''B')"));
    }

    #[test]
    fn query_activity_rows() {
        let (n, r) = t(
            &["KIND", "K", "N", "EXTRA"],
            &[
                &[Some("day"), Some("2026-10-15"), Some("4"), None],
                &[Some("total"), None, Some("7"), Some("2026-10-15 10:00:00")],
                &[Some("day"), Some("2026-10-14"), Some("3"), None],
                &[Some("ms"), None, Some("12500"), None],
                &[Some("wh"), Some("WH_S"), Some("2"), None],
                &[Some("wh"), Some("WH_XS"), Some("5"), None],
                &[Some("role"), Some("ANALYST"), Some("7"), None],
            ],
        );
        let a = parse_query_activity(&n, &r);
        assert_eq!((a.total, a.elapsed_ms, a.last.as_deref()), (7, 12500, Some("2026-10-15 10:00:00")));
        assert_eq!(a.per_day, vec![("2026-10-14".into(), 3), ("2026-10-15".into(), 4)], "days in ascending order");
        assert_eq!(a.top_warehouses, vec![("WH_XS".into(), 5), ("WH_S".into(), 2)], "largest first");
        assert_eq!(a.top_roles, vec![("ANALYST".into(), 7)]);
    }

    #[test]
    fn login_activity_rows() {
        let (n, r) = t(
            &["KIND", "A", "B", "C", "D", "E", "F"],
            &[
                &[Some("ok"), Some("9"), None, None, None, None, None],
                &[Some("failed"), Some("1"), None, None, None, None, None],
                &[Some("event"), Some("2026-10-14 08:00:00"), Some("NO"), Some("PASSWORD"), Some("JDBC_DRIVER"), Some("1.2.3.4"), Some("INCORRECT_USERNAME_PASSWORD")],
                &[Some("event"), Some("2026-10-15 08:00:00"), Some("YES"), Some("SAML2_ASSERTION"), Some("SNOWFLAKE_UI"), Some("5.6.7.8"), None],
            ],
        );
        let l = parse_login_activity(&n, &r);
        assert_eq!((l.ok, l.failed), (9, 1));
        assert_eq!(l.recent.len(), 2);
        assert!(l.recent[0].success && l.recent[0].at.starts_with("2026-10-15"), "most recent first");
        assert_eq!(l.recent[1].error.as_deref(), Some("INCORRECT_USERNAME_PASSWORD"));
    }

    #[test]
    fn sql_str_escapes_backslash() {
        assert_eq!(sql_str("foo\\"), "'foo\\\\'");
        assert_eq!(sql_str("x\\' or 1=1 --"), "'x\\\\'' or 1=1 --'");
    }

    #[test]
    fn quoted_role_names_are_unquoted() {
        let (n, r) = t(&["privilege", "granted_on", "name"], &[&[Some("USAGE"), Some("ROLE"), Some("\"my \"\"x\"\"\"")], &[Some("SELECT"), Some("TABLE"), Some("DB.S.\"t\"")]]);
        let g = parse_role_grants(&n, &r);
        assert_eq!(inherited_roles(&g), vec!["my \"x\""]);
        assert_eq!(g[1].name, "DB.S.\"t\"", "non-role object stays as it came");
        let (n, r) = t(&["role", "granted_to", "grantee_name"], &[&[Some("\"low\""), Some("ROLE"), Some("\"a b\"")]]);
        assert_eq!(parse_grantees(&n, &r)[0].name, "a b");
        assert_eq!(parse_user_grants(&n, &r)[0].role, "low");
    }

    #[test]
    fn role_graph_from_account_usage() {
        let (n, r) =
            t(&["GRANTEE_NAME", "NAME"], &[&[Some("ACCOUNTADMIN"), Some("SYSADMIN")], &[Some("SYSADMIN"), Some("DATA_ENG")], &[Some("ACCOUNTADMIN"), Some("SECURITYADMIN")], &[None, Some("X")]]);
        let g = parse_role_graph(&n, &r);
        assert_eq!(g["ACCOUNTADMIN"], vec!["SYSADMIN", "SECURITYADMIN"]);
        assert!(ROLE_GRAPH_SQL.contains("deleted_on is null"));
        let eff: Vec<String> = effective_from_graph(&["ACCOUNTADMIN".to_string()], &g).into_iter().map(|e| e.path.join(" → ")).collect();
        assert_eq!(eff, vec!["ACCOUNTADMIN → SYSADMIN", "ACCOUNTADMIN → SECURITYADMIN", "ACCOUNTADMIN → SYSADMIN → DATA_ENG"]);
    }

    #[test]
    fn owner_and_distinct_roles_are_read() {
        let (n, r) = t(&["name", "owner"], &[&[Some("U"), Some("OKTA_PROVISIONER")]]);
        assert_eq!(parse_users(&n, &r)[0].owner.as_deref(), Some("OKTA_PROVISIONER"));
        let (n, r) = t(&["KIND", "K", "N", "EXTRA"], &[&[Some("roles"), None, Some("3"), None]]);
        assert_eq!(parse_query_activity(&n, &r).roles_used, 3);
        assert!(query_activity_sql(&["U"]).contains("count(distinct role_name)"));
    }

    #[test]
    fn direct_grants_keep_who_granted_and_when() {
        let (n, r) = t(
            &["created_on", "role", "granted_to", "grantee_name", "granted_by"],
            &[
                &[Some("2026-03-03 10:00:00"), Some("SANDBOX_ADMIN"), Some("USER"), Some("ANA"), Some("SECURITYADMIN")],
                &[None, Some("\"idp_fr\""), Some("USER"), Some("ANA"), Some("OKTA_PROVISIONER")],
            ],
        );
        let g = parse_user_grants(&n, &r);
        assert_eq!(g[0], DirectGrant { role: "SANDBOX_ADMIN".into(), granted_by: Some("SECURITYADMIN".into()), created_on: Some("2026-03-03 10:00:00".into()) });
        assert_eq!(g[1].role, "idp_fr");
        assert!(g[1].from_scim() && !g[0].from_scim());
    }

    #[test]
    fn usage_rows_by_warehouse_and_role() {
        let (n, r) = t(&["KIND", "K", "LAST"], &[&[Some("wh"), Some("GENERAL_XS_WH"), Some("2026-10-15 10:00:00")], &[Some("role"), Some("FINANCE_FR"), Some("2026-10-01 10:00:00")]]);
        let u = parse_usage(&n, &r);
        assert_eq!(u.warehouses["GENERAL_XS_WH"], "2026-10-15 10:00:00");
        assert_eq!(u.roles["FINANCE_FR"], "2026-10-01 10:00:00");
        let sql = usage_sql(&["A"]);
        assert!(sql.contains("-90") && sql.contains("user_name in ('A')"), "{sql}");
        assert!(ROLE_DATABASES_SQL.contains("granted_on = 'DATABASE'"));
    }
}
