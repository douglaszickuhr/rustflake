#![allow(dead_code)]

use rb_core::config::{ConnectionConfig, SslMode};
use rb_core::session::{Engine, Session};

/// Conexão de teste a partir de RB_TEST_DATABASE_URL; `None` se a variável não existir.
pub fn open() -> Option<(Engine, Session)> {
    let url = std::env::var("RB_TEST_DATABASE_URL").ok()?;
    let pg: tokio_postgres::Config = url.parse().expect("RB_TEST_DATABASE_URL inválida");
    let host = match &pg.get_hosts()[0] {
        tokio_postgres::config::Host::Tcp(h) => h.clone(),
        _ => panic!("use host TCP em RB_TEST_DATABASE_URL"),
    };
    let cfg = ConnectionConfig {
        id: "test".into(),
        name: "test".into(),
        host,
        port: pg.get_ports().first().copied().unwrap_or(5432),
        database: pg.get_dbname().unwrap_or("postgres").into(),
        user: pg.get_user().unwrap_or("postgres").into(),
        ssl: SslMode::Disable,
        ssh: None,
    };
    let password = pg.get_password().map(|p| String::from_utf8_lossy(p).into_owned());
    let engine = Engine::new().expect("runtime");
    let creds = rb_core::session::Credentials { password, ..Default::default() };
    let session = pollster::block_on(engine.connect(&cfg, creds)).expect("conectar ao Postgres de teste");
    Some((engine, session))
}

pub fn skip() {
    eprintln!("RB_TEST_DATABASE_URL ausente; teste de integração pulado");
}

pub fn unique(prefix: &str) -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{prefix}_{n}")
}
