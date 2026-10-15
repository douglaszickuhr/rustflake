use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use russh::ChannelStream;
use russh::client::Msg;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

use rust_i18n::t;

use crate::DbError;
use crate::batch::{Batch, ColumnInfo};
use crate::config::{ConnectionConfig, SnowflakeConfig, SshConfig};
use crate::dialect::Dialect;
use crate::snowflake::{SfActor, SfCanceller, SfShared};
use crate::split::{is_read_only_query, is_transaction_control};
use crate::ssh::{SshEnv, Tunnel};

pub const FETCH_ROWS: i32 = 1000;

/// Runtime tokio dedicado ao I/O de banco. Barato de clonar.
#[derive(Clone)]
pub struct Engine {
    rt: Arc<Runtime>,
    ssh_env: Arc<SshEnv>,
    tunnels: Arc<TunnelCache>,
    sf_sessions: Arc<SnowflakeCache>,
}

#[derive(Debug)]
pub struct ExecOutcome {
    /// Identifica este resultado; `fetch_more` só busca linhas do resultado com este id.
    pub exec_id: u64,
    pub columns: Arc<[ColumnInfo]>,
    pub batch: Batch,
    pub more: bool,
    /// Linhas afetadas, para statements que não retornam colunas.
    pub affected: Option<u64>,
    /// Até o primeiro lote.
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub batch: Batch,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlWithParams {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// Falha (e desfaz tudo) se o statement não afetar exatamente 1 linha.
    pub expect_one: bool,
}

pub(crate) type Reply<T> = oneshot::Sender<Result<T, DbError>>;

/// Segredos de uma conexão, lidos do Keychain pelo app.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub password: Option<String>,
    pub ssh_passphrase: Option<String>,
    /// Passphrase da chave privada Snowflake (key-pair).
    pub key_passphrase: Option<String>,
}

/// Um túnel por conexão do app (id + bloco SSH), compartilhado pelas sessões dela.
type TunnelCache = Mutex<HashMap<(String, SshConfig), Weak<Tunnel>>>;

/// Uma sessão Snowflake por conexão do app (id, usuário, database e bloco Snowflake).
type SnowflakeCache = Mutex<HashMap<(String, String, String, SnowflakeConfig), Weak<SfShared>>>;

pub(crate) enum Cmd {
    Execute { sql: String, reply: Reply<ExecOutcome> },
    FetchMore { exec_id: u64, reply: Reply<FetchOutcome> },
    QueryText { sql: String, params: Vec<String>, reply: Reply<Vec<Vec<Option<String>>>> },
    QueryNamed { sql: String, reply: Reply<(Vec<String>, Vec<Vec<Option<String>>>)> },
    Transaction { stmts: Vec<SqlWithParams>, reply: Reply<u64> },
}

impl Cmd {
    fn fail(self, e: DbError) {
        let _ = match self {
            Cmd::Execute { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::FetchMore { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::QueryText { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::QueryNamed { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::Transaction { reply, .. } => reply.send(Err(e)).is_ok(),
        };
    }
}

/// Configuração TLS com as raízes do sistema, montada uma vez e fora da thread de UI.
fn tls() -> MakeRustlsConnect {
    static TLS: OnceLock<MakeRustlsConnect> = OnceLock::new();
    TLS.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        MakeRustlsConnect::new(cfg)
    })
    .clone()
}

async fn connect_client(pg: &tokio_postgres::Config) -> Result<Client, DbError> {
    let (client, conn) = pg.connect(tls()).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!("conexão encerrada: {e}");
        }
    });
    Ok(client)
}

impl Engine {
    pub fn new() -> Result<Engine, DbError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rb-db")
            .enable_all()
            .build()
            .map_err(|e| DbError::Io(e.to_string()))?;
        Ok(Engine { rt: Arc::new(rt), ssh_env: Arc::new(SshEnv::system()), tunnels: Arc::default(), sf_sessions: Arc::default() })
    }

    /// Troca de onde vem a configuração SSH (testes).
    pub fn with_ssh_env(mut self, env: SshEnv) -> Engine {
        self.ssh_env = Arc::new(env);
        self
    }

    pub async fn connect(&self, cfg: &ConnectionConfig, creds: Credentials) -> Result<Session, DbError> {
        if cfg.snowflake.is_some() {
            let (cfg, cache) = (cfg.clone(), self.sf_sessions.clone());
            let handle = self.rt.handle().clone();
            return self
                .rt
                .spawn(async move {
                    let shared = shared_snowflake(&cache, &cfg, &creds).await?;
                    Ok(Session::open_snowflake(shared, handle))
                })
                .await
                .map_err(|e| DbError::Connection(e.to_string()))?;
        }
        let pg = cfg.pg_config(creds.password.as_deref());
        let route = cfg.ssh.clone().map(|ssh| (cfg.id.clone(), ssh, creds.ssh_passphrase, cfg.host.clone(), cfg.port));
        let (env, tunnels) = (self.ssh_env.clone(), self.tunnels.clone());
        let handle = self.rt.handle().clone();
        self.rt
            .spawn(async move {
                let tunnel = match route {
                    None => None,
                    Some((id, ssh, passphrase, host, port)) => {
                        Some((shared_tunnel(&tunnels, id, ssh, passphrase, &env).await?, host, port))
                    }
                };
                Session::open(Connector { pg, tunnel }, handle).await
            })
            .await
            .map_err(|e| DbError::Connection(e.to_string()))?
    }
}

