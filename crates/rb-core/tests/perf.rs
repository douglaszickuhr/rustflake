mod common;

use std::time::Instant;

use pollster::block_on;
use rb_core::batch::ResultSet;

/// Mede a memória com `perf_big` (1M × 10, de scripts/seed-perf.sql) inteiro carregado.
/// Rode: `cargo test --release -p rb-core --test perf -- --ignored --nocapture`.
#[test]
#[ignore]
fn load_perf_big_and_report_rss() {
    let Some((_e, s)) = common::open() else { return common::skip() };
    let rss_kb = || {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap()
    };
    let before = rss_kb();
    let started = Instant::now();
    let rs = block_on(async {
        let out = s.execute("select * from perf_big".into()).await.unwrap();
        let id = out.exec_id;
        let mut rs = ResultSet::new(out.columns, out.batch, out.more);
        while rs.more {
            let f = s.fetch_more(id).await.unwrap();
            rs.push(f.batch, f.more);
        }
        rs
    });
    assert_eq!(rs.rows(), 1_000_000);
    let after = rss_kb();
    println!("1M×10 carregado em {:?}; RSS {} MB (antes {} MB)", started.elapsed(), after / 1024, before / 1024);
    assert!(after / 1024 < 400, "meta: < 400 MB");
}
