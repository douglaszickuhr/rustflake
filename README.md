# rustbeaver

A fast, native SQL client for macOS, written in Rust on [GPUI](https://www.gpui.rs/) (via `gpui-kit`).
It speaks **PostgreSQL** and **Snowflake**, starts in well under a second and stays light on memory.

## Features

**Connections**
- PostgreSQL (with SSL modes) and Snowflake (SSO in the browser, key-pair, or programmatic access token).
- In-process SSH tunnel: reads `~/.ssh/config` aliases, asks before trusting unknown hosts (`known_hosts`), supports keys with passphrases and the SSH agent.
- Passwords, passphrases and tokens live in the macOS Keychain, never in the config file.
- Snowflake SSO caches the ID token in the Keychain, so reopening the app doesn't open the browser again while the token is valid.
- Per-connection environment color, database filter (Snowflake) and recent role/warehouse/database/schema picker.

**Editor**
- SQL highlighting and context-aware autocomplete: tables after `FROM`, columns by alias, three-part names, quoting when needed.
- Run the statement under the cursor or the whole script; each statement gets its own result tab.
- Formatter, line comments, query history with search, ⌘-click on a table name to open its definition.
- Scripts are saved automatically per connection.

**Results and tables**
- Virtualized grid that pages in more rows as you scroll; copy cells, rows or columns as TSV.
- Table tab with server-side filtering and sorting, and inline editing on PostgreSQL tables with a primary key.
- Snowflake table view: definition, tags, classification, masking, column lineage, and dbt descriptions and tests from a local dbt project.

**Query plans**
- `EXPLAIN` rendered as a diagram (estimated or actual), with per-node timings, hot-path highlighting, zoom and a details panel.
- PostgreSQL `EXPLAIN ANALYZE` always runs inside a transaction that is rolled back; on Snowflake the actual plan comes from `GET_QUERY_OPERATOR_STATS`.

**Snowflake administration (read-only)**
- Users & Roles: role inheritance, what a user unlocks, login history.
- Warehouses: configuration, who can use them, credits and usage.
- Company conventions (role naming, classification tag, masking policies) come from an optional per-connection TOML; see [`examples/configurations/acme.toml`](examples/configurations/acme.toml).

**App**
- Quick open (⌘K) across schemas, tables, open tabs and script contents.
- English, Portuguese and Spanish UI; light, dark or system theme.

## Requirements

- macOS (the app uses the Keychain and AppKit).
- Rust stable with edition 2024 support (developed on 1.96).

## Build and run

```sh
cargo run --release -p rb-app
```

The binary is called `rustbeaver`.

### Keychain prompts during development

Every unsigned build looks like a new app to macOS, so the Keychain asks for permission again after each build.
To avoid that, create a local code-signing certificate once:

```sh
scripts/dev-cert.sh
```

From then on, `cargo run` signs the binary before launching it (see `.cargo/config.toml` and `scripts/dev-run.sh`), and "Always Allow" sticks across builds.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| ⌘T / ⌘W | New SQL tab / close tab |
| ⌘↩ | Run the statement under the cursor |
| ⌘⇧↩ | Run the whole script |
| ⌘. | Cancel the running query |
| ⌃Space | Show completions (Tab or Enter accepts) |
| ⌘⇧F | Format SQL |
| ⌘⌥F | Find and replace |
| ⌘/ | Toggle line comment |
| ⌘E / ⌘⇧E | Estimated / actual query plan |
| ⌘= ⌘- ⌘0, ⌘ + scroll | Zoom the plan diagram / fit |
| ⌘K | Quick open |
| ⌘Y | Query history |
| ⌘S | Save table edits |
| ⌘C | Copy selected cells |
| F5 / ⌘R | Refresh the selected tree node |

## Where things are stored

Everything lives under `~/Library/Application Support/rustbeaver/`:

| Path | Contents |
|---|---|
| `connections.toml` | Connections and settings (no secrets) |
| `scripts/<connection id>/` | SQL scripts; closed tabs move to `scripts/closed/` |
| `history/<connection id>.jsonl` | Query history |
| `configurations/` | Optional company convention files (see the example above) |

## Project layout

| Path | What |
|---|---|
| `crates/rb-core` | Database engine: sessions, SSH tunnel, Snowflake, metadata, autocomplete, SQL lexer/splitter/formatter, plans, config |
| `crates/rb-app` | GPUI desktop app |
| `vendor/snowflake-connector-rs` | `snowflake-connector-rs` 1.1.0 with a small patch for the SSO ID token cache (see its `PATCH.md`) |
| `scripts/` | Dev signing, test bastion, performance seed data |

## Tests

Unit tests run anywhere:

```sh
cargo test --workspace
```

Integration tests run against Docker containers and are skipped when the environment variables are missing:

```sh
# PostgreSQL
docker run -d --name rb-pg -e POSTGRES_PASSWORD=rb -p 55432:5432 postgres:17
# SSH bastion on the same network (prints the RB_TEST_SSH value)
scripts/test-sshd.sh

RB_TEST_DATABASE_URL=postgres://postgres:rb@localhost:55432/postgres \
RB_TEST_SSH=127.0.0.1:52222 \
cargo test --workspace
```

Snowflake integration tests need a real account: set `RB_TEST_SNOWFLAKE_{ACCOUNT,USER,WAREHOUSE,ROLE,DATABASE}` plus either `RB_TEST_SNOWFLAKE_PRIVATE_KEY` (path to a `.p8`) or `RB_TEST_SNOWFLAKE_PAT`.

## License

[MIT](LICENSE). The bundled Geist fonts are under the SIL Open Font License (`crates/rb-app/assets/fonts/OFL.txt`), and the vendored Snowflake connector keeps its own MIT license.
