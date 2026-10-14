use std::collections::{BTreeMap, BTreeSet};

use crate::DbError;
use crate::lexer::quote_ident;
use crate::metadata::ColumnMeta;
use crate::session::SqlWithParams;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEdit {
    Update { pk: Vec<Option<String>>, changes: Vec<(usize, Option<String>)> },
    Insert { values: Vec<(usize, Option<String>)> },
    Delete { pk: Vec<Option<String>> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingEdits {
    changed: BTreeMap<usize, BTreeMap<usize, Option<String>>>,
    deleted: BTreeSet<usize>,
    inserted: Vec<BTreeMap<usize, Option<String>>>,
}

pub fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(table))
}

/// SELECT da aba de tabela. O filtro é uma expressão WHERE crua digitada pelo usuário;
/// vai entre parênteses e com quebra de linha para que um `--` não engula o ORDER BY.
pub fn select_sql(schema: &str, table: &str, filter: Option<&str>, order: Option<(&str, bool)>) -> String {
    let mut sql = format!("SELECT * FROM {}", qualified(schema, table));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql += &format!(" WHERE ({f}\n)");
    }
    if let Some((col, asc)) = order {
        sql += &format!(" ORDER BY {} {}", quote_ident(col), if asc { "ASC" } else { "DESC" });
    }
    sql
}

impl EditTarget {
    pub fn pk(&self) -> Vec<usize> {
        self.columns.iter().enumerate().filter(|(_, c)| c.is_pk).map(|(i, _)| i).collect()
    }
}

fn bind(params: &mut Vec<Option<String>>, value: &Option<String>, type_name: &str) -> String {
    params.push(value.clone());
    format!("(${}::text)::{type_name}", params.len())
}

fn where_pk(t: &EditTarget, pk_cols: &[usize], pk: &[Option<String>], params: &mut Vec<Option<String>>) -> Result<String, DbError> {
    if pk.len() != pk_cols.len() {
        return Err(DbError::Edit("valores de chave primária incompletos".into()));
    }
    Ok(pk_cols
        .iter()
        .zip(pk)
        .map(|(&i, v)| {
            let c = &t.columns[i];
            format!("{} = {}", quote_ident(&c.name), bind(params, v, &c.type_name))
        })
        .collect::<Vec<_>>()
        .join(" AND "))
}

fn writable(t: &EditTarget, cols: &[(usize, Option<String>)]) -> Result<(), DbError> {
    match cols.iter().find(|(i, _)| t.columns[*i].generated) {
        Some((i, _)) => Err(DbError::Edit(format!("a coluna {} é gerada e não pode ser editada", t.columns[*i].name))),
        None => Ok(()),
    }
}

