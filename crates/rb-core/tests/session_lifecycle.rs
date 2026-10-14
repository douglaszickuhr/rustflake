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
    std::thread::sleep(Duration::from_millis(200)); // o client percebe o socket fechado
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
        // Se a transação do portal continuasse aberta, xact_start seria de ~300 ms atrás.
        let r = s
            .query_text(
                "select (clock_timestamp() - xact_start < interval '200 ms')::text from pg_stat_activity where pid = pg_backend_pid()",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r[0][0].as_deref(), Some("true"));
        let f = s.fetch_more().await.unwrap();
        assert_eq!((f.batch.rows(), f.more), (0, false));
    });
}
