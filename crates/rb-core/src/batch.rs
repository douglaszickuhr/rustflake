use std::ops::Range;
use std::sync::Arc;

use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, Type};

use crate::DbError;
use crate::decode;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_: Type,
}

const NULL: u32 = u32::MAX;

/// Batch of rows in raw binary format: one contiguous buffer + (offset, len) per cell.
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

/// Accepts any type and keeps the raw bytes, without decoding.
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

    pub fn from_rows(rows: &[Row], ncols: usize) -> Result<Batch, DbError> {
        let mut b = Batch { data: Vec::new(), cells: Vec::with_capacity(rows.len() * ncols), ncols };
        for row in rows {
            for i in 0..ncols {
                // Raw accepts all types; an error here could only be a nonexistent column.
                let v = row.try_get::<_, Option<Raw>>(i).ok().flatten();
                b.push_cell(v.map(|r| r.0))?;
            }
        }
        Ok(b)
    }

    pub fn push_row(&mut self, cells: &[Option<&[u8]>]) -> Result<(), DbError> {
        debug_assert_eq!(cells.len(), self.ncols);
        cells.iter().try_for_each(|c| self.push_cell(*c))
    }

    /// Offsets are `u32` (half the memory of `usize`): a block past 4 GiB is refused, never wrapped.
    fn push_cell(&mut self, v: Option<&[u8]>) -> Result<(), DbError> {
        match v {
            Some(bytes) => {
                let end = self.data.len().checked_add(bytes.len()).and_then(|e| u32::try_from(e).ok()).filter(|e| *e < NULL);
                let Some(_) = end else { return Err(DbError::Unsupported(rust_i18n::t!("err.batch_too_large").to_string())) };
                self.cells.push((self.data.len() as u32, bytes.len() as u32));
                self.data.extend_from_slice(bytes);
            }
            None => self.cells.push((0, NULL)),
        }
        Ok(())
    }

    pub fn rows(&self) -> usize {
        self.cells.len().checked_div(self.ncols).unwrap_or(0)
    }

    /// Raw cell bytes; `None` = NULL.
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

    /// Raw cell bytes; `None` = NULL or out of range (stale selection in the UI).
    pub fn raw(&self, row: usize, col: usize) -> Option<&[u8]> {
        if row >= self.rows() || col >= self.columns.len() {
            return None;
        }
        let b = self.ends.partition_point(|&e| e <= row);
        let start = if b == 0 { 0 } else { self.ends[b - 1] };
        self.batches[b].cell(row - start, col)
    }

    /// Full value as text; `None` = NULL.
    pub fn text(&self, row: usize, col: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::cell_text(&self.columns[col].type_, r))
    }

    /// Value for display (one line, truncated); `None` = NULL.
    pub fn display(&self, row: usize, col: usize, max_chars: usize) -> Option<String> {
        self.raw(row, col).map(|r| decode::display_text(&self.columns[col].type_, r, max_chars))
    }

    /// Fixed-width text table (Text mode, like DBeaver): header, `---+---`, `|` between
    /// columns, NULL empty, numeric-only columns right-aligned, cells cut at `max_chars`.
    /// Only the first `max_rows`: the whole text lives in one editor buffer.
    pub fn to_text_table(&self, max_chars: usize, max_rows: usize) -> String {
        let n = self.rows().min(max_rows);
        let cells: Vec<Vec<String>> = (0..n).map(|r| (0..self.columns.len()).map(|c| self.display(r, c, max_chars).unwrap_or_default()).collect()).collect();
        let len = |s: &str| s.chars().count();
        let cols: Vec<(usize, bool)> = self
            .columns
            .iter()
            .enumerate()
            .map(|(c, info)| {
                let w = cells.iter().map(|row| len(&row[c])).chain([len(&info.name)]).max().unwrap_or(0);
                // Snowflake returns everything as text: decide by content.
                let numeric = cells.iter().any(|row| !row[c].is_empty()) && cells.iter().all(|row| row[c].is_empty() || row[c].parse::<f64>().is_ok());
                (w, numeric)
            })
            .collect();
        let line = |vals: &mut dyn Iterator<Item = &str>, right: bool| {
            let parts: Vec<String> = vals
                .zip(&cols)
                .map(|(v, &(w, num))| if right && num { format!("{v:>w$}") } else { format!("{v:<w$}") })
                .collect();
            parts.join("|").trim_end().to_string()
        };
        let mut out = line(&mut self.columns.iter().map(|c| c.name.as_str()), false);
        out.push('\n');
        out.push_str(&cols.iter().map(|&(w, _)| "-".repeat(w)).collect::<Vec<_>>().join("+"));
        for row in &cells {
            out.push('\n');
            out.push_str(&line(&mut row.iter().map(String::as_str), true));
        }
        out.push('\n');
        out
    }

    /// TSV in the format spreadsheets accept: NULL becomes empty, special fields are quoted.
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
            b.push_row(&[Some(&id), t.map(str::as_bytes)]).unwrap();
        }
        b
    }

    #[test]
    fn text_table_aligns_and_cuts() {
        let rs = ResultSet::new(cols(), batch(&[(1, Some("ab")), (1234, None), (7, Some("a very long value here"))]), false);
        assert_eq!(
            rs.to_text_table(10, 100),
            "id  |t\n----+-----------\n   1|ab\n1234|\n   7|a very lon…\n",
            "numbers right, text left, NULL empty, cut with …"
        );
        let empty = ResultSet::new(cols(), Batch::empty(2), false);
        assert_eq!(empty.to_text_table(10, 100), "id|t\n--+-\n");
    }

    #[test]
    fn text_table_stops_at_max_rows() {
        let rs = ResultSet::new(cols(), batch(&[(1, None), (2, None), (3, None)]), false);
        assert_eq!(rs.to_text_table(10, 2).lines().count(), 4, "header, rule and 2 rows");
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
    fn out_of_range_cells_are_none_not_panic() {
        let rs = ResultSet::new(cols(), batch(&[(1, Some("a"))]), false);
        assert_eq!(rs.raw(0, 2), None);
        assert_eq!(rs.raw(5, 0), None);
        assert_eq!(rs.text(0, 9), None);
        assert_eq!(rs.to_tsv(0..3, &[0, 7]), "1\t\n\t\n\t\n");
    }

    #[test]
    fn display_marks_null_as_none() {
        let rs = ResultSet::new(cols(), batch(&[(1, None)]), false);
        assert_eq!(rs.display(0, 1, 10), None);
        assert_eq!(rs.display(0, 0, 10).as_deref(), Some("1"));
    }
}
