//! Conventions of a company (role names, classification tag, masking policies) in a TOML
//! file chosen per connection. Without a config the app assumes no convention.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use rust_i18n::t;
use serde::Deserialize;

use crate::admin::{RoleKind, sql_str};
use crate::masking::{Scope, TagCount};
use crate::metadata::glob_match;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    kind: String,
    #[serde(rename = "match")]
    pattern: String,
    write: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClass {
    tag: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    values: HashMap<String, u8>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub role: String,
    pub databases: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Masking {
    global: Option<String>,
    group: Option<Group>,
    scoped_suffix: Option<String>,
    #[serde(default)]
    forced_by: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrgConfig {
    name: String,
    #[serde(default)]
    roles: Vec<RawRule>,
    classification: Option<RawClass>,
    masking: Option<Masking>,
}

#[derive(Debug)]
struct Rule {
    /// `Classification(0)`: the level comes from the `level` group.
    kind: RoleKind,
    re: Regex,
    write: Option<String>,
}

#[derive(Debug)]
struct Classification {
    /// db, schema, name.
    tag: [String; 3],
    labels: Vec<String>,
    values: HashMap<String, u8>,
}

#[derive(Debug, Default)]
pub struct OrgConfig {
    name: String,
    rules: Vec<Rule>,
    class: Option<Classification>,
    masking: Masking,
}

impl OrgConfig {
    pub fn none() -> OrgConfig {
        OrgConfig::default()
    }

    pub fn parse(src: &str) -> Result<OrgConfig, String> {
        let raw: RawOrgConfig = toml::from_str(src).map_err(|e| e.to_string())?;
        if raw.name.trim().is_empty() {
            return Err(t!("err.config_name").to_string());
        }
        let mut rules = Vec::new();
        for r in raw.roles {
            let (kind, need) = match r.kind.as_str() {
                "classification" => (RoleKind::Classification(0), Some("level")),
                "warehouse" => (RoleKind::Warehouse, Some("warehouse")),
                "base" => (RoleKind::Base, None),
                "functional" => (RoleKind::Functional, None),
                "access" => (RoleKind::Access, None),
                k => return Err(t!("err.config_kind", kind = k).to_string()),
            };
            let re = RegexBuilder::new(&r.pattern).case_insensitive(true).build().map_err(|e| t!("err.config_regex", kind = r.kind, error = e).to_string())?;
            if let Some(g) = need.filter(|g| !re.capture_names().flatten().any(|n| n == *g)) {
                return Err(t!("err.config_group", kind = r.kind, group = g).to_string());
            }
            rules.push(Rule { kind, re, write: r.write.map(|w| w.to_ascii_uppercase()) });
        }
        let class = match raw.classification {
            None => None,
            Some(c) => {
                let parts: Vec<&str> = c.tag.split('.').collect();
                let [db, schema, name] = parts.as_slice() else { return Err(t!("err.config_tag").to_string()) };
                if [db, schema, name].iter().any(|p| p.trim().is_empty()) {
                    return Err(t!("err.config_tag").to_string());
                }
                Some(Classification { tag: [db.to_string(), schema.to_string(), name.to_string()], labels: c.labels, values: c.values })
            }
        };
        Ok(OrgConfig { name: raw.name, rules, class, masking: raw.masking.unwrap_or_default() })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// First matching rule (a classification with `level` outside u8 doesn't match).
    fn matched<'a>(&'a self, role: &'a str) -> Option<(&'a Rule, regex::Captures<'a>, RoleKind)> {
        self.rules.iter().find_map(|r| {
            let c = r.re.captures(role)?;
            let kind = match r.kind {
                RoleKind::Classification(_) => RoleKind::Classification(c.name("level")?.as_str().parse().ok()?),
                k => k,
            };
            Some((r, c, kind))
        })
    }

    pub fn classify(&self, role: &str) -> RoleKind {
        self.matched(role).map_or(RoleKind::Other, |(_, _, k)| k)
    }

    /// `X_WH_OP_AR` → (`X_WH`, operar?).
    pub fn warehouse_of(&self, role: &str) -> Option<(String, bool)> {
        let (_, c, kind) = self.matched(role)?;
        // The group exists in the regex but may not take part in the match (alternation).
        let w = c.name("warehouse")?;
        (kind == RoleKind::Warehouse).then(|| (w.as_str().to_string(), c.name("op").is_some()))
    }

    /// Department of a base role; without the `department` group, the role itself.
    pub fn department_of(&self, role: &str) -> String {
        self.matched(role).and_then(|(_, c, _)| c.name("department").map(|m| m.as_str().to_string())).unwrap_or_else(|| role.to_string())
    }

    pub fn writes(&self, role: &str) -> bool {
        self.matched(role).and_then(|(r, _, _)| r.write.as_deref()).is_some_and(|w| role.to_ascii_uppercase().contains(w))
    }

    /// Some rule produces this kind (only the variant counts).
    pub fn has_kind(&self, k: RoleKind) -> bool {
        self.rules.iter().any(|r| std::mem::discriminant(&r.kind) == std::mem::discriminant(&k))
    }

    /// Has a classification tag (otherwise the screens don't talk about classification).
    pub fn has_classification(&self) -> bool {
        self.class.is_some()
    }

    pub fn level_label(&self, level: u8) -> Option<&str> {
        self.class.as_ref()?.labels.get(usize::from(level).checked_sub(1)?).map(String::as_str)
    }

    /// The highest level (the one that `forced_by` roles always mask).
    pub fn max_level(&self) -> Option<u8> {
        self.class.as_ref().and_then(|c| u8::try_from(c.labels.len()).ok()).filter(|&n| n > 0)
    }

    pub fn tag_level(&self, value: &str) -> Option<u8> {
        self.class.as_ref()?.values.get(value).copied()
    }

    pub fn is_class_tag(&self, db: &str, schema: &str, name: &str) -> bool {
        self.class.as_ref().is_some_and(|c| [db, schema, name].iter().zip(&c.tag).all(|(a, b)| a.eq_ignore_ascii_case(b)))
    }

    /// Where a classification role unmasks. `databases`: the longest that matches the start wins.
    pub fn scope(&self, role: &str, level: u8, databases: &[String]) -> Scope {
        let r = role.to_ascii_uppercase();
        let fill = |t: &str| t.replace("{level}", &level.to_string()).to_ascii_uppercase();
        let m = &self.masking;
        if m.global.as_deref().is_some_and(|g| r == fill(g)) {
            return Scope::Global;
        }
        if m.group.as_ref().is_some_and(|g| r == fill(&g.role)) {
            return Scope::Group;
        }
        let Some(base) = m.scoped_suffix.as_deref().and_then(|s| r.strip_suffix(&fill(s))) else { return Scope::Unresolved(role.to_string()) };
        let mut best: Option<&String> = None;
        for db in databases {
            let d = db.to_ascii_uppercase();
            if (base == d || base.starts_with(&format!("{d}_"))) && best.is_none_or(|b| b.len() < db.len()) {
                best = Some(db);
            }
        }
        match best {
            Some(db) if base.len() == db.len() => Scope::Database(db.clone()),
            Some(db) => Scope::Schema(db.clone(), base[db.len() + 1..].to_string()),
            None => Scope::Unresolved(base.to_string()),
        }
    }

    /// Columns of the level that the role unmasks in the scope.
    pub fn columns(&self, counts: &[TagCount], level: u8, scope: &Scope) -> u64 {
        counts
            .iter()
            .filter(|c| self.tag_level(&c.value) == Some(level))
            .filter(|c| match scope {
                Scope::Global => true,
                Scope::Group => self.group_glob().is_some_and(|g| glob_match(g, &c.database)),
                Scope::Database(d) => &c.database == d,
                Scope::Schema(d, s) => &c.database == d && &c.schema == s,
                Scope::Unresolved(_) => false,
            })
            .map(|c| c.columns)
            .sum()
    }

    /// Has some masking rule (otherwise there is no "where it unmasks").
    pub fn has_masking(&self) -> bool {
        let m = &self.masking;
        m.global.is_some() || m.group.is_some() || m.scoped_suffix.is_some()
    }

    pub fn group_glob(&self) -> Option<&str> {
        self.masking.group.as_ref().map(|g| g.databases.as_str())
    }

    /// The person's roles that force masking; empty = none.
    pub fn forces_masking(&self, roles: &[String]) -> Vec<String> {
        roles.iter().filter(|r| self.masking.forced_by.iter().any(|f| f.eq_ignore_ascii_case(r))).cloned().collect()
    }

    /// Columns with the classification tag, aggregated on the server (~1–2 thousand rows).
    pub fn tag_counts_sql(&self) -> Option<String> {
        let [db, schema, name] = &self.class.as_ref()?.tag;
        Some(format!(
            "select object_database as database, object_schema as schema, tag_value as value, count(*)::string as columns
from snowflake.account_usage.tag_references
where tag_database = {} and tag_schema = {} and tag_name = {}
  and domain = 'COLUMN' and object_deleted is null
group by 1, 2, 3",
            sql_str(&db.to_ascii_uppercase()),
            sql_str(&schema.to_ascii_uppercase()),
            sql_str(&name.to_ascii_uppercase())
        ))
    }
}

pub fn org_configs_dir() -> PathBuf {
    crate::config::app_dir().join("configurations")
}

/// `*.toml` in alphabetical order; repeated name: the first wins and the other becomes an error.
pub fn load_all(dir: &Path) -> (Vec<OrgConfig>, Vec<String>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
    files.sort();
    let (mut out, mut errs) = (Vec::<OrgConfig>::new(), Vec::new());
    for f in files {
        let file = f.file_name().unwrap_or_default().to_string_lossy().to_string();
        match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|s| OrgConfig::parse(&s)) {
            Ok(a) if out.iter().any(|o| o.name.eq_ignore_ascii_case(&a.name)) => errs.push(format!("{file}: {}", t!("err.config_duplicate", name = a.name))),
            Ok(a) => out.push(a),
            Err(e) => errs.push(format!("{file}: {e}")),
        }
    }
    (out, errs)
}

/// The connection's config; missing name = none. Error = the name didn't load (broken or missing file).
pub fn resolve(name: Option<&str>) -> Result<OrgConfig, String> {
    let Some(name) = name.filter(|n| !n.is_empty()) else { return Ok(OrgConfig::none()) };
    let (all, errs) = load_all(&org_configs_dir());
    all.into_iter().find(|a| a.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
        let msg = t!("err.config_not_found", name = name).to_string();
        if errs.is_empty() { msg } else { format!("{msg}\n{}", errs.join("\n")) }
    })
}

