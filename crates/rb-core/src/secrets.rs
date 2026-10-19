use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

use crate::DbError;

const SERVICE: &str = "dev.rustflake";
/// Service used while the app was called rustbeaver; items move over on first read.
const LEGACY_SERVICE: &str = "dev.rustbeaver";
const NOT_FOUND: i32 = -25300; // errSecItemNotFound

/// Keychain account that holds the SSH key passphrase of a connection.
pub fn ssh_account(id: &str) -> String {
    format!("ssh:{id}")
}

/// Keychain account with the Snowflake private key passphrase of a connection.
pub fn key_account(id: &str) -> String {
    format!("key:{id}")
}

/// Keychain account with the SSO ID token (reopen without a browser while valid, ~4 h).
pub fn id_token_account(id: &str) -> String {
    format!("idtoken:{id}")
}

fn err(e: security_framework::base::Error) -> DbError {
    DbError::Keychain(e.to_string())
}

pub fn get_password(id: &str) -> Result<Option<String>, DbError> {
    match get_generic_password(SERVICE, id) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|e| DbError::Keychain(e.to_string())),
        Err(e) if e.code() == NOT_FOUND => migrate_legacy(id),
        Err(e) => Err(err(e)),
    }
}

fn migrate_legacy(id: &str) -> Result<Option<String>, DbError> {
    let bytes = match get_generic_password(LEGACY_SERVICE, id) {
        Ok(b) => b,
        Err(e) if e.code() == NOT_FOUND => return Ok(None),
        Err(e) => return Err(err(e)),
    };
    set_generic_password(SERVICE, id, &bytes).map_err(err)?;
    let _ = delete_generic_password(LEGACY_SERVICE, id);
    String::from_utf8(bytes).map(Some).map_err(|e| DbError::Keychain(e.to_string()))
}

pub fn set_password(id: &str, password: &str) -> Result<(), DbError> {
    set_generic_password(SERVICE, id, password.as_bytes()).map_err(err)
}

pub fn delete_password(id: &str) -> Result<(), DbError> {
    let _ = delete_generic_password(LEGACY_SERVICE, id);
    match delete_generic_password(SERVICE, id) {
        Err(e) if e.code() != NOT_FOUND => Err(err(e)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real Keychain; run manually: `cargo test -p rb-core keychain -- --ignored`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let id = format!("rb-test-{}", std::process::id());
        assert_eq!(get_password(&id).unwrap(), None);
        set_password(&id, "s3cret").unwrap();
        assert_eq!(get_password(&id).unwrap().as_deref(), Some("s3cret"));
        delete_password(&id).unwrap();
        assert_eq!(get_password(&id).unwrap(), None);
    }
}
