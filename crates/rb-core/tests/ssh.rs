//! Túnel SSH contra o container rb-sshd. Suba com `scripts/test-sshd.sh` e exporte RB_TEST_SSH.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

use pollster::block_on;
use rb_core::DbError;
use rb_core::config::{ConnectionConfig, SshConfig, SslMode};
use rb_core::session::{Credentials, Engine};
use rb_core::ssh::{SshEnv, SshTarget, Tunnel, trust_host};

struct Env {
    host: String,
    port: u16,
    dir: PathBuf,
}

fn env() -> Option<Env> {
    let addr = std::env::var("RB_TEST_SSH").ok()?;
    let (host, port) = addr.rsplit_once(':').expect("RB_TEST_SSH = host:porta");
    Some(Env {
        host: host.into(),
        port: port.parse().expect("porta"),
        dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-ssh"),
    })
}

fn skip() {
    eprintln!("RB_TEST_SSH ausente; teste de túnel pulado");
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("rb-ssh-{}-{name}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run<F: Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

fn target(e: &Env, key: &str, known_hosts: PathBuf, agent: Option<PathBuf>) -> SshTarget {
    SshTarget {
        hostname: e.host.clone(),
        port: e.port,
        user: "rb".into(),
        identity_files: vec![e.dir.join(key)],
        identities_explicit: true,
        agent,
        known_hosts,
    }
}

/// known_hosts temporário com a chave do rb-sshd, aprendida pelo fluxo de confiança.
fn trusted_known_hosts(e: &Env) -> PathBuf {
    let kh = scratch("kh").join("known_hosts");
    match run(Tunnel::connect(target(e, "id_plain", kh.clone(), None), None)).err() {
        Some(DbError::HostKeyUnknown { host, port, key, .. }) => trust_host(&kh, &host, port, &key).unwrap(),
        other => panic!("esperava HostKeyUnknown: {other:?}"),
    }
    kh
}

/// Mensagens no idioma padrão dos testes (`en`: ninguém chama `set_locale`).
fn ssh_err(res: Option<DbError>, needle: &str) {
    assert!(matches!(&res, Some(DbError::Ssh(m)) if m.contains(needle)), "esperava SSH com {needle:?}: {res:?}");
}

#[test]
fn unreachable_bastion() {
    let t = SshTarget {
        hostname: "127.0.0.1".into(),
        port: 1,
        user: "x".into(),
        identity_files: vec![],
        identities_explicit: false,
        agent: None,
        known_hosts: scratch("unreach").join("known_hosts"),
    };
    ssh_err(run(Tunnel::connect(t, None)).err(), "bastion 127.0.0.1:1 unreachable");
}

#[test]
fn unknown_host_is_reported_then_trusted() {
    let Some(e) = env() else { return skip() };
    let kh = scratch("unknown").join("known_hosts");
    let err = run(Tunnel::connect(target(&e, "id_plain", kh.clone(), None), None)).err();
    let Some(DbError::HostKeyUnknown { host, port, fingerprint, key }) = err else { panic!("{err:?}") };
    assert_eq!((host.as_str(), port), (e.host.as_str(), e.port));
    assert!(fingerprint.starts_with("SHA256:"), "{fingerprint}");
    trust_host(&kh, &host, port, &key).unwrap();
    assert!(std::fs::read_to_string(&kh).unwrap().trim_start().starts_with(&format!("[{}]:{} ", e.host, e.port)));
    assert!(run(Tunnel::connect(target(&e, "id_plain", kh, None), None)).is_ok());
}

#[test]
fn changed_host_key_is_refused_without_touching_known_hosts() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    let real = std::fs::read_to_string(&kh).unwrap();
    let fake = std::fs::read_to_string(e.dir.join("id_plain.pub")).unwrap();
    let mut parts: Vec<String> = real.trim().splitn(3, ' ').map(String::from).collect();
    assert_eq!(parts[1], "ssh-ed25519", "o teste supõe chave de host ed25519");
    parts[2] = fake.split_whitespace().nth(1).unwrap().to_string();
    let tampered = format!("{}\n", parts.join(" "));
    std::fs::write(&kh, &tampered).unwrap();
    ssh_err(run(Tunnel::connect(target(&e, "id_plain", kh.clone(), None), None)).err(), "changed");
    assert_eq!(std::fs::read_to_string(&kh).unwrap(), tampered);
}

#[test]
fn key_file_with_passphrase() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    let t = || target(&e, "id_pass", kh.clone(), None);
    ssh_err(run(Tunnel::connect(t(), None)).err(), "requires a passphrase");
    ssh_err(run(Tunnel::connect(t(), Some("errada".into()))).err(), "wrong passphrase");
    assert!(run(Tunnel::connect(t(), Some("segredo".into()))).is_ok());
}

