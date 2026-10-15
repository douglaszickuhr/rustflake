use std::fmt;

use rust_i18n::t;
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
    /// Falha no túnel SSH (conexão, autenticação, canal, known_hosts).
    Ssh(String),
    /// Host SSH fora do known_hosts. `key` é a chave pública em formato OpenSSH, para `ssh::trust_host`.
    HostKeyUnknown { host: String, port: u16, fingerprint: String, key: String },
    /// Erro devolvido pelo Snowflake (código numérico do Snowflake, ex. 001003).
    Snowflake { code: String, message: String },
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
            DbError::Connection(m) => write!(f, "{}: {m}", t!("err.prefix.connection")),
            DbError::Config(m) => write!(f, "{}: {m}", t!("err.prefix.config")),
            DbError::Keychain(m) => write!(f, "Keychain: {m}"),
            DbError::Io(m) => write!(f, "{}: {m}", t!("err.prefix.io")),
            DbError::Edit(m) => write!(f, "{}: {m}", t!("err.prefix.edit")),
            DbError::Unsupported(m) => write!(f, "{}: {m}", t!("err.prefix.unsupported")),
            DbError::Ssh(m) => write!(f, "SSH: {m}"),
            DbError::Snowflake { code, message } => write!(f, "[{code}] {message}"),
            DbError::HostKeyUnknown { host, port, fingerprint, .. } => {
                write!(f, "SSH: {}", t!("err.host_unknown", host = host, port = port, fingerprint = fingerprint))
            }
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
