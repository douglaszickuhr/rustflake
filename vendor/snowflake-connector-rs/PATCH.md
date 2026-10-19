# rustbeaver patch on top of snowflake-connector-rs 1.1.0

SSO ID token cache (like the official connectors' `client_store_temporary_credential`):

- `wire/login_response.rs`: reads `data.idToken` (`LoginSession.id_token`).
- `Session::id_token()`: exposes the login's ID token.
- `AuthConfig::id_token(token)` → credential `AUTHENTICATOR=ID_TOKEN`, `TOKEN=…`.
- Empty `[workspace]` in Cargo.toml: only so the connector's tests run on their own.

- `wire/login_request.rs` + `auth/login.rs`: the login sends `CLIENT_APP_ID="Go"` and `CLIENT_APP_VERSION`; without an id the server recognizes, it does not return `idToken`. Side effect: as "Go", the server answers in Arrow; rustbeaver asks for `GO_QUERY_RESULT_FORMAT=JSON` on the session (rb-core/src/snowflake.rs).

The token is requested with the `CLIENT_STORE_TEMPORARY_CREDENTIAL=true` session parameter (existing API).
Tests: `cargo test --manifest-path vendor/snowflake-connector-rs/Cargo.toml --lib --features external-browser-sso,key-pair-auth id_token`.
When upgrading the connector, reapply these changes (or drop them if upstream gains support).
