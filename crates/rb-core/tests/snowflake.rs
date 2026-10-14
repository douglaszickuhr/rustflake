//! Contra uma conta Snowflake real. Variáveis: RB_TEST_SNOWFLAKE_{ACCOUNT,USER,WAREHOUSE,ROLE,DATABASE}
//! e RB_TEST_SNOWFLAKE_PRIVATE_KEY (caminho da .p8) ou RB_TEST_SNOWFLAKE_PAT.

use std::time::{Duration, Instant};

use pollster::block_on;
use rb_core::DbError;
use rb_core::config::{ConnectionConfig, SnowflakeAuth, SnowflakeConfig, SslMode};
use rb_core::session::{Credentials, Engine, Session};

struct Env {
    engine: Engine,
    cfg: ConnectionConfig,
    creds: Credentials,
    database: String,
}

fn env() -> Option<Env> {
    let v = |k: &str| std::env::var(format!("RB_TEST_SNOWFLAKE_{k}")).ok().filter(|s| !s.is_empty());
    let (key, pat) = (v("PRIVATE_KEY"), v("PAT"));
    let auth = match (&key, &pat) {
        (Some(_), _) => SnowflakeAuth::KeyPair,
        (None, Some(_)) => SnowflakeAuth::Pat,
        _ => return None,
    };
    let database = v("DATABASE")?;
    let cfg = ConnectionConfig {
        id: format!("sf-test-{}", std::process::id()),
        name: "sf".into(),
        host: String::new(),
        port: 0,
        database: database.clone(),
        user: v("USER")?,
        ssl: SslMode::Disable,
        ssh: None,
        snowflake: Some(SnowflakeConfig {
            account: v("ACCOUNT")?,
            auth,
            private_key: key,
            warehouse: v("WAREHOUSE"),
            role: v("ROLE"),
            schema: None,
        }),
    };
    let creds = Credentials { password: pat, ..Default::default() };
    Some(Env { engine: Engine::new().unwrap(), cfg, creds, database })
}

fn skip() {
    eprintln!("RB_TEST_SNOWFLAKE_* ausentes; teste Snowflake pulado");
}

fn connect(e: &Env) -> Session {
    block_on(e.engine.connect(&e.cfg, e.creds.clone())).expect("conectar ao Snowflake")
}

/// Cria um schema descartável e devolve `"DB"."RB_TEST_<pid>_<tag>"`.
fn scratch_schema(e: &Env, s: &Session, tag: &str) -> String {
    let name = format!("\"{}\".\"RB_TEST_{}_{}\"", e.database, std::process::id(), tag.to_uppercase());
    block_on(s.execute(format!("create or replace schema {name}"))).unwrap();
    name
}

fn drop_schema(s: &Session, name: &str) {
    let _ = block_on(s.execute(format!("drop schema if exists {name}")));
}

fn texts(s: &Session, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = block_on(s.execute(sql.into())).unwrap();
    (0..out.batch.rows())
        .map(|r| (0..out.columns.len()).map(|c| out.batch.cell(r, c).map(|b| String::from_utf8(b.to_vec()).unwrap())).collect())
        .collect()
}

#[test]
fn select_one() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    assert_eq!(texts(&s, "select 1 as x"), vec![vec![Some("1".to_string())]]);
}

#[test]
fn large_result_pages_in_order() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let out = block_on(s.execute("select seq4() as n from table(generator(rowcount => 5000)) order by n".into())).unwrap();
    let mut got: Vec<String> = (0..out.batch.rows()).map(|r| String::from_utf8(out.batch.cell(r, 0).unwrap().to_vec()).unwrap()).collect();
    assert_eq!(got.len(), 1000);
    let mut more = out.more;
    while more {
        let f = block_on(s.fetch_more(out.exec_id)).unwrap();
        assert!(f.batch.rows() <= 1000);
        got.extend((0..f.batch.rows()).map(|r| String::from_utf8(f.batch.cell(r, 0).unwrap().to_vec()).unwrap()));
        more = f.more;
    }
    assert_eq!(got, (0..5000).map(|i| i.to_string()).collect::<Vec<_>>());
}

#[test]
fn stale_fetch_more_returns_empty() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let a = block_on(s.execute("select seq4() from table(generator(rowcount => 3000))".into())).unwrap();
    let b = block_on(s.execute("select seq4() from table(generator(rowcount => 3000))".into())).unwrap();
    let stale = block_on(s.fetch_more(a.exec_id)).unwrap();
    assert_eq!((stale.batch.rows(), stale.more), (0, false));
    assert_eq!(block_on(s.fetch_more(b.exec_id)).unwrap().batch.rows(), 1000);
}

