# Patch do rustbeaver sobre snowflake-connector-rs 1.1.0

Cache de ID token do SSO (como o `client_store_temporary_credential` dos conectores oficiais):

- `wire/login_response.rs`: lê `data.idToken` (`LoginSession.id_token`).
- `Session::id_token()`: expõe o ID token do login.
- `AuthConfig::id_token(token)` → credencial `AUTHENTICATOR=ID_TOKEN`, `TOKEN=…`.
- `[workspace]` vazio no Cargo.toml: só para rodar os testes do conector isolado.

- `wire/login_request.rs` + `auth/login.rs`: o login manda `CLIENT_APP_ID="Go"` e `CLIENT_APP_VERSION`; sem um id que o servidor reconheça, ele não devolve `idToken`. Efeito colateral: como "Go", o servidor responde em Arrow; o rustbeaver pede `GO_QUERY_RESULT_FORMAT=JSON` na sessão (rb-core/src/snowflake.rs).

O token é pedido com a session parameter `CLIENT_STORE_TEMPORARY_CREDENTIAL=true` (API já existente).
Testes: `cargo test --manifest-path vendor/snowflake-connector-rs/Cargo.toml --lib --features external-browser-sso,key-pair-auth id_token`.
Ao atualizar o conector, reaplicar estas mudanças (ou remover se o upstream passar a suportar).