/// ponytail: duas sessões abrindo ao mesmo tempo podem fazer dois logins; o segundo fica no cache.
async fn shared_snowflake(cache: &SnowflakeCache, cfg: &ConnectionConfig, creds: &Credentials) -> Result<Arc<SfShared>, DbError> {
    let s = cfg.snowflake.clone().expect("chamado só com bloco snowflake");
    let key = (cfg.id.clone(), cfg.user.clone(), cfg.database.clone(), s);
    if let Some(t) = cache.lock().expect("cache Snowflake").get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let shared = Arc::new(SfShared::login(cfg, creds).await?);
    cache.lock().expect("cache Snowflake").insert(key, Arc::downgrade(&shared));
    Ok(shared)
}

/// O túnel cai quando a última sessão que o usa cai (o cache guarda só `Weak`).
/// ponytail: duas sessões abrindo ao mesmo tempo podem criar dois túneis; ambos funcionam
/// e o segundo fica no cache.
async fn shared_tunnel(
    cache: &TunnelCache,
    id: String,
    ssh: SshConfig,
    passphrase: Option<String>,
    env: &SshEnv,
) -> Result<Arc<Tunnel>, DbError> {
    let key = (id, ssh);
    if let Some(t) = cache.lock().expect("cache de túneis").get(&key).and_then(Weak::upgrade) {
        return Ok(t);
    }
    let target = key.1.resolve(env)?;
    let tunnel = Arc::new(Tunnel::connect(target, passphrase).await?);
    cache.lock().expect("cache de túneis").insert(key, Arc::downgrade(&tunnel));
    Ok(tunnel)
}

/// Como abrir um cliente Postgres: direto, ou por um canal do túnel SSH (sem porta local).
#[derive(Clone)]
struct Connector {
    pg: tokio_postgres::Config,
    /// Túnel e destino (`host`, `port`) vistos pelo bastion.
    tunnel: Option<(Arc<Tunnel>, String, u16)>,
}

type ChannelTls = <MakeRustlsConnect as MakeTlsConnect<ChannelStream<Msg>>>::TlsConnect;

fn channel_tls(host: &str) -> Result<ChannelTls, DbError> {
    MakeTlsConnect::<ChannelStream<Msg>>::make_tls_connect(&mut tls(), host).map_err(|e| DbError::Connection(e.to_string()))
}

impl Connector {
    async fn client(&self) -> Result<Client, DbError> {
        let Some((tunnel, host, port)) = &self.tunnel else { return connect_client(&self.pg).await };
        let stream = tunnel.open_stream(host, *port).await?;
        let (client, conn) = self.pg.connect_raw(stream, channel_tls(host)?).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("conexão encerrada: {e}");
            }
        });
        Ok(client)
    }

    async fn cancel(&self, token: CancelToken) -> Result<(), DbError> {
        match &self.tunnel {
            None => token.cancel_query(tls()).await?,
            Some((tunnel, host, port)) => token.cancel_query_raw(tunnel.open_stream(host, *port).await?, channel_tls(host)?).await?,
        }
        Ok(())
    }
}

/// Uma conexão Postgres. Os comandos são processados em ordem por um ator no runtime do Engine.
enum Canceller {
    Postgres { token: Arc<Mutex<CancelToken>>, connector: Connector },
    Snowflake(SfCanceller),
}

pub struct Session {
    tx: mpsc::UnboundedSender<Cmd>,
    rt: Handle,
    canceller: Canceller,
    dialect: Dialect,
    /// Mantém a sessão Snowflake viva enquanto esta `Session` existir.
    _snowflake: Option<Arc<SfShared>>,
}

fn closed() -> DbError {
    DbError::Connection(t!("err.session_closed").to_string())
}

