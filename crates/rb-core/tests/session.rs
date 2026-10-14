mod common;

use pollster::block_on;
use rb_core::DbError;
use rb_core::batch::ResultSet;
use rb_core::session::SqlWithParams;

#[test]
fn select_pages_in_batches_of_1000() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g, g::text || 'x' from generate_series(1, 2500) g".into()).await.unwrap();
        assert_eq!(out.columns.len(), 2);
        assert_eq!(out.batch.rows(), 1000);
        assert!(out.more);
        assert_eq!(out.affected, None);
        let mut rs = ResultSet::new(out.columns, out.batch, out.more);
        while rs.more {
            let f = s.fetch_more().await.unwrap();
            rs.push(f.batch, f.more);
        }
        assert_eq!(rs.rows(), 2500);
        assert_eq!(rs.text(2499, 0).as_deref(), Some("2500"));
        assert_eq!(rs.text(0, 1).as_deref(), Some("1x"));
    });
}

#[test]
fn exact_multiple_of_page_size_terminates() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g from generate_series(1, 2000) g".into()).await.unwrap();
        assert!(out.more);
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (1000, true));
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (0, false));
        let f = s.fetch_more().await.unwrap(); // sem portal aberto: vazio
        assert_eq!((f.batch.rows(), f.more), (0, false));
    });
}

#[test]
fn million_rows_page_through() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g, md5(g::text) from generate_series(1, 1000000) g".into()).await.unwrap();
        let mut rs = ResultSet::new(out.columns, out.batch, out.more);
        while rs.more {
            let f = s.fetch_more().await.unwrap();
            rs.push(f.batch, f.more);
        }
        assert_eq!(rs.rows(), 1_000_000);
        assert_eq!(rs.text(999_999, 0).as_deref(), Some("1000000"));
    });
}

#[test]
fn ddl_and_dml_report_affected_rows() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        s.execute("create temp table t (a int)".into()).await.unwrap();
        let out = s.execute("insert into t select generate_series(1, 5)".into()).await.unwrap();
        assert_eq!(out.affected, Some(5));
        assert!(out.columns.is_empty());
        let out = s.execute("insert into t values (9) returning a * 2".into()).await.unwrap();
        assert_eq!(out.batch.rows(), 1);
    });
}

#[test]
fn postgres_errors_keep_sqlstate_and_position() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let err = s.execute("select nope from pg_class".into()).await.unwrap_err();
        assert!(matches!(err, DbError::Postgres { ref code, position: Some(8), .. } if code == "42703"), "{err:?}");
        // a sessão continua utilizável
        assert_eq!(s.execute("select 1".into()).await.unwrap().batch.rows(), 1);
    });
}

#[test]
fn query_text_returns_strings_and_nulls() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let rows = s.query_text("select $1::text, null::text", &["a"]).await.unwrap();
        assert_eq!(rows, vec![vec![Some("a".to_string()), None]]);
    });
}

#[test]
fn transaction_rolls_back_when_expect_one_fails() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        s.execute("create temp table tx (a int)".into()).await.unwrap();
        let stmts = vec![
            SqlWithParams { sql: "insert into tx values (($1::text)::int4)".into(), params: vec![Some("1".into())], expect_one: true },
            SqlWithParams { sql: "update tx set a = 2 where a = 999".into(), params: vec![], expect_one: true },
        ];
        assert!(matches!(s.run_in_transaction(stmts).await, Err(DbError::Edit(_))));
        let rows = s.query_text("select count(*)::text from tx", &[]).await.unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    });
}