#[test]
fn rejected_user_lists_what_was_tried() {
    let Some(e) = env() else { return skip() };
    let mut t = target(&e, "id_plain", trusted_known_hosts(&e), None);
    t.user = "ninguem".into();
    ssh_err(run(Tunnel::connect(t, None)).err(), "no key accepted for ninguem@");
}

#[test]
fn agent_only_authentication() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    // Caminho curto: sockets Unix no macOS têm limite de ~104 bytes.
    let sock = PathBuf::from(format!("/tmp/rb-agent-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let mut agent = std::process::Command::new("ssh-agent").arg("-D").arg("-a").arg(&sock).stdout(Stdio::null()).spawn().unwrap();
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let added = std::process::Command::new("ssh-add")
        .arg(e.dir.join("id_agent"))
        .env("SSH_AUTH_SOCK", &sock)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let mut t = target(&e, "id_agent", kh, Some(sock.clone()));
    t.identity_files.clear();
    t.identities_explicit = false;
    let res = run(Tunnel::connect(t, None)).err();
    agent.kill().ok();
    let _ = std::fs::remove_file(&sock);
    assert!(added.success());
    assert_eq!(res, None);
}

#[test]
fn channels_to_open_and_closed_ports() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    run(async {
        let t = Tunnel::connect(target(&e, "id_plain", kh, None), None).await.unwrap();
        assert!(t.open_stream("rb-pg", 5432).await.is_ok());
        ssh_err(t.open_stream("rb-pg", 9).await.err(), "refused to open a channel to rb-pg:9");
    });
}

/// Engine que só enxerga o home temporário; destino `rb-pg:5432` só resolve dentro do bastion.
fn tunneled(e: &Env) -> (Engine, ConnectionConfig) {
    let home = scratch("home");
    let env = SshEnv {
        config_file: home.join("nenhum"),
        known_hosts: trusted_known_hosts(e),
        agent: None,
        user: "rb".into(),
        home,
    };
    let cfg = ConnectionConfig {
        environment: None,
        id: "ssh-test".into(),
        name: "ssh".into(),
        host: "rb-pg".into(),
        port: 5432,
        database: "postgres".into(),
        user: "postgres".into(),
        ssl: SslMode::Disable,
        ssh: Some(SshConfig {
            host: e.host.clone(),
            user: Some("rb".into()),
            port: Some(e.port),
            identity_file: Some(e.dir.join("id_plain").display().to_string()),
        }),

        snowflake: None,
    };
    (Engine::new().unwrap().with_ssh_env(env), cfg)
}

#[test]
fn queries_go_through_the_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
    let s2 = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert_eq!(block_on(s2.execute("select 2".into())).unwrap().batch.rows(), 1);
}

#[test]
fn reconnects_after_backend_is_killed_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    assert!(block_on(s.execute("select pg_terminate_backend(pg_backend_pid())".into())).is_err());
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(block_on(s.execute("select 1".into())).unwrap().batch.rows(), 1);
}

#[test]
fn cancel_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    let started = std::time::Instant::now();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            s.cancel();
        });
        block_on(s.execute("select pg_sleep(30)".into()))
    });
    assert!(matches!(res, Err(DbError::Postgres { ref code, .. }) if code == "57014"), "{res:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// `cargo test --release -p rb-core --test ssh -- --ignored --nocapture` (precisa do seed de perf no rb-pg).
#[test]
#[ignore]
fn perf_big_first_batch_through_tunnel() {
    let Some(e) = env() else { return skip() };
    let (engine, cfg) = tunneled(&e);
    let s = block_on(engine.connect(&cfg, Credentials::default())).unwrap();
    let out = block_on(s.execute("select * from perf_big".into())).unwrap();
    println!("primeiro lote de perf_big pelo túnel: {} ms", out.elapsed.as_millis());
    assert!(out.elapsed.as_millis() < 200);
}

#[test]
fn slow_channel_does_not_block_others_and_times_out() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    run(async {
        let t = std::sync::Arc::new(Tunnel::connect(target(&e, "id_plain", kh, None), None).await.unwrap());
        // Endereço que não responde: o sshd fica esperando o connect.
        let slow = tokio::spawn({
            let t = t.clone();
            async move { t.open_stream("10.255.255.1", 5432).await.err() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let started = std::time::Instant::now();
        assert!(t.open_stream("rb-pg", 5432).await.is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        ssh_err(slow.await.unwrap(), "did not respond");
    });
}

#[test]
fn host_known_under_another_key_type_is_not_unknown() {
    let Some(e) = env() else { return skip() };
    let out = std::process::Command::new("ssh-keyscan")
        .args(["-t", "ecdsa", "-p", &e.port.to_string(), &e.host])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let line = String::from_utf8(out.stdout).unwrap();
    assert!(line.contains("ecdsa-sha2"), "{line}");
    let kh = scratch("ecdsa").join("known_hosts");
    std::fs::write(&kh, &line).unwrap();
    assert_eq!(run(Tunnel::connect(target(&e, "id_plain", kh, None), None)).err(), None);
}
