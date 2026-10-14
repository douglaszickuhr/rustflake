mod common;

use pollster::block_on;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::lexer::quote_ident;
use rb_core::metadata;
use rb_core::table::{EditTarget, PendingEdits, RowEdit, build_sql, select_sql};

#[test]
fn pending_edits_round_trip_through_postgres() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let schema = common::unique("RbEdit");
    let q = quote_ident(&schema);
    block_on(async {
        for sql in [
            format!("create schema {q}"),
            format!("create type {q}.mood as enum ('ok', 'sad')"),
            format!("create table {q}.\"Order Items\" (id int primary key, \"Name\" text, tags text[], mood {q}.mood)"),
        ] {
            s.execute(sql).await.unwrap();
        }
        let target = EditTarget {
            schema: schema.clone(),
            table: "Order Items".into(),
            columns: metadata::columns(&s, &schema, "Order Items").await.unwrap(),
        };
        let read = async || {
            let out = s.execute(select_sql(&schema, "Order Items", None, Some(("id", true)))).await.unwrap();
            ResultSet::new(out.columns, out.batch, out.more)
        };

        // inserir
        let mut p = PendingEdits::default();
        p.add_row();
        for (col, v) in [(0, "1"), (1, "Ana"), (2, "{a,\"b c\"}"), (3, "sad")] {
            p.set(0, col, Some(v.into()), 0);
        }
        let n = s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |_, _| None)).unwrap()).await.unwrap();
        assert_eq!(n, 1);
        let rs = read().await;
        assert_eq!(rs.text(0, 1).as_deref(), Some("Ana"));
        assert_eq!(rs.text(0, 2).as_deref(), Some("{a,\"b c\"}"));
        assert_eq!(rs.text(0, 3).as_deref(), Some("sad"));

        // alterar para NULL usando o texto decodificado como PK
        let mut p = PendingEdits::default();
        p.set(0, 1, None, rs.rows());
        s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |r, c| rs.text(r, c))).unwrap()).await.unwrap();
        assert_eq!(read().await.text(0, 1), None);

        // linha inexistente: erro e nada muda
        let ghost = [RowEdit::Update { pk: vec![Some("999".into())], changes: vec![(1, Some("x".into()))] }];
        assert!(matches!(s.run_in_transaction(build_sql(&target, &ghost).unwrap()).await, Err(DbError::Edit(_))));

        // excluir
        let mut p = PendingEdits::default();
        let rs = read().await;
        p.toggle_delete(0, rs.rows());
        s.run_in_transaction(build_sql(&target, &p.to_row_edits(&target, |r, c| rs.text(r, c))).unwrap()).await.unwrap();
        assert_eq!(read().await.rows(), 0);

        s.execute(format!("drop schema {q} cascade")).await.unwrap();
    });
}

#[test]
fn too_long_values_are_rejected_not_truncated() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let schema = common::unique("RbLen");
    let q = quote_ident(&schema);
    block_on(async {
        for sql in [format!("create schema {q}"), format!("create table {q}.t (id int primary key, v varchar(5))"), format!("insert into {q}.t values (1, 'a')")] {
            s.execute(sql).await.unwrap();
        }
        let target = EditTarget { schema: schema.clone(), table: "t".into(), columns: metadata::columns(&s, &schema, "t").await.unwrap() };
        let edit = [RowEdit::Update { pk: vec![Some("1".into())], changes: vec![(1, Some("abcdefghij".into()))] }];
        let res = s.run_in_transaction(build_sql(&target, &edit).unwrap()).await;
        assert!(matches!(res, Err(DbError::Postgres { ref code, .. }) if code == "22001"), "{res:?}");
        s.execute(format!("drop schema {q} cascade")).await.unwrap();
    });
}
