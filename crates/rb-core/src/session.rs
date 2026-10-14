use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio_postgres::types::ToSql;
use tokio_postgres::{CancelToken, Client};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::DbError;
use crate::batch::{Batch, ColumnInfo};
use crate::config::ConnectionConfig;

pub const FETCH_ROWS: i32 = 1000;

/// Runtime tokio dedicado ao I/O de banco. Barato de clonar.
#[derive(Clone)]
pub struct Engine {
    rt: Arc<Runtime>,
}

#[derive(Debug)]
pub struct ExecOutcome {
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

type Reply<T> = oneshot::Sender<Result<T, DbError>>;

enum Cmd {
    Execute { sql: String, reply: Reply<ExecOutcome> },
    FetchMore { reply: Reply<FetchOutcome> },
    QueryText { sql: String, params: Vec<String>, reply: Reply<Vec<Vec<Option<String>>>> },
    Transaction { stmts: Vec<SqlWithParams>, reply: Reply<u64> },
}

impl Cmd {
    fn fail(self, e: DbError) {
        let _ = match self {
            Cmd::Execute { reply, .. } => reply.send(Err(e)).is_ok(),
            Cmd::FetchMore { reply } => reply.send(Err(e)).is_ok(),
            Cmd::QueryText { reply, .. } => reply.send(Err(e)).is_ok(),
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
        Ok(Engine { rt: Arc::new(rt) })
    }

    pub async fn connect(&self, cfg: &ConnectionConfig, password: Option<String>) -> Result<Session, DbError> {
        let pg = cfg.pg_config(password.as_deref());
        let handle = self.rt.handle().clone();
        self.rt
            .spawn(async move { Session::open(pg, handle).await })
            .await
            .map_err(|e| DbError::Connection(e.to_string()))?
    }
}

/// Uma conexão Postgres. Os comandos são processados em ordem por um ator no runtime do Engine.
pub struct Session {
    tx: mpsc::UnboundedSender<Cmd>,
    cancel: Arc<Mutex<CancelToken>>,
    rt: Handle,
}

fn closed() -> DbError {
    DbError::Connection("sessão encerrada".into())
}

impl Session {
    async fn open(pg: tokio_postgres::Config, rt: Handle) -> Result<Session, DbError> {
        let started = Instant::now();
        let client = connect_client(&pg).await?;
        tracing::info!(elapsed_ms = started.elapsed().as_millis() as u64, "conectado");
        let cancel = Arc::new(Mutex::new(client.cancel_token()));
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Actor { client, pg, cancel: cancel.clone() }.run(rx));
        Ok(Session { tx, cancel, rt })
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, DbError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(make(reply)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }

    pub async fn execute(&self, sql: String) -> Result<ExecOutcome, DbError> {
        self.call(|reply| Cmd::Execute { sql, reply }).await
    }

    pub async fn fetch_more(&self) -> Result<FetchOutcome, DbError> {
        self.call(|reply| Cmd::FetchMore { reply }).await
    }

    /// Para consultas internas cujas colunas são todas texto (use `::text` no SQL).
    pub async fn query_text(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, DbError> {
        let (sql, params) = (sql.to_string(), params.iter().map(|p| p.to_string()).collect());
        self.call(|reply| Cmd::QueryText { sql, params, reply }).await
    }

    pub async fn run_in_transaction(&self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        self.call(|reply| Cmd::Transaction { stmts, reply }).await
    }
}

struct Actor {
    client: Client,
    pg: tokio_postgres::Config,
    cancel: Arc<Mutex<CancelToken>>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut next = rx.recv().await;
        while let Some(cmd) = next.take() {
            next = match cmd {
                Cmd::Execute { sql, reply } => self.execute(sql, reply, &mut rx).await,
                Cmd::FetchMore { reply } => {
                    // Nenhum portal aberto: nada mais a buscar.
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                    rx.recv().await
                }
                Cmd::QueryText { sql, params, reply } => {
                    let _ = reply.send(self.query_text(&sql, &params).await);
                    rx.recv().await
                }
                Cmd::Transaction { stmts, reply } => {
                    let _ = reply.send(self.transaction(stmts).await);
                    rx.recv().await
                }
            };
        }
    }

    /// Executa um statement. Se ele retorna linhas, mantém o portal aberto atendendo
    /// FetchMore até esgotar ou chegar outro comando — que é devolvido para o loop principal.
    async fn execute(&mut self, sql: String, reply: Reply<ExecOutcome>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Option<Cmd> {
        let started = Instant::now();
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
        let first = match tx.query_portal(&portal, FETCH_ROWS).await {
            Ok(r) => r,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return rx.recv().await;
            }
        };
        let mut more = first.len() == FETCH_ROWS as usize;
        let elapsed = started.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, rows = first.len(), "primeiro lote");
        let _ = reply.send(Ok(ExecOutcome { columns, batch: Batch::from_rows(&first, ncols), more, affected: None, elapsed }));

        let mut pending: Option<Option<Cmd>> = None;
        while more {
            match rx.recv().await {
                Some(Cmd::FetchMore { reply }) => match tx.query_portal(&portal, FETCH_ROWS).await {
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

    async fn transaction(&mut self, stmts: Vec<SqlWithParams>) -> Result<u64, DbError> {
        let tx = self.client.transaction().await?;
        let mut total = 0;
        for s in &stmts {
            let p: Vec<&(dyn ToSql + Sync)> = s.params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            let n = tx.execute(s.sql.as_str(), &p).await?;
            if s.expect_one && n != 1 {
                return Err(DbError::Edit(format!("esperava alterar 1 linha, alterou {n}: {}", s.sql)));
            }
            total += n;
        }
        tx.commit().await?;
        Ok(total)
    }
}
