# rustflake

[![CI](https://github.com/douglaszickuhr/rustflake/actions/workflows/rust.yml/badge.svg)](https://github.com/douglaszickuhr/rustflake/actions/workflows/rust.yml)
[![Release](https://img.shields.io/github/v/release/douglaszickuhr/rustflake)](https://github.com/douglaszickuhr/rustflake/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![macOS](https://img.shields.io/badge/platform-macOS-lightgrey)

A fast, native SQL client for **Snowflake** and **PostgreSQL** on macOS, written in Rust on [GPUI](https://www.gpui.rs/).
It starts in well under a second, stays light on memory, and knows Snowflake well: lineage, tasks, pipes, stages, dynamic tables, roles and warehouses, all a click away from your SQL.

<!-- GIF hero: .github/media/hero.gif
     Open a Snowflake connection → expand a schema in the tree → type a query with autocomplete
     → ⌘↵ → scroll the result → ⌘-click a table name to open its definition. ~15 s. -->

## Install

1. Download the latest `rustflake-<version>-macos-arm64.dmg` from [Releases](https://github.com/douglaszickuhr/rustflake/releases/latest).
2. Open it and drag **rustflake** to **Applications**.

The app is signed and notarized, so it opens without Gatekeeper warnings. It needs macOS 11 or later on Apple Silicon.

The first time rustflake reads a saved password, macOS asks for Keychain access: choose **Always Allow**, and later versions won't ask again.

## What you can do

### Write SQL with an editor that knows your schema

- Autocomplete that follows context: tables after `FROM`, columns by alias, CTE and subquery columns, three-part names, quoting when needed.
- Hover a column to see its type, table and most common values; hover a table for its facts.
- Run the statement under the cursor (⌘↵), the selection, or the whole script (⌘⇧↵); each statement gets its own result tab.
- Formatter, find and replace, line comments, searchable query history (⌘Y), and ⌘-click on a table name to open it.
- Scripts are saved automatically per connection.

<!-- GIF: .github/media/editor.gif
     Type `select * from orders o join customers c on ` and let autocomplete fill the join;
     hover `o.status` to show the sample values card; ⌘↵. ~10 s. -->

### Explore results without writing more SQL

- A virtualized grid that pages in rows as you scroll; copy cells, rows or columns as TSV.
- Right-click a cell to count by its column, keep or exclude that value, or filter nulls; right-click a header for distinct values or min/max. rustflake rewrites the query behind the result and runs it in a new tab.
- Long values open a card with JSON colours, folding and search; `http(s)` values are links (⌘-click).
- Table tabs with server-side filtering and sorting, and inline editing on PostgreSQL tables with a primary key (⌘S saves).

<!-- GIF: .github/media/results.gif
     Run a query → right-click a status cell → "Count by status" opens and runs in a new tab
     → open a JSON cell's card and fold a key. ~10 s. -->

### Understand a Snowflake table at a glance

- Definition with column search, tags, classification, masking policies and dbt descriptions and tests from a local dbt project.
- Badges for Iceberg tables (catalog, external volume) and dynamic tables (target lag, refresh state, data age).
- Lineage graph (Enterprise): upstream and downstream objects with expand/collapse and re-centering, each object's definition and input queries, and a column's lineage highlighted on the graph.

<!-- GIF: .github/media/lineage.gif
     Open a table → Definition → hover a column to show its lineage card → open the Lineage panel,
     expand one upstream node, click a column to highlight its path. ~15 s. -->

### Keep an eye on pipelines

- **Tasks**: status, schedule, recent runs with errors and query ids, the task graph, and the data flow from the last runs (tables read → task → tables written).
- **Pipes**: status, pending files, last ingest, and recent loads with failures first.
- **Stages**: location and integration, the newest files, and a preview of one file.
- **Procedures and functions**: double-click to open the code in a script, ending with a ready-to-fill `CALL`.

<!-- Screenshot: .github/media/task.png — a task tab with recent runs and the data flow. -->

### See where a query spends its time

- `EXPLAIN` rendered as a diagram (estimated ⌘E or actual ⌘⇧E), with per-node timings, hot-path highlighting, zoom and a details panel.
- PostgreSQL `EXPLAIN ANALYZE` always runs inside a transaction that is rolled back; on Snowflake the actual plan comes from `GET_QUERY_OPERATOR_STATS`.

<!-- Screenshot: .github/media/plan.png — an actual plan with the hot path highlighted and the details panel open. -->

### Answer access questions (read-only)

- **Users & Roles**: role inheritance, what a user unlocks, login history.
- **Warehouses**: configuration, who can use them, credits and usage.
- Company conventions (role naming, classification tag, masking policies) come from an optional per-connection TOML; see [`examples/configurations/acme.toml`](examples/configurations/acme.toml).

### Jump anywhere

**Go to anything** (⌘K) searches schemas, tables, open tabs and script contents. Paste a `DB.SCHEMA.NAME` path and it opens that exact object.

<!-- GIF: .github/media/goto.gif — ⌘K, type part of a table name, Enter; then paste a full path. ~6 s. -->

## Connections

- **Snowflake**: SSO in the browser, key-pair, or programmatic access token. SSO caches the ID token in the Keychain, so reopening the app doesn't open the browser while the token is valid (see [the caveat](#snowflake-id-token-caveat)).
- **PostgreSQL**: SSL `disable`, `prefer` (the default, like libpq) or `require`.
- In-process **SSH tunnel**: reads `~/.ssh/config` aliases, asks before trusting unknown hosts, supports keys with passphrases and the SSH agent.
- Passwords, passphrases and tokens live in the macOS Keychain, never in the config file.
- Per-connection environment colour, database filter (Snowflake) and a picker for recent role, warehouse, database and schema.
- English, Portuguese and Spanish UI; light, dark or system theme.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| ⌘T / ⌘W | New SQL tab / close tab |
| ⌘1–⌘8 / ⌘9 | Go to tab / last tab |
| ⌘↵ | Run the statement under the cursor (or the selection) |
| ⌘⇧↵ | Run the whole script |
| ⌘. | Cancel the running query |
| ⌃Space | Show completions (Tab or Enter accepts) |
| ⌘⇧F | Format SQL |
| ⌘⌥F | Find and replace |
| ⌘/ | Toggle line comment |
| ⌘E / ⌘⇧E | Estimated / actual query plan |
| ⌘= ⌘- ⌘0, ⌘ + scroll | Zoom the plan diagram / fit |
| ⌘K | Go to anything |
| ⌘Y | Query history |
| ⌘, | Edit the connection |
| ⌘S | Save table edits |
| ⌘C | Copy selected cells |
| ⌘R | Refresh the open table, users & roles or warehouses tab |
| F5 | Refresh the selected tree node |

The **+** in the status bar lists the rest.

## Where things are stored

Everything lives under `~/Library/Application Support/rustflake/`, which is owner-only (`0700`; new files `0600`),
since scripts and history may contain passwords:

| Path | Contents |
|---|---|
| `connections.toml` | Connections and settings (no secrets) |
| `scripts/<connection id>/` | SQL scripts and saved table tabs; closed tabs move to `scripts/closed/` |
| `history/<connection id>.jsonl` | Query history |
| `settings.toml` | Language, theme, recent picks and the connections to reopen |
| `configurations/` | Optional company convention files (see the example above) |

## Snowflake ID token caveat

Snowflake only issues an SSO ID token to clients it recognizes, so the vendored connector logs in as
`CLIENT_APP_ID="Go"` (see `vendor/snowflake-connector-rs/PATCH.md`). Consequences:

- Admins see rustflake sessions as the Go driver in `LOGIN_HISTORY`, `QUERY_HISTORY` and `SESSIONS`.
- Results then come back as Arrow by default, so rustflake asks for `GO_QUERY_RESULT_FORMAT=JSON`.
- The account needs `ALLOW_ID_TOKEN = TRUE`; without it every launch opens the browser again.
- It relies on undocumented server behaviour: if Snowflake changes it, SSO keeps working but the token cache stops.

## Development

Requires macOS and Rust stable with edition 2024 support (developed on 1.96).

```sh
cargo run --release -p rb-app
```

The binary is called `rustflake`.

### Keychain prompts during development

Every unsigned build looks like a new app to macOS, so the Keychain asks for permission again after each build.
`cargo run` signs the binary before launching it (see `.cargo/config.toml` and `scripts/dev-run.sh`), preferring an
"Apple Development" certificate if you have one. Without it, create a local code-signing certificate once:

```sh
scripts/dev-cert.sh
```

### Tests

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

### Releasing

Bump `version` in both `crates/*/Cargo.toml`, merge to `main`, wait for CI, then push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The [release workflow](.github/workflows/release.yml) builds the app, signs and notarizes it, and publishes the `.dmg`
with generated notes. `scripts/bundle-macos.sh` builds the same `.dmg` locally (signed when `MACOS_SIGN_IDENTITY` is set).

### Project layout

| Path | What |
|---|---|
| `crates/rb-core` | Database engine: sessions, SSH tunnel, Snowflake, metadata, autocomplete, SQL lexer/splitter/formatter, plans, config |
| `crates/rb-app` | GPUI desktop app |
| `vendor/snowflake-connector-rs` | `snowflake-connector-rs` 1.1.0 with a small patch for the SSO ID token cache (see its `PATCH.md`) |
| `scripts/` | Bundling, dev signing, test bastion, performance seed data |

## License

[MIT](LICENSE). The bundled Geist fonts are under the SIL Open Font License (`crates/rb-app/assets/fonts/OFL.txt`), and the vendored Snowflake connector keeps its own MIT license.
