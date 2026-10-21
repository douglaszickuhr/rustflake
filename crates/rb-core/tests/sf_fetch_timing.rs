//! Where the time goes when fetching a big Snowflake result: the query itself, downloading the
//! result chunks, and parsing them (JSON). Decides whether an Arrow result format would pay off.
//!
//! Uses a connection saved in the app (browser SSO is fine: it may open the login page):
//! `RF_BENCH_CONNECTION=Prod cargo test --release -p rb-core --test sf_fetch_timing -- --ignored --nocapture`
//! `RF_BENCH_ROWS` (default 1,000,000) sets the size. The data comes from GENERATOR, so it costs a few
//! seconds of the connection's warehouse and reads no table.

use std::time::Instant;

use pollster::block_on;
use rb_core::config::{connections_path, load};
use rb_core::secrets;
use rb_core::session::{Credentials, Engine};
use snowflake_connector_rs::metrics;

#[test]
#[ignore]
fn where_the_time_goes_fetching_a_big_result() {
    let file = load(&connections_path()).expect("read connections.toml");
    let wanted = std::env::var("RF_BENCH_CONNECTION").ok();
    let cfg = file
        .connections
        .into_iter()
        .filter(|c| c.snowflake.is_some())
        .find(|c| wanted.as_ref().is_none_or(|w| c.name.eq_ignore_ascii_case(w)))
        .expect("no Snowflake connection (set RF_BENCH_CONNECTION to its name)");
    let rows: u64 = std::env::var("RF_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let creds = Credentials { password: secrets::get_password(&cfg.id).ok().flatten(), key_passphrase: secrets::get_password(&secrets::key_account(&cfg.id)).ok().flatten(), ..Default::default() };
    let engine = Engine::new().unwrap();
    let t_login = Instant::now();
    let s = block_on(engine.connect(&cfg, creds)).expect("connect");
    println!("connection {:?}: login in {:?}", cfg.name, t_login.elapsed());

    // Mixed types, like real results: ids, numbers, text, timestamps, decimals and a VARIANT.
    let sql = format!(
        "select seq8() as id, uniform(1, 1000000, random()) as n, randstr(24, random()) as s, \
         dateadd(second, seq4(), '2024-01-01'::timestamp_ntz) as ts, (uniform(1, 100000, random()) / 100)::number(12,2) as amount, \
         object_construct('k', seq4(), 'v', randstr(8, random())) as obj \
         from table(generator(rowcount => {rows}))"
    );
    metrics::reset();
    let t0 = Instant::now();
    let (got, total) = block_on(async {
        let out = s.execute(sql).await.expect("query");
        let first = t0.elapsed();
        let mut got = out.batch.rows() as u64;
        let (id, mut more) = (out.exec_id, out.more);
        println!("first page ({got} rows) after {first:?}  — query on Snowflake + first chunk");
        while more {
            let f = s.fetch_more(id).await.expect("fetch more");
            got += f.batch.rows() as u64;
            more = f.more;
        }
        (got, t0.elapsed())
    });
    let m = metrics::chunk_timings();
    println!("all {got} rows after {total:?}");
    println!("chunks: {}  received {:.1} MB  |  download (sum over chunks) {:?}  |  JSON parse (sum, CPU) {:?}", m.chunks, m.bytes as f64 / 1e6, m.download, m.parse);
    println!("parse throughput: {:.0} rows/s per core;  parse vs download: {:.0}%", got as f64 / m.parse.as_secs_f64().max(1e-9), 100. * m.parse.as_secs_f64() / m.download.as_secs_f64().max(1e-9));
}

/// The same result as JSON and as Arrow: Snowflake's run time, chunk sizes and the wall time to
/// download every chunk in parallel (no decoding). Same connection settings as above:
/// `RF_BENCH_CONNECTION=Prod cargo test --release -p rb-core --test sf_fetch_timing json_vs_arrow -- --ignored --nocapture`
#[test]
#[ignore]
fn json_vs_arrow_sizes_and_download() {
    let file = load(&connections_path()).expect("read connections.toml");
    let wanted = std::env::var("RF_BENCH_CONNECTION").ok();
    let cfg = file
        .connections
        .into_iter()
        .filter(|c| c.snowflake.is_some())
        .find(|c| wanted.as_ref().is_none_or(|w| c.name.eq_ignore_ascii_case(w)))
        .expect("no Snowflake connection (set RF_BENCH_CONNECTION to its name)");
    let rows: u64 = std::env::var("RF_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let creds = Credentials { password: secrets::get_password(&cfg.id).ok().flatten(), key_passphrase: secrets::get_password(&secrets::key_account(&cfg.id)).ok().flatten(), ..Default::default() };
    let engine = Engine::new().unwrap();
    let s = block_on(engine.connect(&cfg, creds)).expect("connect");
    let sql = format!(
        "select seq8() as id, uniform(1, 1000000, random()) as n, randstr(24, random()) as s, \
         dateadd(second, seq4(), '2024-01-01'::timestamp_ntz) as ts, (uniform(1, 100000, random()) / 100)::number(12,2) as amount, \
         object_construct('k', seq4(), 'v', randstr(8, random())) as obj \
         from table(generator(rowcount => {rows}))"
    );
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let http = reqwest::Client::new();
    // Snowflake has several names for this setting depending on the driver; set them all.
    let set_format = |f: &str| {
        for p in ["QUERY_RESULT_FORMAT", "JDBC_QUERY_RESULT_FORMAT", "PYTHON_CONNECTOR_QUERY_RESULT_FORMAT", "GO_QUERY_RESULT_FORMAT", "ODBC_QUERY_RESULT_FORMAT"] {
            let _ = block_on(s.execute(format!("alter session set {p} = '{f}'")));
        }
    };
    println!("{rows} rows, 6 columns\n");
    println!("{:<6} {:>9} {:>7} {:>13} {:>15} {:>12} {:>14}", "format", "query", "chunks", "compressed", "uncompressed", "received", "download wall");
    for f in ["JSON", "ARROW"] {
        set_format(f);
        let t0 = Instant::now();
        let raw = block_on(s.sf_raw_query(sql.clone())).expect("snowflake").expect("raw query");
        let query = t0.elapsed();
        let headers: reqwest::header::HeaderMap = (&raw.download_headers).try_into().unwrap();
        let t1 = Instant::now();
        let received: u64 = rt.block_on(async {
            let mut set = tokio::task::JoinSet::new();
            for c in &raw.chunks {
                let (http, url, headers) = (http.clone(), c.url.clone(), headers.clone());
                set.spawn(async move { http.get(url).headers(headers).send().await.unwrap().error_for_status().unwrap().bytes().await.unwrap().len() as u64 });
            }
            let mut total = 0;
            while let Some(n) = set.join_next().await {
                total += n.unwrap();
            }
            total
        });
        let download = t1.elapsed();
        let mb = |b: i64| format!("{:.1} MB", b as f64 / 1e6);
        println!(
            "{:<6} {:>9.2?} {:>7} {:>13} {:>15} {:>12} {:>14.2?}   (first part inline: {:.1} KB, format reported: {:?})",
            f,
            query,
            raw.chunks.len(),
            mb(raw.chunks.iter().map(|c| c.compressed_size).sum()),
            mb(raw.chunks.iter().map(|c| c.uncompressed_size).sum()),
            mb(received as i64),
            download,
            raw.first_part_bytes as f64 / 1e3,
            raw.format
        );
    }
    set_format("JSON");
}
