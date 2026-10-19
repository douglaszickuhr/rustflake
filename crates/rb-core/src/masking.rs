//! Escopo das roles de classificação nas masking policies por tag. As regras (nomes das roles,
//! tag) vêm da configuração da conexão; aqui ficam os tipos e a consulta.

use crate::DbError;
use crate::org_config::OrgConfig;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// `masking.global`: todo o account.
    Global,
    /// `masking.group`: os databases do glob da configuração.
    Group,
    /// `<DB>` + `scoped_suffix`.
    Database(String),
    /// `<DB>_<SCHEMA>` + `scoped_suffix`.
    Schema(String, String),
    /// Não casou com nenhum database conhecido (ou não segue o padrão).
    Unresolved(String),
}

/// Colunas com a tag de classificação, agregadas por schema e valor.
#[derive(Debug, Clone, PartialEq)]
pub struct TagCount {
    pub database: String,
    pub schema: String,
    pub value: String,
    pub columns: u64,
}

/// Sem tag de classificação na configuração: nada a contar.
pub async fn tag_counts(s: &Session, org: &OrgConfig) -> Result<Vec<TagCount>, DbError> {
    let Some(sql) = org.tag_counts_sql() else { return Ok(Vec::new()) };
    let (names, rows) = s.query_named(&sql).await?;
    let col = |r: &[Option<String>], c: &str| names.iter().position(|n| n.eq_ignore_ascii_case(c)).and_then(|i| r.get(i).cloned().flatten());
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(TagCount { database: col(r, "database")?, schema: col(r, "schema")?, value: col(r, "value")?, columns: col(r, "columns")?.parse().ok()? })
        })
        .collect())
}
