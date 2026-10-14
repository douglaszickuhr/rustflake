use std::fmt;

use tokio_postgres::error::ErrorPosition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    Postgres {
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        /// Posição 1-based em caracteres dentro do statement enviado.
        position: Option<u32>,
    },
    Connection(String),
    Config(String),
    Keychain(String),
    Io(String),
    Edit(String),
    Unsupported(String),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Postgres { code, message, detail, hint, .. } => {
                write!(f, "[{code}] {message}")?;
                if let Some(d) = detail {
                    write!(f, "\nDETAIL: {d}")?;
                }
                if let Some(h) = hint {
                    write!(f, "\nHINT: {h}")?;
                }
                Ok(())
            }
            DbError::Connection(m) => write!(f, "conexão: {m}"),
            DbError::Config(m) => write!(f, "configuração: {m}"),
            DbError::Keychain(m) => write!(f, "Keychain: {m}"),
            DbError::Io(m) => write!(f, "E/S: {m}"),
            DbError::Edit(m) => write!(f, "edição: {m}"),
            DbError::Unsupported(m) => write!(f, "não suportado: {m}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<tokio_postgres::Error> for DbError {
    fn from(e: tokio_postgres::Error) -> Self {
        match e.as_db_error() {
            Some(db) => DbError::Postgres {
                code: db.code().code().to_string(),
                message: db.message().to_string(),
                detail: db.detail().map(Into::into),
                hint: db.hint().map(Into::into),
                position: match db.position() {
                    Some(ErrorPosition::Original(p)) => Some(*p),
                    _ => None,
                },
            },
            None => DbError::Connection(e.to_string()),
        }
    }
}

/// Converte a posição do Postgres (1-based, em caracteres) para offset em bytes no statement.
pub fn error_byte_offset(stmt: &str, position: u32) -> usize {
    let n = position.saturating_sub(1) as usize;
    stmt.char_indices().nth(n).map_or(stmt.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_offset_counts_chars_not_bytes() {
        assert_eq!(error_byte_offset("select ção, x", 13), 14);
        assert_eq!(error_byte_offset("select x", 8), 7);
        assert_eq!(error_byte_offset("ab", 99), 2);
    }
}
