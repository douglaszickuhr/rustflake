mod common;

use pollster::block_on;
use rb_core::session::{Credentials, Engine};

#[test]
fn test_reports_the_server_and_checks_the_password_it_is_given() {
    let Some((cfg, password)) = common::target() else { return common::skip() };
    let engine = Engine::new().expect("runtime");
    let creds = |p: Option<String>| Credentials { password: p, ..Default::default() };
    let ok = block_on(engine.test(&cfg, creds(password.clone()))).expect("the test connection works");
    assert!(ok.summary.starts_with("PostgreSQL "), "{}", ok.summary);
    assert!(ok.summary.contains(&cfg.user), "{}", ok.summary);
    // An open session on the same connection must not let a wrong password pass.
    let _open = block_on(engine.connect(&cfg, creds(password))).expect("connect");
    assert!(block_on(engine.test(&cfg, creds(Some("wrong".into())))).is_err());
}