#[cfg(test)]
pub(crate) fn acme() -> OrgConfig {
    OrgConfig::parse(include_str!("../tests/fixtures/acme.toml")).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::RoleKind::*;

    #[test]
    fn roles_by_rule_first_match_wins() {
        let a = acme();
        for (name, kind) in [
            ("CLASSIFICATION_C2_MASKING_AR", Classification(2)),
            ("SALES_RAW_C4_MASKING_AR", Classification(4)),
            ("c3_lower_ar", Classification(3)),
            ("GENERAL_XS_WH_OP_AR", Warehouse),
            ("GENERAL_XS_WH_MONITOR_AR", Warehouse),
            ("GENERAL_XS_WH_FULL_AR", Access),
            ("X_WH_USAGE_AR", Access),
            ("FINANCE_DEPARTMENT_BASE_ROLE", Base),
            ("DATA_UNIT_BASE_ROLE", Base),
            ("FINANCE_GENERAL_AREA_FR", Functional),
            ("RAW_CRM_RO_AR", Access),
            ("CC2_AR", Access),
            ("ACCOUNTADMIN", Other),
        ] {
            assert_eq!(a.classify(name), kind, "{name}");
        }
        assert_eq!(a.classify("X_C999_AR"), Access, "level outside u8: the rule does not match");
    }

    #[test]
    fn warehouse_operate_or_monitor() {
        let a = acme();
        assert_eq!(a.warehouse_of("GENERAL_XS_WH_OP_AR"), Some(("GENERAL_XS_WH".into(), true)));
        assert_eq!(a.warehouse_of("ops_xs_wh_monitor_ar"), Some(("ops_xs_wh".into(), false)), "OPS in the name is not operate");
        assert_eq!(a.warehouse_of("GENERAL_XS_WH_FULL_AR"), None);
        assert_eq!(a.department_of("FINANCE_DEPARTMENT_BASE_ROLE"), "FINANCE");
        assert_eq!(a.department_of("DATA_UNIT_BASE_ROLE"), "DATA_UNIT_BASE_ROLE");
        assert!(a.writes("SALES_PROD_CRM_RW_AR") && !a.writes("SALES_PROD_CRM_RO_AR"));
        let alt = OrgConfig::parse("name = 'X'\n[[roles]]\nkind = 'warehouse'\nmatch = '^(?P<warehouse>.+_WH)_OP|^WH_MONITOR'").unwrap();
        assert_eq!(alt.classify("WH_MONITOR"), Warehouse);
        assert_eq!(alt.warehouse_of("WH_MONITOR"), None, "group outside the match: no panic");
    }

    #[test]
    fn no_config_knows_nothing() {
        let n = OrgConfig::none();
        assert_eq!(n.classify("ANY_C4_FR"), Other);
        assert!(acme().has_masking() && !OrgConfig::parse("name = 'X'\n[classification]\ntag = 'A.B.C'").unwrap().has_masking());
        assert!(!n.has_kind(Warehouse) && !n.has_masking() && !n.has_classification() && n.tag_counts_sql().is_none() && n.max_level().is_none());
        assert!(n.forces_masking(&["AI_AGENT_FR".into()]).is_empty());
        assert_eq!(n.scope("CLASSIFICATION_C2_MASKING_AR", 2, &[]), Scope::Unresolved("CLASSIFICATION_C2_MASKING_AR".into()));
        assert!(acme().has_kind(Classification(9)) && acme().has_kind(Base));
    }

    #[test]
    fn invalid_files_are_rejected() {
        let e = |s: &str| OrgConfig::parse(s).unwrap_err();
        assert!(e("name = 'X'\n[[roles]]\nkind = 'classification'\nmatch = '_C\\d+'").contains("level"));
        assert!(e("name = 'X'\n[[roles]]\nkind = 'warehouse'\nmatch = '_WH'").contains("warehouse"));
        assert!(e("name = 'X'\n[[roles]]\nkind = 'access'\nmatch = '('").contains("regex"));
        assert!(e("name = 'X'\n[[roles]]\nkind = 'team'\nmatch = 'x'").contains("team"));
        assert!(e("name = 'X'\n[classification]\ntag = 'ONLY.TWO'").contains("DB.SCHEMA"));
        assert!(e("name = ' '").contains("name"));
    }

    #[test]
    fn classification_tag_and_levels() {
        let a = acme();
        assert_eq!(a.level_label(1), Some("Public"));
        assert_eq!(a.level_label(4), Some("Restricted"));
        assert_eq!((a.level_label(0), a.level_label(7)), (None, None));
        assert_eq!(a.max_level(), Some(4));
        assert_eq!((a.tag_level("C-2 Internal"), a.tag_level("UNCLASSIFIED"), a.tag_level("UNUSED")), (Some(2), Some(4), None));
        assert!(a.is_class_tag("governance", "TAGS", "data_classification"));
        assert!(!a.is_class_tag("OTHER", "TAGS", "DATA_CLASSIFICATION"));
        let s = a.tag_counts_sql().unwrap();
        assert!(s.contains("tag_name = 'DATA_CLASSIFICATION'") && s.contains("group by"), "{s}");
        let q = OrgConfig::parse("name = 'X'\n[classification]\ntag = \"A.B.C'D\"").unwrap().tag_counts_sql().unwrap();
        assert!(q.contains("'C''D'"), "{q}");
    }

    fn dbs() -> Vec<String> {
        ["SALES_PROD", "SALES", "REPORTING_PROD", "LAKE_RAW", "FINANCE_SHARED"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scope_follows_the_masking_rules() {
        let (a, d) = (acme(), dbs());
        assert_eq!(a.scope("classification_c2_masking_ar", 2, &d), Scope::Global);
        assert_eq!(a.scope("SHARED_C3_MASKING_AR", 3, &d), Scope::Group);
        assert_eq!(a.scope("SALES_PROD_C4_MASKING_AR", 4, &d), Scope::Database("SALES_PROD".into()));
        assert_eq!(a.scope("SALES_PROD_MART_CRM_C4_MASKING_AR", 4, &d), Scope::Schema("SALES_PROD".into(), "MART_CRM".into()), "longest matching database");
        assert_eq!(a.scope("LAKE_RAW_HR_ABSENCE_C4_MASKING_AR", 4, &d), Scope::Schema("LAKE_RAW".into(), "HR_ABSENCE".into()));
        assert_eq!(a.scope("UNKNOWN_DB_X_C4_MASKING_AR", 4, &d), Scope::Unresolved("UNKNOWN_DB_X".into()));
        assert_eq!(a.scope("SOMETHING_C4_POLICY_READER", 4, &d), Scope::Unresolved("SOMETHING_C4_POLICY_READER".into()));
    }

    #[test]
    fn columns_per_scope_and_level() {
        let a = acme();
        let c = |db: &str, sc: &str, v: &str, n| TagCount { database: db.into(), schema: sc.into(), value: v.into(), columns: n };
        let c = vec![
            c("SALES_PROD", "MART_CRM", "C-4 Restricted", 90),
            c("SALES_PROD", "MART_CRM", "UNCLASSIFIED", 6),
            c("SALES_PROD", "MART_CX", "C-4 Restricted", 10),
            c("REPORTING_PROD", "RPT_CX", "C-3 Confidential", 88),
            c("FINANCE_SHARED", "PUBLIC", "C-3 Confidential", 5),
        ];
        assert_eq!(a.columns(&c, 4, &Scope::Schema("SALES_PROD".into(), "MART_CRM".into())), 96, "C4 also covers UNCLASSIFIED/CONFLICT");
        assert_eq!(a.columns(&c, 4, &Scope::Database("SALES_PROD".into())), 106);
        assert_eq!(a.columns(&c, 4, &Scope::Global), 106);
        assert_eq!(a.columns(&c, 3, &Scope::Global), 93);
        assert_eq!(a.columns(&c, 3, &Scope::Group), 5);
        assert_eq!(a.columns(&c, 3, &Scope::Unresolved("X".into())), 0);
    }

    #[test]
    fn forced_masking_lists_the_held_roles() {
        let a = acme();
        assert_eq!(a.forces_masking(&["X".into(), "ai_agent_fr".into()]), vec!["ai_agent_fr".to_string()]);
        assert!(a.forces_masking(&["X".into()]).is_empty());
    }

    #[test]
    fn load_all_skips_broken_and_duplicate_files() {
        let dir = std::env::temp_dir().join(format!("rb-configs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.toml"), "name = 'ACME'").unwrap();
        std::fs::write(dir.join("b.toml"), "name = 'acme'").unwrap();
        std::fs::write(dir.join("c.toml"), "name = ").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let (ok, errs) = load_all(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(ok.iter().map(OrgConfig::name).collect::<Vec<_>>(), vec!["ACME"]);
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].starts_with("b.toml") && errs[1].starts_with("c.toml"));
        assert!(load_all(Path::new("/nonexistent/rb")).0.is_empty());
    }
}

