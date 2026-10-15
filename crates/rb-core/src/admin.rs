//! Usuários e roles do Snowflake (somente leitura): SQL, parsers dos `SHOW` e herança de roles.

use crate::DbError;
use crate::session::Session;
use crate::snowflake::field;

type Rows = [Vec<Option<String>>];

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

/// Quem recebeu uma role: `kind` = USER ou ROLE.
#[derive(Debug, Clone, PartialEq)]
pub struct Grantee {
    pub kind: String,
    pub name: String,
}

/// Nomes vêm exatos do `SHOW`: entre aspas preserva caixa, espaço e aspas.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// No Snowflake `\\` também escapa dentro de '...': dobra os dois.
pub fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// O SHOW pode devolver nome de role já entre aspas (`"minúscula"`): volta ao nome cru.
fn unquote(s: String) -> String {
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(inner) if !inner.is_empty() => inner.replace("\"\"", "\""),
        _ => s,
    }
}

fn opt(names: &[String], r: &[Option<String>], col: &str) -> Option<String> {
    field(names, r, col).filter(|v| !v.is_empty()).map(String::from)
}

fn flag(names: &[String], r: &[Option<String>], col: &str) -> bool {
    field(names, r, col).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn num(names: &[String], r: &[Option<String>], col: &str) -> u64 {
    field(names, r, col).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// `SHOW USERS`.
pub fn parse_users(names: &[String], rows: &Rows) -> Vec<UserInfo> {
    rows.iter()
        .filter_map(|r| {
            Some(UserInfo {
                name: opt(names, r, "name")?,
                login_name: opt(names, r, "login_name"),
                display_name: opt(names, r, "display_name"),
                email: opt(names, r, "email"),
                disabled: flag(names, r, "disabled"),
                locked: flag(names, r, "snowflake_lock"),
                default_role: opt(names, r, "default_role"),
                default_warehouse: opt(names, r, "default_warehouse"),
                default_namespace: opt(names, r, "default_namespace"),
                created_on: opt(names, r, "created_on"),
                last_success_login: opt(names, r, "last_success_login"),
                has_password: flag(names, r, "has_password"),
                has_rsa_public_key: flag(names, r, "has_rsa_public_key"),
                has_mfa: flag(names, r, "has_mfa"),
                user_type: opt(names, r, "type"),
                comment: opt(names, r, "comment"),
            })
        })
        .collect()
}

/// `SHOW ROLES`.
pub fn parse_roles(names: &[String], rows: &Rows) -> Vec<RoleInfo> {
    rows.iter()
        .filter_map(|r| {
            Some(RoleInfo {
                name: opt(names, r, "name")?,
                owner: opt(names, r, "owner"),
                comment: opt(names, r, "comment"),
                assigned_to_users: num(names, r, "assigned_to_users"),
                granted_to_roles: num(names, r, "granted_to_roles"),
            })
        })
        .collect()
}

/// `SHOW GRANTS TO USER`: só as roles diretas.
pub fn parse_user_roles(names: &[String], rows: &Rows) -> Vec<String> {
    rows.iter().filter_map(|r| opt(names, r, "role").map(unquote)).collect()
}

/// `SHOW GRANTS TO ROLE`.
pub fn parse_role_grants(names: &[String], rows: &Rows) -> Vec<Grant> {
    rows.iter()
        .filter_map(|r| {
            let granted_on = opt(names, r, "granted_on").unwrap_or_default();
            let name = opt(names, r, "name").unwrap_or_default();
            Some(Grant {
                privilege: opt(names, r, "privilege")?,
                // Só role: em tabela as aspas fazem parte do nome qualificado.
                name: if granted_on == "ROLE" { unquote(name) } else { name },
                granted_on,
                grant_option: flag(names, r, "grant_option"),
            })
        })
        .collect()
}

/// `SHOW GRANTS OF ROLE`.
pub fn parse_grantees(names: &[String], rows: &Rows) -> Vec<Grantee> {
    rows.iter().filter_map(|r| Some(Grantee { kind: opt(names, r, "granted_to")?, name: unquote(opt(names, r, "grantee_name")?) })).collect()
}

/// Herdar uma role = ter USAGE nela.
pub fn inherited_roles(grants: &[Grant]) -> Vec<String> {
    grants.iter().filter(|g| g.privilege == "USAGE" && g.granted_on == "ROLE").map(|g| g.name.clone()).collect()
}

/// Erro de permissão (vale dica sobre MANAGE GRANTS / IMPORTED PRIVILEGES).
pub fn privilege_hint(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("insufficient privileges") || m.contains("does not exist or not authorized")
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRole {
    pub name: String,
    /// Da role direta até esta (inclusive).
    pub path: Vec<String>,
}

/// Roles herdadas (sem as diretas), por nível: cada uma uma vez, com o caminho mais curto.
/// `seen` corta ciclos. Nomes de role são exatos (case-sensitive) no Snowflake.
pub async fn effective_roles<E>(direct: &[String], mut inherits: impl AsyncFnMut(&str) -> Result<Vec<String>, E>) -> Result<Vec<EffectiveRole>, E> {
    let mut seen: std::collections::HashSet<String> = direct.iter().cloned().collect();
    let mut frontier: Vec<Vec<String>> = direct.iter().map(|r| vec![r.clone()]).collect();
    let mut out = Vec::new();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for path in frontier {
            let last = path.last().expect("caminho nunca vazio").clone();
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

pub async fn users(s: &Session) -> Result<Vec<UserInfo>, DbError> {
    let (n, r) = s.query_named("SHOW USERS").await?;
    Ok(parse_users(&n, &r))
}

pub async fn roles(s: &Session) -> Result<Vec<RoleInfo>, DbError> {
    let (n, r) = s.query_named("SHOW ROLES").await?;
    Ok(parse_roles(&n, &r))
}

pub async fn user_roles(s: &Session, user: &str) -> Result<Vec<String>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS TO USER {}", quote_ident(user))).await?;
    Ok(parse_user_roles(&n, &r))
}

pub async fn role_grants(s: &Session, role: &str) -> Result<Vec<Grant>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS TO ROLE {}", quote_ident(role))).await?;
    Ok(parse_role_grants(&n, &r))
}

pub async fn grantees(s: &Session, role: &str) -> Result<Vec<Grantee>, DbError> {
    let (n, r) = s.query_named(&format!("SHOW GRANTS OF ROLE {}", quote_ident(role))).await?;
    Ok(parse_grantees(&n, &r))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryActivity {
    pub total: u64,
    pub last: Option<String>,
    pub elapsed_ms: u64,
    /// 30 dias, do mais antigo ao mais novo (o servidor gera os dias vazios).
    pub per_day: Vec<(String, u64)>,
    pub top_warehouses: Vec<(String, u64)>,
    pub top_roles: Vec<(String, u64)>,
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
    /// Mais recente primeiro.
    pub recent: Vec<LoginEvent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Activity {
    pub logins: LoginActivity,
    pub queries: QueryActivity,
}

fn in_list(names: &[&str]) -> String {
    names.iter().map(|n| sql_str(n)).collect::<Vec<_>>().join(", ")
}

/// Linhas `kind, k, n, extra`: várias agregações numa ida só ao servidor.
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
union all select 'role', k, n::string, null from rl",
        u = in_list(names)
    )
}

/// Linhas `kind, a..f`: totais + os 5 eventos mais recentes.
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
    for r in rows {
        let k = opt(names, r, "k");
        let n = num(names, r, "n");
        match field(names, r, "kind") {
            Some("total") => (a.total, a.last) = (n, opt(names, r, "extra")),
            Some("ms") => a.elapsed_ms = n,
            Some("day") => a.per_day.extend(k.map(|k| (k, n))),
            Some("wh") => wh.extend(k.map(|k| (k, n))),
            Some("role") => rl.extend(k.map(|k| (k, n))),
            _ => {}
        }
    }
    // UNION ALL não garante ordem: ordena aqui.
    a.per_day.sort();
    (a.top_warehouses, a.top_roles) = (top(wh), top(rl));
    a
}

pub fn parse_login_activity(names: &[String], rows: &Rows) -> LoginActivity {
    let mut l = LoginActivity::default();
    for r in rows {
        match field(names, r, "kind") {
            Some("ok") => l.ok = num(names, r, "a"),
            Some("failed") => l.failed = num(names, r, "a"),
            Some("event") => l.recent.push(LoginEvent {
                at: opt(names, r, "a").unwrap_or_default(),
                success: field(names, r, "b") == Some("YES"),
                factor: opt(names, r, "c"),
                client: opt(names, r, "d"),
                ip: opt(names, r, "e"),
                error: opt(names, r, "f"),
            }),
            _ => {}
        }
    }
    // Mesmo fuso na sessão: a ordem do texto é a ordem do tempo.
    l.recent.sort_by(|a, b| b.at.cmp(&a.at));
    l
}

pub async fn activity(s: &Session, u: &UserInfo) -> Result<Activity, DbError> {
    let mut names = vec![u.name.as_str()];
    if let Some(l) = u.login_name.as_deref().filter(|l| *l != u.name) {
        names.push(l);
    }
    let (n, r) = s.query_named(&login_activity_sql(&names)).await?;
    let logins = parse_login_activity(&n, &r);
    let (n, r) = s.query_named(&query_activity_sql(&names)).await?;
    Ok(Activity { logins, queries: parse_query_activity(&n, &r) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(names: &[&str], rows: &[&[Option<&str>]]) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        (names.iter().map(|s| s.to_string()).collect(), rows.iter().map(|r| r.iter().map(|v| v.map(String::from)).collect()).collect())
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("ANALYST"), "\"ANALYST\"");
        assert_eq!(quote_ident("my role"), "\"my role\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
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
        assert_eq!(u.len(), 2, "linha sem nome é descartada");
        assert_eq!(u[0].name, "JDOE");
        assert_eq!(u[0].default_role.as_deref(), Some("ANALYST"));
        assert_eq!(u[0].last_success_login.as_deref(), Some("2026-10-14 09:00:00"));
        assert!(!u[0].disabled && u[0].has_rsa_public_key && !u[0].locked);
        assert_eq!(u[0].default_warehouse, None, "coluna ausente vira None");
        assert!(u[1].disabled && u[1].locked);
    }

    #[test]
    fn roles_and_user_roles() {
        let (n, r) = t(&["name", "owner", "assigned_to_users", "granted_to_roles", "comment"], &[&[Some("ANALYST"), Some("SECURITYADMIN"), Some("12"), Some("1"), None]]);
        let roles = parse_roles(&n, &r);
        assert_eq!(roles[0], RoleInfo { name: "ANALYST".into(), owner: Some("SECURITYADMIN".into()), comment: None, assigned_to_users: 12, granted_to_roles: 1 });
        let (n, r) = t(&["created_on", "role", "granted_to", "grantee_name"], &[&[None, Some("ANALYST"), Some("USER"), Some("JDOE")], &[None, Some("PUBLIC"), Some("USER"), Some("JDOE")]]);
        assert_eq!(parse_user_roles(&n, &r), vec!["ANALYST", "PUBLIC"]);
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
        assert_eq!(eff(&["ANALYST"], &[("ANALYST", "READER"), ("READER", "PUBLIC")]), vec![
            ("READER".into(), "ANALYST → READER".into()),
            ("PUBLIC".into(), "ANALYST → READER → PUBLIC".into()),
        ]);
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
        assert_eq!(a.per_day, vec![("2026-10-14".into(), 3), ("2026-10-15".into(), 4)], "dias em ordem crescente");
        assert_eq!(a.top_warehouses, vec![("WH_XS".into(), 5), ("WH_S".into(), 2)], "maior primeiro");
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
        assert!(l.recent[0].success && l.recent[0].at.starts_with("2026-10-15"), "mais recente primeiro");
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
        assert_eq!(g[1].name, "DB.S.\"t\"", "objeto que não é role fica como veio");
        let (n, r) = t(&["role", "granted_to", "grantee_name"], &[&[Some("\"low\""), Some("ROLE"), Some("\"a b\"")]]);
        assert_eq!(parse_grantees(&n, &r)[0].name, "a b");
        assert_eq!(parse_user_roles(&n, &r), vec!["low"]);
    }
}
