//! "What the person unlocks": roles grouped by warehouse, classification, department and
//! database, with all the paths to each one.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use crate::org_config::OrgConfig;
use crate::admin::RoleKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    Warehouse,
    Classification,
    Department,
    Data,
    Functional,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    None,
    Warehouse { operate: bool, monitor: bool },
    Level(u8),
    Data { write: bool },
}

/// A list row: what is unlocked and the roles that grant it.
#[derive(Debug, Clone, PartialEq)]
pub struct UnlockItem {
    pub kind: ItemKind,
    /// Warehouse without suffix, `C4`, department, database (`""` = unknown) or the role itself.
    pub name: String,
    pub detail: Detail,
    pub roles: Vec<String>,
}

/// `databases`: role → databases where it has USAGE (from ACCOUNT_USAGE or SHOW GRANTS).
pub fn unlock_items(org: &OrgConfig, roles: &[String], databases: &HashMap<String, Vec<String>>) -> Vec<UnlockItem> {
    let mut items: BTreeMap<(ItemKind, std::cmp::Reverse<u8>, String), UnlockItem> = BTreeMap::new();
    let mut add = |kind: ItemKind, order: u8, name: String, detail: Detail, role: &str| {
        let it = items.entry((kind, std::cmp::Reverse(order), name.clone())).or_insert(UnlockItem { kind, name, detail, roles: Vec::new() });
        match (&mut it.detail, detail) {
            (Detail::Warehouse { operate, monitor }, Detail::Warehouse { operate: o, monitor: m }) => (*operate, *monitor) = (*operate || o, *monitor || m),
            (Detail::Data { write }, Detail::Data { write: w }) => *write = *write || w,
            _ => {}
        }
        it.roles.push(role.to_string());
    };
    for r in roles {
        match org.classify(r) {
            RoleKind::Classification(l) => add(ItemKind::Classification, l, format!("C{l}"), Detail::Level(l), r),
            RoleKind::Warehouse => match org.warehouse_of(r) {
                Some((wh, op)) => add(ItemKind::Warehouse, 0, wh, Detail::Warehouse { operate: op, monitor: !op }, r),
                None => add(ItemKind::Other, 0, r.clone(), Detail::None, r),
            },
            RoleKind::Base => add(ItemKind::Department, 0, org.department_of(r), Detail::None, r),
            RoleKind::Access => {
                let write = org.writes(r);
                match databases.get(r).filter(|d| !d.is_empty()) {
                    // Unknown last: "~" sorts after the letters.
                    None => add(ItemKind::Data, 0, "~".into(), Detail::Data { write }, r),
                    Some(dbs) => dbs.iter().for_each(|db| add(ItemKind::Data, 0, db.clone(), Detail::Data { write }, r)),
                }
            }
            RoleKind::Functional => add(ItemKind::Functional, 0, r.clone(), Detail::None, r),
            RoleKind::Other => add(ItemKind::Other, 0, r.clone(), Detail::None, r),
        }
    }
    items
        .into_values()
        .map(|mut it| {
            if it.name == "~" {
                it.name.clear();
            }
            it.roles.sort_unstable();
            it
        })
        .collect()
}

/// All paths (up to `limit`, shortest first) from some direct role to `target`.
pub fn all_paths(direct: &[String], inherits: &HashMap<String, Vec<String>>, target: &str, limit: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut queue: VecDeque<Vec<String>> = direct.iter().map(|d| vec![d.clone()]).collect();
    // ponytail: breadth-first search with a step cap; huge hierarchies stop at the limit.
    let mut steps = 0;
    while let Some(path) = queue.pop_front() {
        steps += 1;
        if out.len() >= limit || steps > 50_000 {
            break;
        }
        let last = path.last().expect("path is never empty");
        if last == target {
            out.push(path);
            continue;
        }
        for c in inherits.get(last).into_iter().flatten() {
            if !path.contains(c) {
                let mut p = path.clone();
                p.push(c.clone());
                queue.push_back(p);
            }
        }
    }
    out
}

/// Search tokens (lowercase, space-separated).
pub fn tokens(q: &str) -> Vec<String> {
    q.split_whitespace().map(str::to_lowercase).collect()
}

pub fn matches_all(text: &str, tokens: &[String]) -> bool {
    let t = text.to_lowercase();
    tokens.iter().all(|k| t.contains(k.as_str()))
}

