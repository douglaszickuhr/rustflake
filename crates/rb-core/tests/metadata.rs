mod common;

use pollster::block_on;
use rb_core::lexer::quote_ident;
use rb_core::metadata;

#[test]
fn catalog_queries_cover_tree_and_autocomplete() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let schema = common::unique("RbMeta");
    let q = quote_ident(&schema);
    block_on(async {
        for sql in [
            format!("create schema {q}"),
            format!(
                "create table {q}.\"Order Items\" (id int primary key, \"Name\" text not null, total numeric default 0, twice int generated always as (id * 2) stored)"
            ),
            format!("create index items_name on {q}.\"Order Items\" (\"Name\")"),
            format!("create view {q}.v_items as select id from {q}.\"Order Items\""),
            format!("create materialized view {q}.mv_items as select id from {q}.\"Order Items\""),
            format!("create function {q}.add_one(a integer) returns integer language sql as $$ select a + 1 $$"),
        ] {
            s.execute(sql).await.unwrap();
        }

        assert!(metadata::schemas(&s).await.unwrap().contains(&schema));
        assert!(metadata::search_path(&s).await.unwrap().contains(&"public".to_string()));

        let o = metadata::objects(&s, &schema).await.unwrap();
        assert_eq!(o.tables, vec!["Order Items"]);
        assert_eq!(o.views, vec!["v_items"]);
        assert_eq!(o.matviews, vec!["mv_items"]);
        assert_eq!(o.functions, vec!["add_one(a integer)"]);

        let cols = metadata::columns(&s, &schema, "Order Items").await.unwrap();
        let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "Name", "total", "twice"]);
        assert!(cols[0].is_pk && !cols[0].nullable);
        assert_eq!(cols[0].type_name, "integer");
        assert!(!cols[1].nullable && !cols[1].is_pk);
        assert_eq!(cols[2].default.as_deref(), Some("0"));
        assert!(cols[3].generated);

        let idx = metadata::indexes(&s, &schema, "Order Items").await.unwrap();
        let names: Vec<_> = idx.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Order Items_pkey", "items_name"]);

        s.execute(format!("drop schema {q} cascade")).await.unwrap();
    });
}