#[test]
fn cancel_interrupts_wait() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let started = Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            s.cancel();
        });
        block_on(s.execute("call system$wait(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Snowflake { .. })), "{res:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[test]
fn types_round_trip() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let row = texts(
        &s,
        "select 12345678901234567890.1234567890::number(38,10), 1.5::float, 'olá'::varchar, true, \
         '2026-10-14'::date, '09:05:07'::time, '2026-10-14 09:05:07.120'::timestamp_ntz, \
         '2026-10-14 09:05:07.120 +03:00'::timestamp_tz, parse_json('{\"a\":[1,2]}'), \
         to_binary('cafe', 'HEX'), null",
    )
    .remove(0);
    let expect = [
        Some("12345678901234567890.1234567890"),
        Some("1.5"),
        Some("olá"),
        Some("true"),
        Some("2026-10-14"),
        Some("09:05:07"),
        Some("2026-10-14 09:05:07.120"),
        Some("2026-10-14 09:05:07.120+03:00"),
        Some(r#"{"a":[1,2]}"#),
        Some("\\xcafe"),
        None,
    ];
    assert_eq!(row, expect.iter().map(|v| v.map(String::from)).collect::<Vec<_>>());
}

#[test]
fn syntax_error_has_snowflake_code() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let res = block_on(s.execute("selec 1".into()));
    assert!(matches!(&res, Err(DbError::Snowflake { code, .. }) if !code.is_empty()), "{res:?}");
}

#[test]
fn manual_transactions_and_grid_edits_are_refused() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    assert!(matches!(block_on(s.execute("begin".into())), Err(DbError::Unsupported(_))));
    assert!(matches!(block_on(s.run_in_transaction(vec![])), Err(DbError::Unsupported(_))));
}

#[test]
fn sessions_of_one_connection_share_one_login() {
    let Some(e) = env() else { return skip() };
    let before = rb_core::snowflake::login_count();
    let a = connect(&e);
    let b = connect(&e);
    assert_eq!(rb_core::snowflake::login_count() - before, 1);
    let schema = scratch_schema(&e, &a, "share");
    assert_eq!(texts(&b, "select current_schema()")[0][0].as_deref(), Some(schema.rsplit('.').next().unwrap().trim_matches('"')));
    drop_schema(&a, &schema);
}

#[test]
fn metadata_of_test_schema() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let schema = scratch_schema(&e, &s, "meta");
    block_on(s.execute(format!("create table {schema}.T (ID number primary key, \"nome\" varchar)"))).unwrap();
    block_on(s.execute(format!("create view {schema}.V as select ID from {schema}.T"))).unwrap();
    let key = format!("{}.RB_TEST_{}_META", e.database, std::process::id());

    let dbs = block_on(rb_core::metadata::databases(&s)).unwrap();
    assert!(dbs.contains(&e.database), "{dbs:?}");
    assert!(block_on(rb_core::metadata::db_schemas(&s, &e.database)).unwrap().contains(&key));
    let o = block_on(rb_core::metadata::objects(&s, &key)).unwrap();
    assert_eq!((o.tables, o.views), (vec!["T".to_string()], vec!["V".to_string()]));
    let cols = block_on(rb_core::metadata::columns(&s, &key, "T")).unwrap();
    assert_eq!(cols.iter().map(|c| (c.name.as_str(), c.is_pk)).collect::<Vec<_>>(), vec![("ID", true), ("nome", false)]);
    assert_eq!(block_on(rb_core::metadata::columns(&s, &key, "V")).unwrap().len(), 1);
    drop_schema(&s, &schema);
}

#[test]
fn use_role_and_warehouse_change_context() {
    let Some(e) = env() else { return skip() };
    let s = connect(&e);
    let wh = e.cfg.snowflake.as_ref().unwrap().warehouse.clone().unwrap();
    block_on(s.execute(format!("use warehouse \"{wh}\""))).unwrap();
    block_on(s.execute(format!("use database \"{}\"", e.database))).unwrap();
    block_on(s.execute("use schema INFORMATION_SCHEMA".into())).unwrap();
    let ctx = block_on(rb_core::metadata::context(&s)).unwrap();
    assert_eq!(ctx.warehouse.as_deref(), Some(wh.as_str()));
    assert_eq!(ctx.search_path, vec![format!("{}.INFORMATION_SCHEMA", e.database)]);
    assert!(ctx.role.is_some());
}
