//! Regras de nome que mudam de um banco para outro.

use crate::lexer::{ident_text, quote_always, quote_ident, quote_ident_snowflake};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    #[default]
    Postgres,
    Snowflake,
}

impl Dialect {
    /// Identificador pronto para inserir no SQL, com aspas só quando preciso.
    pub fn quote(self, name: &str) -> String {
        match self {
            Dialect::Postgres => quote_ident(name),
            Dialect::Snowflake => quote_ident_snowflake(name),
        }
    }

    /// Nome que o banco enxerga a partir do token digitado (sem aspas dobra a caixa).
    pub fn fold(self, raw: &str) -> String {
        match self {
            Dialect::Postgres => ident_text(raw),
            Dialect::Snowflake if raw.starts_with('"') => ident_text(raw),
            Dialect::Snowflake => raw.to_uppercase(),
        }
    }

    /// `FROM` totalmente citado. No Snowflake a chave de schema é `DB.SCHEMA`.
    /// ponytail: database com "." no nome não é suportado.
    pub fn qualified(self, schema_key: &str, table: &str) -> String {
        match (self, schema_key.split_once('.')) {
            (Dialect::Snowflake, Some((db, s))) => format!("{}.{}.{}", quote_always(db), quote_always(s), quote_always(table)),
            _ => format!("{}.{}", quote_always(schema_key), quote_always(table)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_quotes_only_what_would_change_meaning() {
        let d = Dialect::Snowflake;
        assert_eq!(d.quote("ORDERS"), "ORDERS");
        assert_eq!(d.quote("ORDER_2$"), "ORDER_2$");
        assert_eq!(d.quote("orders"), r#""orders""#);
        assert_eq!(d.quote("Order Items"), r#""Order Items""#);
        assert_eq!(d.quote("SELECT"), r#""SELECT""#);
        assert_eq!(d.quote(r#"A"B"#), r#""A""B""#);
        assert_eq!(Dialect::Postgres.quote("orders"), "orders");
    }

    #[test]
    fn fold_follows_each_database() {
        assert_eq!(Dialect::Snowflake.fold("orders"), "ORDERS");
        assert_eq!(Dialect::Snowflake.fold(r#""Order Items""#), "Order Items");
        assert_eq!(Dialect::Postgres.fold("Orders"), "orders");
        assert_eq!(Dialect::Postgres.fold(r#""Orders""#), "Orders");
    }

    #[test]
    fn qualified_names() {
        assert_eq!(Dialect::Snowflake.qualified("DB.PUBLIC", "T x"), r#""DB"."PUBLIC"."T x""#);
        assert_eq!(Dialect::Postgres.qualified("public", "t"), r#""public"."t""#);
    }
}
