//! Snowflake: conversão de valores, paginação, metadados e sessão compartilhada.

use rust_i18n::t;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use snowflake_connector_rs as sf;
use snowflake_connector_rs::{CellValue, VectorValue};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::DbError;
use crate::batch::{Batch, ColumnInfo};
use crate::config::{ConnectionConfig, SnowflakeAuth, expand_home};
use crate::explain::{Plan, parse_sf_explain, parse_sf_stats};
use crate::metadata::{ColumnMeta, SchemaObjects};
use crate::session::{Cmd, Credentials, ExecOutcome, FETCH_ROWS, FetchOutcome, Reply};
use crate::split::{is_read_only_query, is_transaction_control};

type Rows = Vec<Vec<Option<String>>>;

/// Texto exibido na grade. Decimal fica exato; timestamps em ISO 8601.
pub fn cell_text(v: &CellValue) -> Option<String> {
    Some(match v {
        CellValue::Null => return None,
        CellValue::Boolean(b) => b.to_string(),
        CellValue::Integer(i) => i.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Decimal(d) => d.raw().to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Date(d) => d.format("%Y-%m-%d").to_string(),
        CellValue::Time(t) => t.format("%H:%M:%S%.f").to_string(),
        CellValue::TimestampNtz(t) => t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        CellValue::TimestampLtz(t) => t.format("%Y-%m-%d %H:%M:%S%.fZ").to_string(),
        CellValue::TimestampTz(t) => t.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string(),
        CellValue::Json(j) => j.to_string(),
        CellValue::Binary(b) => format!("\\x{}", b.as_bytes().iter().map(|x| format!("{x:02x}")).collect::<String>()),
        CellValue::Vector(VectorValue::Int(x)) => format!("{:?}", x.clone().into_vec()),
        CellValue::Vector(VectorValue::Float(x)) => format!("{:?}", x.clone().into_vec()),
        // CellValue é non_exhaustive: tipo novo aparece como texto de depuração em vez de sumir.
        other => format!("{other:?}"),
    })
}

/// Fonte de blocos de linhas (o cursor do conector; nos testes, uma fila).
pub(crate) trait Chunks {
    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Rows>, DbError>> + Send;
}

/// Reparte blocos de tamanho qualquer em lotes de `n` linhas.
pub(crate) struct Pager<C> {
    src: C,
    buf: VecDeque<Vec<Option<String>>>,
    done: bool,
    ncols: usize,
}

impl<C: Chunks> Pager<C> {
    pub(crate) fn new(src: C, ncols: usize) -> Self {
        Pager { src, buf: VecDeque::new(), done: false, ncols }
    }

    async fn pull(&mut self) -> Result<(), DbError> {
        match self.src.next_chunk().await? {
            Some(rows) => self.buf.extend(rows),
            None => self.done = true,
        }
        Ok(())
    }

    /// Próximo lote e se ainda há linhas depois dele.
    pub(crate) async fn next_batch(&mut self, n: usize) -> Result<(Batch, bool), DbError> {
        while self.buf.len() < n && !self.done {
            self.pull().await?;
        }
        let take = n.min(self.buf.len());
        let mut batch = Batch::empty(self.ncols);
        for row in self.buf.drain(..take) {
            let cells: Vec<Option<&[u8]>> = row.iter().map(|c| c.as_deref().map(str::as_bytes)).collect();
            batch.push_row(&cells);
        }
        // Lote exato no fim de um bloco: confere se há outro para responder "more" certo.
        while self.buf.is_empty() && !self.done {
            self.pull().await?;
        }
        Ok((batch, !self.buf.is_empty()))
    }
}

fn col_index(names: &[String], col: &str) -> Option<usize> {
    names.iter().position(|n| n.eq_ignore_ascii_case(col))
}

pub(crate) fn field<'a>(names: &[String], row: &'a [Option<String>], col: &str) -> Option<&'a str> {
    col_index(names, col).and_then(|i| row.get(i)).and_then(|v| v.as_deref())
}

