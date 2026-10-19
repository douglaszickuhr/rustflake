//! Translation guards: key used without an entry, missing language, variable differing between languages.

use std::collections::BTreeSet;
use std::path::Path;

const LANGS: [&str; 3] = ["en", "pt", "es"];

/// Keys of `t!("…"` in all `.rs` files of `dir`.
fn used_keys(dir: &Path, out: &mut BTreeSet<String>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            used_keys(&p, out);
        // This file mentions the pattern it searches for; it does not count itself.
        } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n_check.rs") {
            let src = std::fs::read_to_string(&p).unwrap();
            // `t!("` bare: `format!("` and the like end the same way and do not count.
            for (i, _) in src.match_indices("t!(\"") {
                if src[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let rest = &src[i + 4..];
                out.insert(rest.split('"').next().unwrap().to_string());
            }
        }
    }
}

fn vars(s: &str) -> BTreeSet<&str> {
    s.split("%{").skip(1).filter_map(|p| p.split('}').next()).collect()
}

/// Leaves `dotted.key -> {en, pt, es}` of TOML v2.
fn entries(v: &toml::Table, prefix: &str, out: &mut Vec<(String, toml::Table)>) {
    for (k, v) in v {
        let Some(t) = v.as_table() else { continue };
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        if t.contains_key("en") {
            out.push((key, t.clone()));
        } else {
            entries(t, &key, out);
        }
    }
}

pub fn check(src: &Path, toml_path: &Path) {
    let file: toml::Table = std::fs::read_to_string(toml_path).unwrap().parse().unwrap();
    let mut all = Vec::new();
    entries(&file, "", &mut all);
    let mut problems = Vec::new();
    for (key, t) in &all {
        let en = t.get("en").and_then(|v| v.as_str()).unwrap_or("");
        for l in LANGS {
            match t.get(l).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() && vars(s) == vars(en) => {}
                _ => problems.push(format!("{key}: `{l}` missing, empty or with variables different from `en`")),
            }
        }
    }
    let defined: BTreeSet<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
    let mut used = BTreeSet::new();
    used_keys(src, &mut used);
    problems.extend(used.iter().filter(|k| !defined.contains(k.as_str())).map(|k| format!("{k}: used in code, missing from the TOML")));
    assert!(problems.is_empty(), "{}:\n{}", toml_path.display(), problems.join("\n"));
}
