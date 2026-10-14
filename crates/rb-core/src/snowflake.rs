//! Snowflake: conversão de valores, paginação, metadados e sessão compartilhada.

use std::collections::VecDeque;
use std::future::Future;

use snowflake_connector_rs::{CellValue, VectorValue};

use crate::DbError;
use crate::batch::Batch;
use crate::metadata::{ColumnMeta, SchemaObjects};

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

fn field<'a>(names: &[String], row: &'a [Option<String>], col: &str) -> Option<&'a str> {
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{FixedOffset, NaiveDate, TimeZone, Utc};

    use super::*;

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
