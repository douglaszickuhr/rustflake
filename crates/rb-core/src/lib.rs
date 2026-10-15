pub mod admin;
pub mod batch;
pub mod complete;
pub mod config;
pub mod decode;
pub mod dialect;
pub mod error;
pub mod explain;
pub mod format;
pub mod history;
pub mod lexer;
pub mod metadata;
pub mod quick_open;
pub mod secrets;
pub mod session;
pub mod snowflake;
pub mod split;
pub mod ssh;
pub mod table;

pub use error::DbError;

rust_i18n::i18n!("locales", fallback = "en");

#[cfg(test)]
mod i18n_check;

#[cfg(test)]
#[test]
fn every_key_is_translated_and_used_keys_exist() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    i18n_check::check(&root.join("src"), &root.join("locales/app.toml"));
}
pub use tokio_postgres::types::Type;
