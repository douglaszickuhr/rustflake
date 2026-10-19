//! Seletor de role/warehouse/database/schema: o que listar, busca e usados recentemente.

use crate::admin::unquote;

/// Quantos recentes guardar por conexão e tipo.
pub const RECENT_MAX: usize = 5;

/// O escolhido vai para o topo, sem repetir.
pub fn push_recent(list: &mut Vec<String>, v: &str) {
    list.retain(|x| x != v);
    list.insert(0, v.to_string());
    list.truncate(RECENT_MAX);
}

/// Roles: só as que o usuário pode assumir (o `SHOW ROLES` lista a conta inteira, milhares).
pub fn list_sql(kind: &str) -> &'static str {
    match kind {
        "role" => "select current_available_roles()",
        "warehouse" => "show warehouses",
        "database" => "show databases",
        _ => "show schemas",
    }
}

/// `CURRENT_AVAILABLE_ROLES()` devolve um array JSON em texto.
pub fn parse_available_roles(json: &str) -> Vec<String> {
    let mut v: Vec<String> = serde_json::from_str::<Vec<String>>(json).unwrap_or_default().into_iter().map(unquote).collect();
    v.sort_unstable();
    v
}

/// Todas as palavras da busca precisam aparecer; quem começa com a primeira vem antes.
pub fn rank<'a>(all: &'a [String], q: &str) -> Vec<&'a str> {
    let toks: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    let mut hits: Vec<&str> = all.iter().map(String::as_str).filter(|n| crate::unlocks::matches_all(n, &toks)).collect();
    if let Some(first) = toks.first() {
        hits.sort_by_key(|n| !n.to_lowercase().starts_with(first.as_str()));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn recent_moves_to_front_without_duplicates_and_caps() {
        let mut r = s(&["A", "B", "C", "D", "E"]);
        push_recent(&mut r, "C");
        assert_eq!(r, s(&["C", "A", "B", "D", "E"]));
        push_recent(&mut r, "F");
        assert_eq!(r, s(&["F", "C", "A", "B", "D"]), "no máximo 5");
    }

    #[test]
    fn available_roles_come_as_a_json_array() {
        assert_eq!(parse_available_roles(r#"["PUBLIC","DATA_FR","\"low\""]"#), s(&["DATA_FR", "PUBLIC", "low"]));
        assert!(parse_available_roles("lixo").is_empty());
    }

    #[test]
    fn search_puts_prefix_matches_first() {
        let all = s(&["ADMINISTER_DEV_X", "DATA_ENG_FR", "FINANCE_DATA_FR", "ACCOUNTADMIN"]);
        assert_eq!(rank(&all, "data"), vec!["DATA_ENG_FR", "FINANCE_DATA_FR"]);
        assert_eq!(rank(&all, "dev x"), vec!["ADMINISTER_DEV_X"], "todas as palavras");
        assert_eq!(rank(&all, "").len(), 4);
    }

    #[test]
    fn list_sql_per_kind() {
        assert_eq!(list_sql("role"), "select current_available_roles()");
        assert_eq!(list_sql("warehouse"), "show warehouses");
        assert_eq!(list_sql("schema"), "show schemas");
    }
}
