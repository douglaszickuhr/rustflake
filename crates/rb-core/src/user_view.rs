//! Pure pieces of the user page: relative time, list row, grouped logins, filtered tree.

use crate::admin::{LoginEvent, TreeRow, UserInfo};

/// `YYYY-MM-DD HH:MM:SS…` → Unix seconds. ponytail: ignores the time zone (Snowflake returns UTC here).
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // Days since 1970-01-01 (Howard Hinnant's civil algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Interval in the largest unit that fits; the screen translates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    Minutes(u64),
    Hours(u64),
    Days(u64),
    Months(u64),
    Years(u64),
}

pub fn span(secs: i64) -> Span {
    let s = secs.max(0) as u64;
    let d = s / 86_400;
    if s < 3600 {
        Span::Minutes(s / 60)
    } else if s < 86_400 {
        Span::Hours(s / 3600)
    } else if d < 60 {
        Span::Days(d)
    } else if d < 365 {
        Span::Months(d / 30)
    } else {
        Span::Years(d / 365)
    }
}

/// No login for longer than this: orange highlight in the list.
pub const IDLE_DAYS: i64 = 90;

/// Second line of each user in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserMeta {
    Disabled,
    Service { key_pair: bool },
    NeverLoggedIn { age: Span },
    Idle { days: i64 },
    Active { since: Span },
}

pub fn user_meta(u: &UserInfo, now: i64) -> UserMeta {
    if u.disabled {
        return UserMeta::Disabled;
    }
    if u.user_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("SERVICE") || t.eq_ignore_ascii_case("LEGACY_SERVICE")) {
        return UserMeta::Service { key_pair: u.has_rsa_public_key };
    }
    match u.last_success_login.as_deref().and_then(parse_ts) {
        None => UserMeta::NeverLoggedIn { age: span(u.created_on.as_deref().and_then(parse_ts).map_or(0, |c| now - c)) },
        Some(t) if (now - t) / 86_400 > IDLE_DAYS => UserMeta::Idle { days: (now - t) / 86_400 },
        Some(t) => UserMeta::Active { since: span(now - t) },
    }
}

/// Consecutive identical logins (result, method, client, IP) become one row with ×n; starts from the most recent.
pub fn group_sign_ins(events: &[LoginEvent]) -> Vec<(LoginEvent, usize)> {
    let mut out: Vec<(LoginEvent, usize)> = Vec::new();
    for e in events {
        match out.last_mut() {
            Some((g, n)) if g.success == e.success && g.factor == e.factor && g.client == e.client && g.ip == e.ip => *n += 1,
            _ => out.push((e.clone(), 1)),
        }
    }
    out
}

/// 0 = none, 1 = few, 2 = some, 3 = many (day band color).
pub fn intensity(n: u64) -> u8 {
    match n {
        0 => 0,
        1..=4 => 1,
        5..=49 => 2,
        _ => 3,
    }
}

/// For each visible row: is it the parent's last child? (`└` instead of `├`).
pub fn last_child(rows: &[&TreeRow]) -> Vec<bool> {
    (0..rows.len())
        .map(|i| {
            let d = rows[i].depth;
            rows[i + 1..].iter().find(|r| r.depth <= d).is_none_or(|r| r.depth < d)
        })
        .collect()
}

/// Provisioned by SCIM: the owner is the IdP provisioner's role (`OKTA_PROVISIONER`,
/// `AAD_PROVISIONER`, `GENERIC_SCIM_PROVISIONER`).
pub fn scim_managed(owner: Option<&str>) -> bool {
    owner.is_some_and(|o| o.to_ascii_uppercase().ends_with("_PROVISIONER"))
}

