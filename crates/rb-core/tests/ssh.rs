//! Túnel SSH contra o container rb-sshd. Suba com `scripts/test-sshd.sh` e exporte RB_TEST_SSH.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};

use rb_core::DbError;
use rb_core::ssh::{SshTarget, Tunnel, trust_host};

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
    ssh_err(run(Tunnel::connect(t, None)).err(), "bastion 127.0.0.1:1 inacessível");
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
    ssh_err(run(Tunnel::connect(target(&e, "id_plain", kh.clone(), None), None)).err(), "mudou");
    assert_eq!(std::fs::read_to_string(&kh).unwrap(), tampered);
}

#[test]
fn key_file_with_passphrase() {
    let Some(e) = env() else { return skip() };
    let kh = trusted_known_hosts(&e);
    let t = || target(&e, "id_pass", kh.clone(), None);
    ssh_err(run(Tunnel::connect(t(), None)).err(), "exige passphrase");
    ssh_err(run(Tunnel::connect(t(), Some("errada".into()))).err(), "passphrase incorreta");
    assert!(run(Tunnel::connect(t(), Some("segredo".into()))).is_ok());
}

#[test]
fn rejected_user_lists_what_was_tried() {
    let Some(e) = env() else { return skip() };
    let mut t = target(&e, "id_plain", trusted_known_hosts(&e), None);
    t.user = "ninguem".into();
    ssh_err(run(Tunnel::connect(t, None)).err(), "nenhuma chave aceita por ninguem@");
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
        ssh_err(t.open_stream("rb-pg", 9).await.err(), "recusou abrir canal para rb-pg:9");
    });
}