impl Session {
    async fn open(connector: Connector, rt: Handle) -> Result<Session, DbError> {
        let started = Instant::now();
        let client = connector.client().await?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "conectado");
        let cancel = Arc::new(Mutex::new(client.cancel_token()));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Actor { client, connector: connector.clone(), cancel: cancel.clone(), last_exec: 0 }.run(rx));
        Ok(Session { tx, rt, canceller: Canceller::Postgres { token: cancel, connector }, dialect: Dialect::Postgres, _snowflake: None })
    }

    fn open_snowflake(shared: Arc<SfShared>, rt: Handle) -> Session {
        let canceller: SfCanceller = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        rt.spawn(SfActor { shared: shared.clone(), canceller: canceller.clone(), last_exec: 0 }.run(rx));
        Session { tx, rt, canceller: Canceller::Snowflake(canceller), dialect: Dialect::Snowflake, _snowflake: Some(shared) }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// As duas sessões usam a mesma sessão Snowflake (um único login)?
    #[doc(hidden)]
    pub fn shares_snowflake_with(&self, other: &Session) -> bool {
        matches!((&self._snowflake, &other._snowflake), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
    }

    /// Linhas e nomes de coluna (usado nos `SHOW`/`DESCRIBE` do Snowflake).
    pub async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), DbError> {
        let sql = sql.to_string();
        self.call(|reply| Cmd::QueryNamed { sql, reply }).await
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(make(reply)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }

    pub async fn execute(&self, sql: String) -> Result<ExecOutcome, DbError> {
        self.call(|reply| Cmd::Execute { sql, reply }).await
    }

    pub async fn fetch_more(&self, exec_id: u64) -> Result<FetchOutcome, DbError> {
        self.call(|reply| Cmd::FetchMore { exec_id, reply }).await
    }

    /// Para consultas internas cujas colunas são todas texto (use `::text` no SQL).
    pub async fn query_text(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let (sql, params) = (sql.to_string(), params.iter().map(|p| p.to_string()).collect());
        self.call(|reply| Cmd::QueryText { sql, params, reply }).await
    }

    pub async fn run_in_transaction(&self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        self.call(|reply| Cmd::Transaction { stmts, reply }).await
    }

    /// Pede ao servidor para cancelar o comando em andamento desta sessão.
    pub fn cancel(&self) {
        match &self.canceller {
            Canceller::Postgres { token, connector } => {
                let (token, connector) = (token.lock().expect("mutex do cancel token").clone(), connector.clone());
                self.rt.spawn(async move {
                    if let Err(e) = connector.cancel(token).await {
                        tracing::warn!("cancelamento falhou: {e}");
                    }
                });
            }
            Canceller::Snowflake(c) => {
                if let Some(c) = c.lock().expect("mutex do cancelador").clone() {
                    self.rt.spawn(async move {
                        if let Err(e) = c.cancel().await {
                            tracing::warn!("cancelamento Snowflake falhou: {e}");
                        }
                    });
                }
            }
        }
    }
}

struct Actor {
    client: Client,
    connector: Connector,
    cancel: Arc<Mutex<CancelToken>>,
    last_exec: u64,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut next = rx.recv().await;
        while let Some(cmd) = next.take() {
            if let Err(e) = self.ensure_connected().await {
                cmd.fail(e);
                next = rx.recv().await;
                continue;
            }
            next = match cmd {
                Cmd::Execute { sql, reply } => self.execute(sql, reply, &mut rx).await,
                Cmd::FetchMore { reply, .. } => {
                    // Nenhum portal aberto: nada mais a buscar.
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                    rx.recv().await
                }
                Cmd::QueryText { sql, params, reply } => {
                    let _ = reply.send(self.query_text(&sql, &params).await);
                    rx.recv().await
                }
                Cmd::QueryNamed { sql, reply } => {
                    let _ = reply.send(self.query_named(&sql).await);
                    rx.recv().await
                }
                Cmd::Transaction { stmts, reply } => {
                    let _ = reply.send(self.transaction(stmts).await);
                    rx.recv().await
                }
            };
        }
    }

    /// Se a conexão caiu, abre outra. O comando que falhou antes não é reenviado.
    async fn ensure_connected(&mut self) -> Result<(), DbError> {
        if !self.client.is_closed() {
            return Ok(());
        }
        tracing::info!("reconectando");
        self.client = self.connector.client().await?;
        *self.cancel.lock().expect("mutex do cancel token") = self.client.cancel_token();
        Ok(())
    }

    /// Executa um statement. Se ele retorna linhas, mantém o portal aberto atendendo
    /// FetchMore até esgotar ou chegar outro comando — que é devolvido para o loop principal.
    async fn execute(&mut self, sql: String, reply: Reply<ExecOutcome>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Option<Cmd> {
        let started = Instant::now();
        if is_transaction_control(&sql) {
            let msg = t!("err.manual_transactions");
            let _ = reply.send(Err(DbError::Unsupported(msg.into())));
            return rx.recv().await;
        }
        self.last_exec += 1;
        let exec_id = self.last_exec;
        let stmt = match self.client.prepare(&sql).await {
            Ok(s) => s,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let columns: Arc<[ColumnInfo]> = stmt
            .columns()
            .iter()
            .map(|c| ColumnInfo { name: c.name().to_string(), type_: c.type_().clone() })
            .collect();

        if columns.is_empty() {
            let res = self.client.execute(&stmt, &[]).await.map_err(DbError::from).map(|n| ExecOutcome {
                exec_id,
                columns,
                batch: Batch::empty(0),
                more: false,
                affected: Some(n),
                elapsed: started.elapsed(),
            });
            let _ = reply.send(res);
            return rx.recv().await;
        }

        let ncols = columns.len();
        let tx = match self.client.transaction().await {
            Ok(t) => t,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        // Em caso de erro, `tx` é descartada e o tokio-postgres faz ROLLBACK.
        let portal = match tx.bind(&stmt, &[]).await {
            Ok(p) => p,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let mut first = match tx.query_portal(&portal, FETCH_ROWS).await {
            Ok(r) => r,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let mut more = first.len() == FETCH_ROWS as usize;
        if !is_read_only_query(&sql) {
            // Escrita com RETURNING: busca tudo e confirma antes de responder, para não segurar
            // locks nem deixar uma alteração "feita" sem commit.
            while more {
                match tx.query_portal(&portal, FETCH_ROWS).await {
                    Ok(rows) => {
                        more = rows.len() == FETCH_ROWS as usize;
                        first.extend(rows);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.into()));
                        return rx.recv().await;
                    }
                }
            }
            let batch = Batch::from_rows(&first, ncols);
            let res = tx.commit().await.map_err(DbError::from).map(|_| ExecOutcome {
                exec_id,
                columns,
                batch,
                more: false,
                affected: None,
                elapsed: started.elapsed(),
            });
            let _ = reply.send(res);
            return rx.recv().await;
        }
        let elapsed = started.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, rows = first.len(), "primeiro lote");
        let _ = reply.send(Ok(ExecOutcome { exec_id, columns, batch: Batch::from_rows(&first, ncols), more, affected: None, elapsed }));

        let mut pending: Option<Option<Cmd>> = None;
        while more {
            match rx.recv().await {
                Some(Cmd::FetchMore { exec_id: id, reply }) if id != exec_id => {
                    // Pedido atrasado de um resultado anterior: não pode consumir este portal.
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                }
                Some(Cmd::FetchMore { reply, .. }) => match tx.query_portal(&portal, FETCH_ROWS).await {
                    Ok(rows) => {
                        more = rows.len() == FETCH_ROWS as usize;
                        let _ = reply.send(Ok(FetchOutcome { batch: Batch::from_rows(&rows, ncols), more }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.into()));
                        return rx.recv().await;
                    }
                },
                other => {
                    pending = Some(other);
                    break;
                }
            }
        }
        // Autocommit: o que o statement fez (ex.: nextval, função com escrita) fica valendo.
        if let Err(e) = tx.commit().await {
            tracing::warn!("commit após leitura falhou: {e}");
        }
        match pending {
            Some(next) => next,
            None => rx.recv().await,
        }
    }

    async fn query_text(&self, sql: &str, params: &[String]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let p: Vec<&(dyn ToSql + Sync)> = params.iter().map(|s| s as &(dyn ToSql + Sync)).collect();
        let rows = self.client.query(sql, &p).await?;
        rows.iter()
            .map(|r| (0..r.len()).map(|i| r.try_get::<_, Option<String>>(i)).collect::<Result<Vec<_>, _>>())
            .collect::<Result<_, _>>()
            .map_err(Into::into)
    }

    async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), DbError> {
        let stmt = self.client.prepare(sql).await?;
        let names = stmt.columns().iter().map(|c| c.name().to_string()).collect();
        Ok((names, self.query_text(sql, &[]).await?))
    }

    async fn transaction(&mut self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        let tx = self.client.transaction().await?;
        let mut total = 0;
        for s in &stmts {
            let p: Vec<&(dyn ToSql + Sync)> = s.params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            let n = tx.execute(s.sql.as_str(), &p).await?;
            if s.expect_one && n != 1 {
                return Err(DbError::Edit(t!("err.expected_one_row", n = n, sql = s.sql).to_string()));
            }
            total += n;
        }
        tx.commit().await?;
        Ok(total)
    }
}
