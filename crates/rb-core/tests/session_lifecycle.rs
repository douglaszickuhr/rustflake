mod common;

use std::time::{Duration, Instant};

use pollster::block_on;
use rb_core::DbError;

#[test]
fn cancel_interrupts_running_query() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let started = Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            s.cancel();
        });
        block_on(s.execute("select pg_sleep(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Postgres { ref code, .. }) if code == "57014"), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
}

#[test]
fn reconnects_after_backend_is_killed() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    assert!(block_on(s.execute("select pg_terminate_backend(pg_backend_pid())".into())).is_err());
    std::thread::sleep(Duration::from_millis(200)); // the client notices the closed socket
    let out = block_on(s.execute("select 1".into())).unwrap();
    assert_eq!(out.batch.rows(), 1);
}

#[test]
fn abandoning_a_partial_result_closes_its_transaction() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g from generate_series(1, 5000) g".into()).await.unwrap();
        assert!(out.more);
        std::thread::sleep(Duration::from_millis(300));
        // If the portal's transaction were still open, xact_start would be ~300 ms ago.
        let r = s.query_text("select (clock_timestamp() - xact_start < interval '200 ms')::text from pg_stat_activity where pid = pg_backend_pid()", &[]).await.unwrap();
        assert_eq!(r[0][0].as_deref(), Some("true"));
        let f = s.fetch_more(out.exec_id).await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (0, false));
    });
}

#[test]
fn manual_transactions_are_refused() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        for sql in ["BEGIN", "/* x */ start transaction isolation level serializable", "savepoint a"] {
            let res = s.execute(sql.into()).await;
            assert!(matches!(res, Err(DbError::Unsupported(_))), "{sql}: {res:?}");
        }
        // nothing was left open: the session stays in autocommit
        let r = s.query_text("select (pg_current_xact_id_if_assigned() is null)::text", &[]).await.unwrap();
        assert_eq!(r[0][0].as_deref(), Some("true"));
    });
}

#[test]
fn stale_fetch_more_does_not_consume_the_next_result() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let a = s.execute("select g from generate_series(1, 5000) g".into()).await.unwrap();
        // a FetchMore for result A arrives after B's Execute
        let (b, stale) = tokio::join!(s.execute("select g from generate_series(1, 5000) g".into()), s.fetch_more(a.exec_id));
        let b = b.unwrap();
        assert_eq!(stale.unwrap().batch.rows(), 0);
        let next = s.fetch_more(b.exec_id).await.unwrap();
        let rs = rb_core::batch::ResultSet::new(b.columns, next.batch, next.more);
        assert_eq!(rs.text(0, 0).as_deref(), Some("1001"));
    });
}

#[test]
fn large_dml_returning_is_committed_immediately() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let Some((_e2, other)) = common::open() else { return common::skip() };
    let t = common::unique("rb_ret");
    block_on(async {
        s.execute(format!("create table {t} (id int, v int default 0)")).await.unwrap();
        s.execute(format!("insert into {t} select generate_series(1, 3000)")).await.unwrap();
        let out = s.execute(format!("update {t} set v = 1 returning id")).await.unwrap();
        assert!(!out.more);
        assert_eq!(out.batch.rows(), 3000);
        let r = other.query_text(&format!("select count(*)::text from {t} where v = 1"), &[]).await.unwrap();
        assert_eq!(r[0][0].as_deref(), Some("3000"));
        s.execute(format!("drop table {t}")).await.unwrap();
    });
}

/// Takes a minute: `cargo test -p rb-core --test session_lifecycle idle -- --ignored`.
#[test]
#[ignore]
fn idle_cursor_is_closed_and_a_late_fetch_says_so() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    block_on(async {
        let out = s.execute("select g from generate_series(1, 5000) g".into()).await.unwrap();
        assert!(out.more);
        std::thread::sleep(Duration::from_secs(62));
        let late = s.fetch_more(out.exec_id).await;
        assert!(matches!(late, Err(DbError::Unsupported(ref m)) if m.contains("idle")), "{late:?}");
        assert_eq!(s.execute("select 1".into()).await.unwrap().batch.rows(), 1, "the session keeps working");
    });
}