/// Valores de uma coluna de um `SHOW` (nomes de coluna sem diferenciar caixa).
pub fn first_named(names: &[String], rows: &[Vec<Option<String>>], col: &str) -> Vec<String> {
    rows.iter().filter_map(|r| field(names, r, col).map(String::from)).collect()
}

/// `SHOW OBJECTS IN SCHEMA`.
pub fn parse_objects(names: &[String], rows: &[Vec<Option<String>>]) -> SchemaObjects {
    let mut o = SchemaObjects::default();
    for r in rows {
        let Some(name) = field(names, r, "name") else { continue };
        let kind = field(names, r, "kind").unwrap_or("TABLE");
        let list = if kind.contains("MATERIALIZED") {
            &mut o.matviews
        } else if kind.contains("VIEW") {
            &mut o.views
        } else {
            &mut o.tables
        };
        list.push(name.to_string());
    }
    o
}

/// `SHOW USER FUNCTIONS IN SCHEMA`: `arguments` = `NOME(TIPOS) RETURN TIPO`.
pub fn parse_functions(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<String> {
    first_named(names, rows, "arguments").into_iter().map(|a| a.split(" RETURN ").next().unwrap_or(&a).to_string()).collect()
}

/// `DESCRIBE TABLE` / `DESCRIBE VIEW`.
pub fn parse_describe(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<ColumnMeta> {
    rows.iter()
        .filter_map(|r| {
            let name = field(names, r, "name")?.to_string();
            let type_name = field(names, r, "type").unwrap_or("").to_string();
            Some(ColumnMeta {
                name,
                cast_type: type_name.clone(),
                type_name,
                nullable: field(names, r, "null?") != Some("N"),
                default: field(names, r, "default").map(String::from),
                is_pk: field(names, r, "primary key") == Some("Y"),
                generated: false,
            })
        })
        .collect()
}

/// O conector espera o retorno do SSO por 60 s e depois cai para ler a URL do stdin, o que num app
/// de janela trava (terminal) ou falha (Finder). Damos um pouco mais e desistimos com mensagem clara.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);

/// O padrão do conector (300 s) derruba queries longas sem cancelá-las no Snowflake.
const QUERY_TIMEOUT: Duration = Duration::from_secs(48 * 3600);

async fn create_session(client: &sf::Client) -> Result<sf::Session, DbError> {
    match tokio::time::timeout(LOGIN_TIMEOUT, client.create_session()).await {
        Ok(r) => r.map_err(sf_err),
        Err(_) => Err(DbError::Connection(t!("err.sf_login_timeout").to_string())),
    }
}

pub(crate) fn sf_err(e: sf::Error) -> DbError {
    match e.snowflake_code() {
        Some(code) => DbError::Snowflake { code: code.to_string(), message: e.snowflake_message().unwrap_or_default().to_string() },
        None if e.is_cancelled() => DbError::Snowflake { code: t!("err.cancelled_code").to_string(), message: t!("err.cancelled").to_string() },
        None => DbError::Connection(e.to_string()),
    }
}

impl Chunks for sf::ResultCursor {
    async fn next_chunk(&mut self) -> Result<Option<Rows>, DbError> {
        let Some(table) = self.next_table().await.map_err(sf_err)? else { return Ok(None) };
        let mut rows = Vec::with_capacity(table.row_count());
        for row in table.dynamic_rows().map_err(sf_err)? {
            rows.push(row.map_err(sf_err)?.values().iter().map(cell_text).collect());
        }
        Ok(Some(rows))
    }
}

/// Uma sessão Snowflake por conexão do app, compartilhada por árvore e abas. Se expirar,
/// `renew` faz um novo login (no SSO, o navegador abre de novo).
pub(crate) struct SfShared {
    client: sf::Client,
    session: AsyncMutex<Arc<sf::Session>>,
    uses: Mutex<UseLog>,
}

impl SfShared {
    pub(crate) async fn login(cfg: &ConnectionConfig, creds: &Credentials) -> Result<SfShared, DbError> {
        let s = cfg.snowflake.as_ref().ok_or_else(|| DbError::Config(t!("err.sf_no_block").to_string()))?;
        let auth = match s.auth {
            SnowflakeAuth::ExternalBrowser => sf::AuthConfig::external_browser(sf::ExternalBrowserConfig::default()),
            SnowflakeAuth::Pat => {
                let token = creds.password.clone().ok_or_else(|| DbError::Config(t!("err.sf_no_pat").to_string()))?;
                sf::AuthConfig::password(sf::PasswordConfig::new(token))
            }
            SnowflakeAuth::KeyPair => {
                let path = s.private_key.as_deref().ok_or_else(|| DbError::Config(t!("err.sf_no_key").to_string()))?;
                let pem = std::fs::read_to_string(expand_home(path)).map_err(|e| DbError::Config(format!("{path}: {e}")))?;
                let key = match &creds.key_passphrase {
                    Some(p) => sf::KeyPairConfig::from_encrypted_pem(pem, p.as_bytes().to_vec()),
                    None => sf::KeyPairConfig::from_pem(pem),
                };
                sf::AuthConfig::key_pair(key)
            }
        };
        let set = |v: &Option<String>| v.clone().filter(|v| !v.is_empty());
        let mut session = sf::SessionConfig::new().with_session_parameter("CLIENT_SESSION_KEEP_ALIVE", serde_json::Value::Bool(true));
        if !cfg.database.is_empty() {
            session = session.with_database(&cfg.database);
        }
        if let Some(w) = set(&s.warehouse) {
            session = session.with_warehouse(w);
        }
        if let Some(sc) = set(&s.schema) {
            session = session.with_schema(sc);
        }
        if let Some(r) = set(&s.role) {
            session = session.with_role(r);
        }
        let query = sf::QueryConfig::new().with_query_response_timeout(QUERY_TIMEOUT);
        let client = sf::Client::new(sf::ClientConfig::new(&cfg.user, &s.account, auth).with_session(session).with_query(query)).map_err(sf_err)?;
        let first = create_session(&client).await?;
        Ok(SfShared { client, session: AsyncMutex::new(Arc::new(first)), uses: Mutex::default() })
    }

    pub(crate) async fn current(&self) -> Arc<sf::Session> {
        self.session.lock().await.clone()
    }

    /// Novo login, a não ser que outro ator já tenha renovado a sessão vencida.
    async fn renew(&self, stale: &Arc<sf::Session>) -> Result<Arc<sf::Session>, DbError> {
        let mut cur = self.session.lock().await;
        if Arc::ptr_eq(&cur, stale) {
            tracing::info!("sessão Snowflake expirada; novo login");
            let fresh = create_session(&self.client).await?;
            // O login novo volta ao contexto da configuração: refaz os USE da sessão anterior.
            let replay = self.uses.lock().expect("mutex dos USE").replay();
            for sql in replay {
                let res = match fresh.query_handle(sql.clone()) {
                    Ok(h) => h.execute().await.map(|_| ()),
                    Err(e) => Err(e),
                };
                if let Err(e) = res {
                    tracing::warn!("não refez `{sql}` após novo login: {e}");
                }
            }
            *cur = Arc::new(fresh);
        }
        Ok(cur.clone())
    }
}

/// Últimos `USE …` bem-sucedidos, para refazer o contexto depois de um novo login.
#[derive(Debug, Default)]
pub(crate) struct UseLog(Vec<(String, String)>);

impl UseLog {
    /// Guarda o statement se for `USE`; o mais recente de cada tipo substitui o anterior.
    pub(crate) fn record(&mut self, sql: &str) {
        let mut words = sql.split_whitespace();
        if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("use")) {
            return;
        }
        let kind = match words.next().map(str::to_ascii_lowercase).as_deref() {
            Some(k @ ("warehouse" | "role" | "database" | "schema" | "secondary")) => k.to_string(),
            _ => "plain".into(),
        };
        // Trocar de database desfaz o schema escolhido antes.
        let reset: &[&str] = if kind == "database" { &["schema", "plain"] } else { &[] };
        self.0.retain(|(k, _)| *k != kind && !reset.contains(&k.as_str()));
        self.0.push((kind, sql.to_string()));
    }

    pub(crate) fn replay(&self) -> Vec<String> {
        self.0.iter().map(|(_, sql)| sql.clone()).collect()
    }
}

pub(crate) type SfCanceller = Arc<Mutex<Option<sf::QueryCanceller>>>;

pub(crate) struct SfActor {
    pub(crate) shared: Arc<SfShared>,
    pub(crate) canceller: SfCanceller,
    pub(crate) last_exec: u64,
}

impl SfActor {
    pub(crate) async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut next = rx.recv().await;
        while let Some(cmd) = next.take() {
            next = match cmd {
                Cmd::Execute { sql, reply } => self.execute(sql, reply, &mut rx).await,
                Cmd::FetchMore { reply, .. } => {
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                    rx.recv().await
                }
                Cmd::QueryText { sql, reply, .. } => {
                    let _ = reply.send(self.query_named(&sql).await.map(|(_, rows)| rows));
                    rx.recv().await
                }
                Cmd::QueryNamed { sql, reply } => {
                    let _ = reply.send(self.query_named(&sql).await);
                    rx.recv().await
                }
                Cmd::Transaction { reply, .. } => {
                    let _ = reply.send(Err(DbError::Unsupported(t!("err.sf_read_only").to_string())));
                    rx.recv().await
                }
                Cmd::Explain { sql, analyze, reply } => {
                    let _ = reply.send(self.explain(&sql, analyze).await);
                    rx.recv().await
                }
            };
        }
    }

    /// Envia o statement; se a sessão venceu, renova e tenta uma vez de novo.
    async fn submit(&self, sql: &str) -> Result<sf::ResultCursor, DbError> {
        let mut session = self.shared.current().await;
        for attempt in 0..2 {
            let handle = session.query_handle(sql.to_string()).map_err(sf_err)?;
            *self.canceller.lock().expect("mutex do cancelador") = Some(handle.canceller());
            let res = handle.execute().await;
            *self.canceller.lock().expect("mutex do cancelador") = None;
            match res {
                Err(e) if e.is_session_expired() && attempt == 0 => session = self.shared.renew(&session).await?,
                other => return other.map_err(sf_err),
            }
        }
        unreachable!("o laço sempre retorna na segunda tentativa")
    }

    /// Estimado: `EXPLAIN USING JSON` (só compila). Real: executa e lê GET_QUERY_OPERATOR_STATS —
    /// só leituras, porque a sessão é compartilhada entre as abas e um BEGIN/ROLLBACK atingiria as outras.
    async fn explain(&self, sql: &str, analyze: bool) -> Result<Plan, DbError> {
        if !analyze {
            let (_, rows) = self.query_named(&format!("EXPLAIN USING JSON {sql}")).await?;
            return parse_sf_explain(&rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default());
        }
        if !is_read_only_query(sql) {
            return Err(DbError::Unsupported(t!("err.explain_sf_dml").to_string()));
        }
        let started = Instant::now();
        let cursor = self.submit(sql).await?;
        let id = cursor.query_id().replace('\'', "");
        drop(cursor);
        let total = started.elapsed().as_secs_f64() * 1000.0;
        let (names, rows) = self.query_named(&format!("SELECT * FROM TABLE(GET_QUERY_OPERATOR_STATS('{id}'))")).await?;
        let mut plan = parse_sf_stats(&names, &rows)?;
        plan.total_ms = Some(total);
        Ok(plan)
    }

    async fn query_named(&self, sql: &str) -> Result<(Vec<String>, Rows), DbError> {
        let mut cursor = self.submit(sql).await?;
        let names = cursor.schema().columns().iter().map(|c| c.name().to_string()).collect();
        let mut rows = Vec::new();
        while let Some(chunk) = cursor.next_chunk().await? {
            rows.extend(chunk);
        }
        Ok((names, rows))
    }

    async fn execute(&mut self, sql: String, reply: Reply<ExecOutcome>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Option<Cmd> {
        let started = Instant::now();
        if is_transaction_control(&sql) {
            let msg = t!("err.manual_transactions");
            let _ = reply.send(Err(DbError::Unsupported(msg.into())));
            return rx.recv().await;
        }
        self.last_exec += 1;
        let exec_id = self.last_exec;
        let cursor = match self.submit(&sql).await {
            Ok(c) => c,
            Err(e) => {
                let _ = reply.send(Err(e));
                return rx.recv().await;
            }
        };
        self.shared.uses.lock().expect("mutex dos USE").record(&sql);
        let columns: Arc<[ColumnInfo]> =
            cursor.schema().columns().iter().map(|c| ColumnInfo { name: c.name().to_string(), type_: crate::Type::TEXT }).collect();
        let mut pager = Pager::new(cursor, columns.len());
        let (batch, mut more) = match pager.next_batch(FETCH_ROWS as usize).await {
            Ok(b) => b,
            Err(e) => {
                let _ = reply.send(Err(e));
                return rx.recv().await;
            }
        };
        let elapsed = started.elapsed();
        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, rows = batch.rows(), "primeiro lote (Snowflake)");
        let _ = reply.send(Ok(ExecOutcome { exec_id, columns, batch, more, affected: None, elapsed }));
        while more {
            match rx.recv().await {
                Some(Cmd::FetchMore { exec_id: id, reply }) if id != exec_id => {
                    let _ = reply.send(Ok(FetchOutcome { batch: Batch::empty(0), more: false }));
                }
                Some(Cmd::FetchMore { reply, .. }) => match pager.next_batch(FETCH_ROWS as usize).await {
                    Ok((batch, m)) => {
                        more = m;
                        let _ = reply.send(Ok(FetchOutcome { batch, more }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return rx.recv().await;
                    }
                },
                other => return other,
            }
        }
        rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};

    use super::*;

    #[test]
    fn use_log_keeps_last_of_each_kind_in_replay_order() {
        let mut log = UseLog::default();
        for sql in ["use warehouse A", "USE ROLE r1", "select 1", "use schema S1", "use warehouse B", "  use database D", "use secondary roles all"] {
            log.record(sql);
        }
        // database antes de schema (USE DATABASE zera o schema); o último de cada tipo vale.
        assert_eq!(log.replay(), vec!["USE ROLE r1", "use warehouse B", "  use database D", "use secondary roles all"]);
        log.record("use schema S2");
        assert_eq!(log.replay().last().map(String::as_str), Some("use schema S2"));
        log.record("use PUBLIC");
        assert_eq!(log.replay().last().map(String::as_str), Some("use PUBLIC"));
    }

    #[test]
    fn values_become_grid_text() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
        let ntz = d.and_hms_milli_opt(9, 5, 7, 120).unwrap();
        assert_eq!(cell_text(&CellValue::Null), None);
        assert_eq!(cell_text(&CellValue::Boolean(true)).as_deref(), Some("true"));
        assert_eq!(cell_text(&CellValue::Integer(-170141183460469231731687303715884105728)).as_deref(), Some("-170141183460469231731687303715884105728"));
        assert_eq!(cell_text(&CellValue::Float(1.5)).as_deref(), Some("1.5"));
        assert_eq!(cell_text(&CellValue::String("olá".into())).as_deref(), Some("olá"));
        assert_eq!(cell_text(&CellValue::Date(d)).as_deref(), Some("2026-10-14"));
        assert_eq!(cell_text(&CellValue::Time(ntz.time())).as_deref(), Some("09:05:07.120"));
        assert_eq!(cell_text(&CellValue::TimestampNtz(ntz)).as_deref(), Some("2026-10-14 09:05:07.120"));
        assert_eq!(cell_text(&CellValue::TimestampNtz(d.and_hms_opt(0, 0, 0).unwrap())).as_deref(), Some("2026-10-14 00:00:00"));
        assert_eq!(cell_text(&CellValue::TimestampLtz(Utc.from_utc_datetime(&ntz))).as_deref(), Some("2026-10-14 09:05:07.120Z"));
        let tz = FixedOffset::east_opt(3 * 3600).unwrap().from_local_datetime(&ntz).unwrap();
        assert_eq!(cell_text(&CellValue::TimestampTz(tz)).as_deref(), Some("2026-10-14 09:05:07.120+03:00"));
        assert_eq!(cell_text(&CellValue::Json(serde_json::json!({"a": [1, 2]}))).as_deref(), Some(r#"{"a":[1,2]}"#));
    }

    struct Fake(VecDeque<Vec<Vec<Option<String>>>>);

    impl Chunks for Fake {
        async fn next_chunk(&mut self) -> Result<Option<Vec<Vec<Option<String>>>>, DbError> {
            Ok(self.0.pop_front())
        }
    }

    fn chunk(from: usize, n: usize) -> Vec<Vec<Option<String>>> {
        (from..from + n).map(|i| vec![Some(i.to_string())]).collect()
    }

    #[test]
    fn pager_reslices_irregular_chunks() {
        let mut p = Pager::new(Fake(VecDeque::from([chunk(0, 700), Vec::new(), chunk(700, 2300)])), 1);
        let mut seen = Vec::new();
        let mut flags = Vec::new();
        loop {
            let (b, more) = pollster::block_on(p.next_batch(1000)).unwrap();
            seen.extend((0..b.rows()).map(|r| String::from_utf8(b.cell(r, 0).unwrap().to_vec()).unwrap()));
            flags.push((b.rows(), more));
            if !more {
                break;
            }
        }
        assert_eq!(flags, vec![(1000, true), (1000, true), (1000, false)]);
        assert_eq!(seen, (0..3000).map(|i| i.to_string()).collect::<Vec<_>>());
    }

    #[test]
    fn pager_with_empty_result() {
        let mut p = Pager::new(Fake(VecDeque::new()), 2);
        let (b, more) = pollster::block_on(p.next_batch(1000)).unwrap();
        assert_eq!((b.rows(), more), (0, false));
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn row(v: &[&str]) -> Vec<Option<String>> {
        v.iter().map(|s| (!s.is_empty()).then(|| s.to_string())).collect()
    }

    #[test]
    fn show_objects_split_by_kind() {
        let n = names(&["created_on", "name", "database_name", "schema_name", "kind"]);
        let rows = vec![
            row(&["x", "ORDERS", "DB", "S", "TABLE"]),
            row(&["x", "V_ORDERS", "DB", "S", "VIEW"]),
            row(&["x", "MV", "DB", "S", "MATERIALIZED_VIEW"]),
        ];
        let o = parse_objects(&n, &rows);
        assert_eq!((o.tables, o.views, o.matviews), (vec!["ORDERS".into()], vec!["V_ORDERS".into()], vec!["MV".into()]));
    }

    #[test]
    fn show_functions_keep_signature() {
        let n = names(&["created_on", "name", "arguments"]);
        let rows = vec![row(&["x", "ADD1", "ADD1(NUMBER, VARCHAR) RETURN NUMBER"])];
        assert_eq!(parse_functions(&n, &rows), vec!["ADD1(NUMBER, VARCHAR)".to_string()]);
    }

    #[test]
    fn describe_reads_type_null_default_and_pk() {
        let n = names(&["name", "type", "kind", "null?", "default", "primary key", "unique key"]);
        let rows = vec![row(&["ID", "NUMBER(38,0)", "COLUMN", "N", "", "Y", "N"]), row(&["NOME", "VARCHAR(16777216)", "COLUMN", "Y", "'x'", "N", "N"])];
        let c = parse_describe(&n, &rows);
        assert_eq!((c[0].name.as_str(), c[0].type_name.as_str(), c[0].nullable, c[0].is_pk), ("ID", "NUMBER(38,0)", false, true));
        assert_eq!((c[1].nullable, c[1].default.as_deref(), c[1].is_pk), (true, Some("'x'"), false));
    }

    #[test]
    fn first_named_is_case_insensitive() {
        let n = names(&["created_on", "NAME"]);
        assert_eq!(first_named(&n, &[row(&["x", "A"]), row(&["y", "B"])], "name"), vec!["A".to_string(), "B".to_string()]);
    }
}
