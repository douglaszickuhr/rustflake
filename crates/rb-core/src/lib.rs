pub mod admin;
pub mod batch;
pub mod complete;
pub mod config;
pub mod dbt;
pub mod decode;
pub mod dialect;
pub mod error;
pub mod explain;
pub mod format;
pub mod history;
pub mod lexer;
pub mod lineage;
pub mod masking;
pub mod metadata;
pub mod org_config;
pub mod picker;
pub mod query_edit;
pub mod quick_open;
pub mod secrets;
pub mod session;
pub mod snowflake;
pub mod split;
pub mod ssh;
pub mod table;
pub mod table_info;
pub mod task;
pub mod unlocks;
pub mod user_view;
pub mod warehouse;

pub use error::DbError;
pub use tokio_postgres::types::Type;

rust_i18n::i18n!("locales", fallback = "en");

/// Translation guards, shared with rb-app's tests.
#[doc(hidden)]
pub mod i18n_check;

#[cfg(test)]
#[test]
fn every_key_is_translated_and_used_keys_exist() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    i18n_check::check(&root.join("src"), &root.join("locales/app.toml"));
}
