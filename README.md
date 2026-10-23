# rustflake

[![CI](https://github.com/douglaszickuhr/rustflake/actions/workflows/rust.yml/badge.svg)](https://github.com/douglaszickuhr/rustflake/actions/workflows/rust.yml)
[![Release](https://img.shields.io/github/v/release/douglaszickuhr/rustflake)](https://github.com/douglaszickuhr/rustflake/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

rustflake is a native SQL client for Snowflake and PostgreSQL on macOS.
It is written in Rust on [GPUI](https://www.gpui.rs/). It starts in less than one second and uses little memory.

## Install

1. Download the `.dmg` file from [Releases](https://github.com/douglaszickuhr/rustflake/releases/latest).
2. Open the file and move **rustflake** to **Applications**.

The app is signed and notarized. It needs macOS 11 or later on Apple Silicon.
When macOS asks for Keychain access, click **Always Allow**.

## Features

- **Editor**: autocomplete, column hover with sample values, formatter, query history.
- **Results**: a fast grid. Right-click a cell or a header to count, filter or get distinct values.
- **Tables**: filter, sort and edit rows (PostgreSQL tables with a primary key).
- **Snowflake objects**: definitions, tags, masking policies, lineage, tasks, pipes, stages and procedures.
- **Query plans**: `EXPLAIN` as a diagram, with the time of each node.
- **Access**: users, roles and warehouses (read-only).
- **Go to anything** (⌘K): find schemas, tables, tabs and scripts.
- **Connections**: Snowflake SSO, key-pair or token. PostgreSQL with SSL and SSH tunnels.
- **Secrets**: the macOS Keychain keeps all passwords and tokens.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| ⌘↵ | Run the statement at the cursor |
| ⌘⇧↵ | Run the script |
| ⌘. | Stop the query |
| ⌘I | Show the completions |
| ⌘E / ⌘⇧E | Show the estimated / actual plan |
| ⌘K | Go to anything |
| ⌘Y | Show the query history |
| ⌘T / ⌘W | Open / close a tab |

Click **+** in the status bar to see all shortcuts.

## Data

rustflake keeps its data in `~/Library/Application Support/rustflake/`.
Only your user can read this folder. Passwords are not in this folder.

## Development

You need macOS and Rust stable (edition 2024).

```sh
cargo run --release -p rb-app   # build and run
cargo test --workspace          # unit tests
```

To stop Keychain prompts after each build, run `scripts/dev-cert.sh` one time.

To make a release, set the same `version` in `crates/*/Cargo.toml`. Then push a tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

The [release workflow](.github/workflows/release.yml) builds, signs and notarizes the `.dmg`.

## License

[MIT](LICENSE). The Geist fonts use the SIL Open Font License.