/// Gera os statements parametrizados. Valores vão como texto e o Postgres converte
/// com `($n::text)::<tipo>`, o que cobre enum, json, array, uuid etc.
pub fn build_sql(t: &EditTarget, edits: &[RowEdit]) -> Result<Vec<SqlWithParams>, DbError> {
    let pk_cols = t.pk();
    if pk_cols.is_empty() {
        return Err(DbError::Edit("a tabela não tem chave primária; edição desativada".into()));
    }
    let table = qualified(&t.schema, &t.table);
    let mut out = Vec::new();
    for e in edits {
        let mut params = Vec::new();
        let sql = match e {
            RowEdit::Update { pk, changes } => {
                writable(t, changes)?;
                if changes.is_empty() {
                    continue;
                }
                let set = changes
                    .iter()
                    .map(|(i, v)| {
                        let c = &t.columns[*i];
                        format!("{} = {}", quote_ident(&c.name), bind(&mut params, v, &c.type_name))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("UPDATE {table} SET {set} WHERE {}", where_pk(t, &pk_cols, pk, &mut params)?)
            }
            RowEdit::Insert { values } => {
                writable(t, values)?;
                if values.is_empty() {
                    format!("INSERT INTO {table} DEFAULT VALUES")
                } else {
                    let cols = values.iter().map(|(i, _)| quote_ident(&t.columns[*i].name)).collect::<Vec<_>>().join(", ");
                    let vals = values
                        .iter()
                        .map(|(i, v)| bind(&mut params, v, &t.columns[*i].type_name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("INSERT INTO {table} ({cols}) VALUES ({vals})")
                }
            }
            RowEdit::Delete { pk } => format!("DELETE FROM {table} WHERE {}", where_pk(t, &pk_cols, pk, &mut params)?),
        };
        out.push(SqlWithParams { sql, params, expect_one: true });
    }
    Ok(out)
}

impl PendingEdits {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    pub fn inserted_rows(&self) -> usize {
        self.inserted.len()
    }

    /// `loaded` = linhas vindas do banco; índices a partir dele são linhas inseridas.
    pub fn set(&mut self, row: usize, col: usize, value: Option<String>, loaded: usize) {
        if row >= loaded {
            if let Some(r) = self.inserted.get_mut(row - loaded) {
                r.insert(col, value);
            }
        } else {
            self.changed.entry(row).or_default().insert(col, value);
        }
    }

    pub fn get(&self, row: usize, col: usize, loaded: usize) -> Option<&Option<String>> {
        if row >= loaded { self.inserted.get(row - loaded)?.get(&col) } else { self.changed.get(&row)?.get(&col) }
    }

    pub fn add_row(&mut self) {
        self.inserted.push(BTreeMap::new());
    }

    /// Marca/desmarca exclusão; numa linha inserida, simplesmente a remove.
    pub fn toggle_delete(&mut self, row: usize, loaded: usize) {
        if row >= loaded {
            if row - loaded < self.inserted.len() {
                self.inserted.remove(row - loaded);
            }
        } else if !self.deleted.remove(&row) {
            self.deleted.insert(row);
        }
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deleted.contains(&row)
    }

    /// Ordem: exclusões, alterações, inserções. A PK vem do valor original (`original(row, col)`).
    pub fn to_row_edits(&self, target: &EditTarget, original: impl Fn(usize, usize) -> Option<String>) -> Vec<RowEdit> {
        let pk = target.pk();
        let pk_of = |row: usize| pk.iter().map(|&c| original(row, c)).collect::<Vec<_>>();
        let mut out: Vec<RowEdit> = self.deleted.iter().map(|&r| RowEdit::Delete { pk: pk_of(r) }).collect();
        out.extend(self.changed.iter().filter(|(r, _)| !self.deleted.contains(r)).map(|(&r, cols)| RowEdit::Update {
            pk: pk_of(r),
            changes: cols.iter().map(|(&c, v)| (c, v.clone())).collect(),
        }));
        out.extend(self.inserted.iter().map(|vals| RowEdit::Insert { values: vals.iter().map(|(&c, v)| (c, v.clone())).collect() }));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool, generated: bool) -> ColumnMeta {
        ColumnMeta { name: name.into(), type_name: ty.into(), nullable: !pk, default: None, is_pk: pk, generated }
    }

    fn target() -> EditTarget {
        EditTarget {
            schema: "Sales".into(),
            table: "Order Items".into(),
            columns: vec![
                col("id", "integer", true, false),
                col("Name", "text", false, false),
                col("twice", "integer", false, true),
                col("mood", "\"Sales\".mood", false, false),
            ],
        }
    }

    fn s(v: &str) -> Option<String> {
        Some(v.into())
    }

    #[test]
    fn select_sql_quotes_identifiers() {
        assert_eq!(select_sql("Sales", "Order Items", None, None), r#"SELECT * FROM "Sales"."Order Items""#);
        assert_eq!(
            select_sql("public", "t", Some(" a > 1 -- x "), Some(("Name", false))),
            "SELECT * FROM public.t WHERE (a > 1 -- x\n) ORDER BY \"Name\" DESC"
        );
        assert_eq!(select_sql("public", "t", Some("   "), Some(("id", true))), "SELECT * FROM public.t ORDER BY id ASC");
    }

    #[test]
    fn update_quotes_mixed_case_identifiers() {
        let sql = build_sql(&target(), &[RowEdit::Update { pk: vec![s("7")], changes: vec![(1, s("Ana")), (3, None)] }]).unwrap();
        assert_eq!(
            sql,
            vec![SqlWithParams {
                sql: r#"UPDATE "Sales"."Order Items" SET "Name" = ($1::text)::text, mood = ($2::text)::"Sales".mood WHERE id = ($3::text)::integer"#.into(),
                params: vec![s("Ana"), None, s("7")],
                expect_one: true,
            }]
        );
    }

    #[test]
    fn insert_and_delete() {
        let sql = build_sql(
            &target(),
            &[RowEdit::Insert { values: vec![(0, s("1")), (1, s("x"))] }, RowEdit::Insert { values: vec![] }, RowEdit::Delete { pk: vec![s("9")] }],
        )
        .unwrap();
        assert_eq!(sql[0].sql, r#"INSERT INTO "Sales"."Order Items" (id, "Name") VALUES (($1::text)::integer, ($2::text)::text)"#);
        assert_eq!(sql[1].sql, r#"INSERT INTO "Sales"."Order Items" DEFAULT VALUES"#);
        assert_eq!(sql[2].sql, r#"DELETE FROM "Sales"."Order Items" WHERE id = ($1::text)::integer"#);
        assert_eq!(sql[2].params, vec![s("9")]);
    }

    #[test]
    fn composite_primary_key() {
        let mut t = target();
        t.columns[1].is_pk = true;
        let sql = build_sql(&t, &[RowEdit::Delete { pk: vec![s("1"), s("a")] }]).unwrap();
        assert_eq!(sql[0].sql, r#"DELETE FROM "Sales"."Order Items" WHERE id = ($1::text)::integer AND "Name" = ($2::text)::text"#);
    }

    #[test]
    fn refuses_tables_without_pk_and_generated_columns() {
        let mut t = target();
        t.columns[0].is_pk = false;
        assert!(matches!(build_sql(&t, &[RowEdit::Delete { pk: vec![] }]), Err(DbError::Edit(_))));
        let err = build_sql(&target(), &[RowEdit::Update { pk: vec![s("1")], changes: vec![(2, s("5"))] }]);
        assert!(matches!(err, Err(DbError::Edit(m)) if m.contains("twice")));
    }

    #[test]
    fn pending_edits_produce_deletes_updates_inserts() {
        let mut p = PendingEdits::default();
        assert!(p.is_empty());
        p.set(0, 1, s("novo"), 2);
        p.set(1, 1, s("vai sumir"), 2);
        p.toggle_delete(1, 2);
        p.add_row();
        p.set(2, 0, s("10"), 2); // linha 2 = primeira inserida
        p.add_row();
        p.toggle_delete(3, 2); // remove a segunda inserida
        assert_eq!(p.inserted_rows(), 1);
        assert_eq!(p.get(0, 1, 2), Some(&s("novo")));
        assert!(p.is_deleted(1));

        let original = |row: usize, _col: usize| s(&format!("pk{row}"));
        assert_eq!(
            p.to_row_edits(&target(), original),
            vec![
                RowEdit::Delete { pk: vec![s("pk1")] },
                RowEdit::Update { pk: vec![s("pk0")], changes: vec![(1, s("novo"))] },
                RowEdit::Insert { values: vec![(0, s("10"))] },
            ]
        );
        p.toggle_delete(1, 2);
        assert!(!p.is_deleted(1));
    }
}