/// Spans of `text` to highlight (each occurrence of each token), sorted and non-overlapping.
pub fn match_ranges(text: &str, tokens: &[String]) -> Vec<std::ops::Range<usize>> {
    let lower = text.to_lowercase();
    // ASCII only in Snowflake names: indexes of the lowercase version hold in the original.
    if lower.len() != text.len() {
        return Vec::new();
    }
    let mut marks = vec![false; text.len()];
    for k in tokens.iter().filter(|k| !k.is_empty()) {
        let mut from = 0;
        while let Some(i) = lower[from..].find(k.as_str()) {
            marks[from + i..from + i + k.len()].iter_mut().for_each(|m| *m = true);
            from += i + k.len();
        }
    }
    let mut out = Vec::new();
    let mut start = None;
    for (i, m) in marks.iter().chain([&false]).enumerate() {
        match (start, *m) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// For the header: inherited roles that are data roles and in how many distinct databases.
pub fn data_spread(items: &[UnlockItem]) -> (usize, usize) {
    let data: Vec<&UnlockItem> = items.iter().filter(|i| i.kind == ItemKind::Data).collect();
    let roles: HashSet<&str> = data.iter().flat_map(|i| i.roles.iter().map(String::as_str)).collect();
    (roles.len(), data.iter().filter(|i| !i.name.is_empty()).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn dbs(map: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        map.iter().map(|(k, v)| (k.to_string(), s(v))).collect()
    }

    #[test]
    fn roles_become_what_they_unlock() {
        let roles = s(&[
            "GENERAL_XS_WH_OP_AR",
            "GENERAL_XS_WH_MONITOR_AR",
            "CLASSIFICATION_C2_MASKING_AR",
            "LAKE_RAW_HR_C4_MASKING_AR",
            "FINANCE_DEPARTMENT_BASE_ROLE",
            "SALES_PROD_MART_CX_RO_AR",
            "SALES_PROD_MART_CRM_RW_AR",
            "RAW_CRM_RO_AR",
            "FINANCE_GENERAL_AREA_FR",
            "PUBLIC",
        ]);
        let d = dbs(&[("SALES_PROD_MART_CX_RO_AR", &["SALES_PROD"]), ("SALES_PROD_MART_CRM_RW_AR", &["SALES_PROD"])]);
        let items = unlock_items(&crate::org_config::acme(), &roles, &d);
        let got: Vec<(ItemKind, &str, Detail, usize)> = items.iter().map(|i| (i.kind, i.name.as_str(), i.detail, i.roles.len())).collect();
        assert_eq!(
            got,
            vec![
                (ItemKind::Warehouse, "GENERAL_XS_WH", Detail::Warehouse { operate: true, monitor: true }, 2),
                (ItemKind::Classification, "C4", Detail::Level(4), 1),
                (ItemKind::Classification, "C2", Detail::Level(2), 1),
                (ItemKind::Department, "FINANCE", Detail::None, 1),
                (ItemKind::Data, "SALES_PROD", Detail::Data { write: true }, 2),
                (ItemKind::Data, "", Detail::Data { write: false }, 1),
                (ItemKind::Functional, "FINANCE_GENERAL_AREA_FR", Detail::None, 1),
                (ItemKind::Other, "PUBLIC", Detail::None, 1),
            ],
            "classification from highest to lowest; unknown database last"
        );
    }

    #[test]
    fn every_path_to_a_role_up_to_a_limit() {
        let g: HashMap<String, Vec<String>> = [("IDP", vec!["AREA", "SB"]), ("AREA", vec!["BASE", "X"]), ("BASE", vec!["X"]), ("SB", vec!["X"]), ("X", vec!["IDP"])]
            .into_iter()
            .map(|(k, v)| (k.to_string(), s(&v)))
            .collect();
        let p: Vec<String> = all_paths(&s(&["IDP"]), &g, "X", 10).into_iter().map(|p| p.join(">")).collect();
        assert_eq!(p, vec!["IDP>AREA>X", "IDP>SB>X", "IDP>AREA>BASE>X"], "shortest first; the X→IDP cycle is ignored");
        assert_eq!(all_paths(&s(&["IDP"]), &g, "X", 1).len(), 1);
        assert_eq!(all_paths(&s(&["IDP"]), &g, "IDP", 5), vec![s(&["IDP"])], "direct: the path is the role itself");
    }

    #[test]
    fn search_ranges_cover_every_token() {
        assert_eq!(match_ranges("SALES_PROD_BILLING", &s(&["billing", "sale"])), vec![0..4, 11..18]);
        assert!(match_ranges("abc", &[]).is_empty());
        assert!(matches_all("SALES_PROD read", &s(&["sale", "read"])));
        assert!(!matches_all("SALES_PROD", &s(&["sale", "write"])));
    }

    #[test]
    fn classification_beats_warehouse() {
        let items = unlock_items(&crate::org_config::acme(), &s(&["X_C3_WH_OP_AR"]), &HashMap::new());
        assert_eq!((items[0].kind, items[0].detail), (ItemKind::Classification, Detail::Level(3)));
    }
}
