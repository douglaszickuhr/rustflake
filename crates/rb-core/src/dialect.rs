//! Naming rules that change from one database to another.

use crate::lexer::{ident_text, qualify, quote_ident, quote_ident_snowflake};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    #[default]
    Postgres,
    Snowflake,
}

impl Dialect {
    /// Identifier ready to insert into SQL, quoted only when needed.
    pub fn quote(self, name: &str) -> String {
        match self {
            Dialect::Postgres => quote_ident(name),
            Dialect::Snowflake => quote_ident_snowflake(name),
        }
    }

    /// Name the database sees from the typed token (unquoted folds the case).
    pub fn fold(self, raw: &str) -> String {
        match self {
            Dialect::Postgres => ident_text(raw),
            Dialect::Snowflake if raw.starts_with('"') => ident_text(raw),
            Dialect::Snowflake => raw.to_uppercase(),
        }
    }

    /// `FROM` fully quoted. On Snowflake the schema key is `DB.SCHEMA`.
    /// ponytail: database with "." in the name is not supported.
    pub fn qualified(self, schema_key: &str, table: &str) -> String {
        format!("{}.{}", self.qualified_schema(schema_key), qualify(&[table]))
    }

    /// Schema key quoted: `DB.SCHEMA` on Snowflake becomes `"DB"."SCHEMA"`.
    pub fn qualified_schema(self, schema_key: &str) -> String {
        match (self, schema_key.split_once('.')) {
            (Dialect::Snowflake, Some((db, s))) => qualify(&[db, s]),
            _ => qualify(&[schema_key]),
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