/// Login methods: SSO comes from recent logins (SHOW USERS does not say).
pub fn auth_summary(u: &UserInfo, recent: &[LoginEvent]) -> Vec<&'static str> {
    let mut out = Vec::new();
    let sso = recent.iter().any(|e| e.success && e.factor.as_deref().is_some_and(|f| f.contains("SAML") || f.contains("OAUTH")));
    if sso {
        out.push("SSO");
    }
    if u.has_password {
        out.push("password");
    }
    if u.has_rsa_public_key {
        out.push("key-pair");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{LoginEvent, TreeRow, UserInfo};

    const DAY: i64 = 86_400;

    #[test]
    fn timestamps_parse_as_utc_seconds() {
        assert_eq!(parse_ts("1970-01-02 00:00:00.000Z"), Some(DAY));
        assert_eq!(parse_ts("2026-10-15 16:57:43.398 Z"), parse_ts("2026-10-15 16:57:43"));
        assert_eq!(parse_ts("2000-03-01 00:00:00").unwrap() - parse_ts("2000-02-28 00:00:00").unwrap(), 2 * DAY, "2000 is a leap year");
        assert_eq!(parse_ts("garbage"), None);
    }

    #[test]
    fn spans_pick_the_largest_unit() {
        assert_eq!(span(30), Span::Minutes(0));
        assert_eq!(span(4 * 3600 + 10), Span::Hours(4));
        assert_eq!(span(2 * DAY + 5), Span::Days(2));
        assert_eq!(span(130 * DAY), Span::Months(4));
        assert_eq!(span(800 * DAY), Span::Years(2));
    }

    fn user(f: impl FnOnce(&mut UserInfo)) -> UserInfo {
        let mut u = UserInfo { name: "U".into(), created_on: Some("2026-01-01 00:00:00".into()), ..Default::default() };
        f(&mut u);
        u
    }

    #[test]
    fn list_meta_describes_each_user() {
        let now = parse_ts("2026-10-16 00:00:00").unwrap();
        assert_eq!(user_meta(&user(|u| u.disabled = true), now), UserMeta::Disabled);
        assert_eq!(user_meta(&user(|u| u.user_type = Some("SERVICE".into())), now), UserMeta::Service { key_pair: false });
        assert_eq!(user_meta(&user(|_| {}), now), UserMeta::NeverLoggedIn { age: Span::Months(9) });
        assert_eq!(user_meta(&user(|u| u.last_success_login = Some("2026-07-01 00:00:00".into())), now), UserMeta::Idle { days: 107 });
        assert_eq!(user_meta(&user(|u| u.last_success_login = Some("2026-10-15 20:00:00".into())), now), UserMeta::Active { since: Span::Hours(4) });
    }

    fn ev(at: &str, ok: bool, ip: &str) -> LoginEvent {
        LoginEvent { at: at.into(), success: ok, factor: Some("OAUTH".into()), client: Some("JDBC".into()), ip: Some(ip.into()), error: None }
    }

    #[test]
    fn consecutive_identical_sign_ins_collapse() {
        let g = group_sign_ins(&[ev("3", true, "a"), ev("2", true, "a"), ev("1", true, "b"), ev("0", false, "b")]);
        let got: Vec<(&str, usize)> = g.iter().map(|(e, n)| (e.at.as_str(), *n)).collect();
        assert_eq!(got, vec![("3", 2), ("1", 1), ("0", 1)], "the most recent represents the group; a failure does not merge with a success");
    }

    #[test]
    fn intensity_levels() {
        assert_eq!([0, 1, 4, 5, 49, 50, 690].map(intensity), [0, 1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn connectors_mark_the_last_child() {
        let r = |n: &str, d| TreeRow { name: n.into(), depth: d, descendants: 0 };
        let rows = [r("A", 0), r("B", 1), r("C", 2), r("D", 1), r("E", 0)];
        let refs: Vec<&TreeRow> = rows.iter().collect();
        assert_eq!(last_child(&refs), vec![false, false, true, true, true]);
    }

    #[test]
    fn scim_and_sso_detection() {
        assert!(scim_managed(Some("OKTA_PROVISIONER")) && scim_managed(Some("aad_provisioner")));
        assert!(!scim_managed(Some("OKTA_ADMIN")) && !scim_managed(Some("ACCOUNTADMIN")) && !scim_managed(None));
        let u = user(|u| u.user_type = Some("PERSON".into()));
        assert_eq!(auth_summary(&u, &[ev("1", true, "a")]), vec!["SSO"]);
        let k = user(|u| u.has_rsa_public_key = true);
        assert_eq!(auth_summary(&k, &[]), vec!["key-pair"]);
    }
}
