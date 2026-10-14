use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

use crate::DbError;

const SERVICE: &str = "dev.rustbeaver";
const NOT_FOUND: i32 = -25300; // errSecItemNotFound

fn err(e: security_framework::base::Error) -> DbError {
    DbError::Keychain(e.to_string())
}

pub fn get_password(id: &str) -> Result<Option<String>, DbError> {
    match get_generic_password(SERVICE, id) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|e| DbError::Keychain(e.to_string())),
        Err(e) if e.code() == NOT_FOUND => Ok(None),
        Err(e) => Err(err(e)),
    }
}

pub fn set_password(id: &str, password: &str) -> Result<(), DbError> {
    set_generic_password(SERVICE, id, password.as_bytes()).map_err(err)
}

pub fn delete_password(id: &str) -> Result<(), DbError> {
    match delete_generic_password(SERVICE, id) {
        Err(e) if e.code() != NOT_FOUND => Err(err(e)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mexe no Keychain real; rode manualmente: `cargo test -p rb-core keychain -- --ignored`.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let id = format!("rb-test-{}", std::process::id());
        assert_eq!(get_password(&id).unwrap(), None);
        set_password(&id, "s3nha").unwrap();
        assert_eq!(get_password(&id).unwrap().as_deref(), Some("s3nha"));
        delete_password(&id).unwrap();
        assert_eq!(get_password(&id).unwrap(), None);
    }
}
