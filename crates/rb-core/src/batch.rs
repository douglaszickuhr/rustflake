use std::ops::Range;
use std::sync::Arc;

use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, Type};

use crate::decode;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_: Type,
}

const NULL: u32 = u32::MAX;

/// Lote de linhas em formato binário cru: um buffer contíguo + (offset, len) por célula.
#[derive(Debug, Default)]
pub struct Batch {
    data: Vec<u8>,
    cells: Vec<(u32, u32)>,
    ncols: usize,
}

#[derive(Debug)]
pub struct ResultSet {
    pub columns: Arc<[ColumnInfo]>,
    pub more: bool,
    batches: Vec<Batch>,
    ends: Vec<usize>,
}

/// Aceita qualquer tipo e guarda os bytes crus, sem decodificar.
struct Raw<'a>(&'a [u8]);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

impl Batch {
    pub fn empty(ncols: usize) -> Batch {
        Batch { data: Vec::new(), cells: Vec::new(), ncols }
    }

    pub fn from_rows(rows: &[Row], ncols: usize) -> Batch {
        let mut b = Batch { data: Vec::new(), cells: Vec::with_capacity(rows.len() * ncols), ncols };
        for row in rows {
            for i in 0..ncols {
                // Raw aceita todos os tipos; erro aqui só seria coluna inexistente.
                let v = row.try_get::<_, Option<Raw>>(i).ok().flatten();
                b.push_cell(v.map(|r| r.0));
            }
        }
        b
    }

    pub fn push_row(&mut self, cells: &[Option<&[u8]>]) {
        debug_assert_eq!(cells.len(), self.ncols);
        for c in cells {
            self.push_cell(*c);
        }
    }

    fn push_cell(&mut self, v: Option<&[u8]>) {
        match v {
            Some(bytes) => {
                self.cells.push((self.data.len() as u32, bytes.len() as u32));
                self.data.extend_from_slice(bytes);
            }
            None => self.cells.push((0, NULL)),
        }
    }

    pub fn rows(&self) -> usize {
        if self.ncols == 0 { 0 } else { self.cells.len() / self.ncols }
    }

    /// Bytes crus da célula; `None` = NULL.
    pub fn cell(&self, row: usize, col: usize) -> Option<&[u8]> {
        let (off, len) = self.cells[row * self.ncols + col];
        (len != NULL).then(|| &self.data[off as usize..(off + len) as usize])
    }
}

impl ResultSet {
    pub fn new(columns: Arc<[ColumnInfo]>, first: Batch, more: bool) -> ResultSet {
        let mut rs = ResultSet { columns, more, batches: Vec::new(), ends: Vec::new() };
        rs.push(first, more);
        rs
    }

    pub fn push(&mut self, batch: Batch, more: bool) {
        self.more = more;
        if batch.rows() == 0 {
            return;
        }
        self.ends.push(self.rows() + batch.rows());
        self.batches.push(batch);
    }

    pub fn rows(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    pub fn raw(&self, row: usize, col: usize) -> Option<&[u8]> {
        let b = self.ends.partition_point(|&e| e <= row);
        let start = if b == 0 { 0 } else { self.ends[b - 1] };
        self.batches[b].cell(row - start, col)
    }

    /// Valor completo como texto; `None` = NULL.
    pub fn text(&self, row: usize, col: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::cell_text(&self.columns[col].type_, r))
    }

    /// Valor para exibição (uma linha, truncado); `None` = NULL.
    pub fn display(&self, row: usize, col: usize, max_chars: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::display_text(&self.columns[col].type_, r, max_chars))
    }

    /// TSV no formato que planilhas aceitam: NULL vira vazio, campos especiais vão entre aspas.
    pub fn to_tsv(&self, rows: Range<usize>, cols: &[usize]) -> String {
        let mut out = String::new();
        for row in rows {
            for (k, &c) in cols.iter().enumerate() {
                if k > 0 {
                    out.push('\t');
                }
                let v = self.text(row, c).unwrap_or_default();
                if v.contains(['\t', '\n', '\r', '"']) {
                    out.push('"');
                    out.push_str(&v.replace('"', "\"\""));
                    out.push('"');
                } else {
                    out.push_str(&v);
                }
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Arc<[ColumnInfo]> {
        Arc::from(vec![
            ColumnInfo { name: "id".into(), type_: Type::INT4 },
            ColumnInfo { name: "t".into(), type_: Type::TEXT },
        ])
    }

    fn batch(rows: &[(i32, Option<&str>)]) -> Batch {
        let mut b = Batch::empty(2);
        for (id, t) in rows {
            let id = id.to_be_bytes();
            b.push_row(&[Some(&id), t.map(str::as_bytes)]);
        }
        b
    }

    #[test]
    fn batch_cells_and_nulls() {
        let b = batch(&[(1, Some("a")), (2, None)]);
        assert_eq!(b.rows(), 2);
        assert_eq!(b.cell(0, 1), Some(&b"a"[..]));
        assert_eq!(b.cell(1, 1), None);
    }

    #[test]
    fn result_set_spans_batches() {
        let mut rs = ResultSet::new(cols(), batch(&[(1, Some("a")), (2, Some("b"))]), true);
        rs.push(batch(&[(3, Some("c"))]), true);
        rs.push(Batch::empty(2), false);
        assert_eq!(rs.rows(), 3);
        assert!(!rs.more);
        assert_eq!(rs.text(2, 0).as_deref(), Some("3"));
        assert_eq!(rs.text(1, 1).as_deref(), Some("b"));
    }

    #[test]
    fn tsv_quotes_special_fields() {
        let rs = ResultSet::new(cols(), batch(&[(1, Some("a\tb")), (2, Some("x\"y")), (3, None)]), false);
        assert_eq!(rs.to_tsv(0..3, &[0, 1]), "1\t\"a\tb\"\n2\t\"x\"\"y\"\n3\t\n");
    }

    #[test]
    fn display_marks_null_as_none() {
        let rs = ResultSet::new(cols(), batch(&[(1, None)]), false);
        assert_eq!(rs.display(0, 1, 10), None);
        assert_eq!(rs.display(0, 0, 10).as_deref(), Some("1"));
    }
}
