# Warehouses Tab Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only "Warehouses" tab for Snowflake connections covering configuration, who can use each warehouse, 14-day credits, usage, queueing/spill and the slowest queries with their actual plan.

**Architecture:**
- **Core:** a new pure `crates/rb-core/src/warehouse.rs` holds the parsers, formatters, SQL builders, the reverse "users who reach it" search and the usage aggregation, plus thin async fetchers.
- **App:** a new `crates/rb-app/src/warehouse_tab.rs` (`TabView::Warehouses`) follows `admin_tab.rs` (list on the left, details on the right, `Load<T>`, a generation counter, own sessions).
- **Navigation:** clicking a role or user calls back into `Workspace`, which opens or focuses Users & Roles at that target.

**Tech Stack:** Rust 2024, gpui-kit 0.7.3, rust-i18n; reuses `admin::{quote_ident, sql_str, RoleGraph, role_graph}`, `explain`/`PlanView`, `Session::actual_plan`.

**Spec:** `docs/superpowers/specs/2026-10-16-warehouses-design.md`

## Global Constraints

- **Do not commit.** The user commits.
- Code comments are short and in Portuguese, and say why.
- rb-app tests use `#[::core::prelude::v1::test]`.
- All UI text lives in `crates/rb-app/locales/app.toml` (en/pt/es); `i18n_check` must pass.
- Read-only: no SUSPEND/RESUME/ALTER.
- Identifiers go through `quote_ident`, literals through `sql_str`.
- Each section fails on its own; ↻ bumps a `generation` counter and discards stale results.
- No new crates.

## Review Focus

1. A warehouse name with lowercase letters or quotes: `SHOW GRANTS ON WAREHOUSE "x"`, the metering query and the usage literal are all quoted or escaped correctly.
2. `auto_suspend` is null or 0 (never suspends): show "never", don't crash.
3. A warehouse with no queries in 14 days: the usage section shows zeros, there is no undersized warning, and the lists are empty.
4. The current role lacks MONITOR on the warehouse: the Plan dialog shows the error, and the tab keeps working.
5. No ACCOUNT_USAGE access: usage and the "users reaching" line fail with the IMPORTED PRIVILEGES hint, while the configuration and the grants still show.

---

### Task 1: `warehouse.rs`: list, config formatting, grants

**Files:**
- Create: `crates/rb-core/src/warehouse.rs`
- Modify: `crates/rb-core/src/lib.rs` (`pub mod warehouse;`)

**Interfaces:**
- Produces:
  - `pub struct WarehouseInfo { name, state, kind, size, min_clusters: u32, max_clusters: u32, running: u64, queued: u64, auto_suspend: Option<u64> /*secs*/, auto_resume: bool, scaling_policy: Option<String>, owner, comment, created_on, resumed_on, resource_monitor: Option<String> }`
  - `pub fn parse_warehouses(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<WarehouseInfo>` (sorted by name)
  - `pub enum Suspend { Never, Secs(u64) }`
  - `pub fn suspend_of(w: &WarehouseInfo) -> Suspend` (None or 0 → Never)
  - `pub fn clusters_label(w) -> Option<String>`: `None` when 1–1, else `"1–3 · STANDARD"`
  - `pub struct WhGrant { pub privilege: String, pub role: String }`
  - `pub fn parse_wh_grants(names, rows) -> Vec<WhGrant>`: only rows with `granted_to = ROLE`; role names unquoted with `admin` semantics
  - `pub fn grants_by_privilege(g: &[WhGrant]) -> Vec<(String, Vec<String>)>`: ordered OWNERSHIP, MODIFY, OPERATE, MONITOR, USAGE, then others; roles sorted
  - Async: `pub async fn warehouses(s) -> Result<Vec<WarehouseInfo>, DbError>` (`SHOW WAREHOUSES`) and `pub async fn wh_grants(s, name) -> Result<Vec<WhGrant>, DbError>` (`SHOW GRANTS ON WAREHOUSE {quote_ident}`)

- [ ] **Step 1: Failing tests.** In the `tests` module, with a `t(names, rows)` fixture helper like `admin.rs` has:
  - `parse_warehouses` with columns out of order (`NAME`, `state`, `size`, `min_cluster_count`, `max_cluster_count`, `running`, `queued`, `auto_suspend`, `auto_resume`, `scaling_policy`, `owner`, `resource_monitor`). Include one row with `auto_suspend` null and one with `"600"`, and check the result is sorted by name.
  - `suspend_of`: null → Never, `"0"` → Never, `"600"` → Secs(600).
  - `clusters_label`: 1–1 → None; 1–3 STANDARD → `Some("1–3 · STANDARD")`.
  - `parse_wh_grants` keeps only `granted_to = ROLE` and unquotes `"low"`.
  - `grants_by_privilege` puts OWNERSHIP first and USAGE after OPERATE.
- [ ] **Step 2:** `cargo test -p rb-core --lib warehouse`, expected to FAIL (the functions don't exist yet).
- [ ] **Step 3: Implement.**
  - Column lookup by name, using `crate::snowflake::field`.
  - Bools come as `"true"`.
  - Numbers parse to 0 on failure.
  - Make `admin::unquote` `pub(crate)` and reuse it.
- [ ] **Step 4:** `cargo test -p rb-core --lib`: PASS.

### Task 2: Users reaching a warehouse

**Files:** Modify `crates/rb-core/src/warehouse.rs`

**Interfaces:**
- Consumes: `admin::RoleGraph` (role → roles it inherits).
- Produces:
  - `pub fn users_reaching(granted: &[String], graph: &RoleGraph, user_roles: &HashMap<String, Vec<String>>) -> Vec<String>`
    - The roles that "reach" = the granted roles plus every role that inherits any of them, transitively, via a reverse BFS with a visited set.
    - The result is the users (sorted, deduplicated) whose direct roles intersect that set.
  - `pub const USER_GRANTS_SQL: &str` = `select grantee_name, role from snowflake.account_usage.grants_to_users where deleted_on is null`
  - `pub fn parse_user_grants_all(names, rows) -> HashMap<String, Vec<String>>` (user → roles)
  - `pub async fn all_user_grants(s) -> Result<HashMap<..>, DbError>`

- [ ] **Step 1: Failing tests.**
  - Graph `A→WH_ROLE`, `B→A`, `C→B`, `X→Y`; users `u1:[C]`, `u2:[X]`, `u3:[WH_ROLE]`; granted `[WH_ROLE]` → `["u1","u3"]`.
  - A cycle `A→B`, `B→A` with granted `[A]` must terminate.
  - A diamond counts each user once.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3: Implement.** Build the reverse map `child → parents` from the graph and BFS from the granted roles.
- [ ] **Step 4:** PASS.

### Task 3: Credits and usage (SQL + aggregation)

**Files:** Modify `crates/rb-core/src/warehouse.rs`

**Interfaces:**
- Produces:
  - `pub fn credits_sql(wh: &str) -> String`: 14 days generated server-side (`generator(rowcount => 14)` + `row_number`), left-joined to `table(information_schema.warehouse_metering_history(dateadd('day', -13, current_date()), dateadd('day', 1, current_date()), <sql_str(wh)>))` and grouped by day. It returns `day` (string `YYYY-MM-DD` via `to_char`) and `credits` (string).
  - `pub fn parse_credits(names, rows) -> Vec<(String, f64)>`: sorted by day.
  - `pub struct UsageUser { name, queries: u64, elapsed_ms: u64 }`
  - `pub struct SlowQuery { query_id, user, elapsed_ms: u64, start: String, text: String }`
  - `pub struct WhUsage { total: u64, queued: u64, avg_queue_ms: f64, max_queue_ms: f64, spilled_local: u64, spilled_remote: u64, top_users: Vec<UsageUser>, slow: Vec<SlowQuery> }`
  - `pub fn usage_sql(wh: &str) -> String`
    - Over `snowflake.account_usage.query_history` with `warehouse_name = <sql_str>` and `start_time >= dateadd(day,-14,current_timestamp())`.
    - `UNION ALL` rows with columns `kind, a, b, c, d, e` (all strings):
      - `'total'` (count, count_if(queued_overload_time>0), avg of queued_overload_time where >0, max, sum spill local, sum spill remote spread over a..e as needed; split into two rows if it doesn't fit);
      - `'user'` top 5 (user, count, sum elapsed);
      - `'slow'` top 10 (query_id, user, elapsed, `to_char(start_time,'YYYY-MM-DD HH24:MI')`, `left(query_text, 200)`).
  - `pub fn parse_usage(names, rows) -> WhUsage`: users sorted by queries desc, slow by elapsed desc.
  - `pub fn undersized(u: &WhUsage) -> bool`: `total > 0 && (queued * 20 > total || spilled_remote > 0)`, i.e. more than 5%.
  - Async: `pub async fn credits(s, wh)` and `pub async fn usage(s, wh)`.

- [ ] **Step 1: Failing tests.**
  - `credits_sql("a'b")` contains `'a''b'` and `rowcount => 14`.
  - `usage_sql` contains `warehouse_name = 'X'`.
  - `parse_usage` from fixture rows with `total`, `queue`, `user`×2 out of order, `slow`×2: check the sorting and the numbers.
  - Undersized:
    - 100 queries, 6 queued → true;
    - 100, 5, no spill → false;
    - remote spill > 0 → true;
    - total 0 → false.
  - `parse_credits` sorts by day.
- [ ] **Step 2:** FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4:** `cargo test -p rb-core --lib`: PASS.

### Task 4: `warehouse_tab.rs` + wiring

**Files:**
- Create: `crates/rb-app/src/warehouse_tab.rs`
- Modify:
  - `crates/rb-app/src/main.rs` (`mod warehouse_tab;`)
  - `crates/rb-app/src/workspace.rs` (`TabView::Warehouses`, `open_warehouses`, the rail button, `admin_goto`)
  - `crates/rb-app/src/admin_tab.rs` (make `Target` and `AdminTab::open` usable from outside: `pub`)
  - `crates/rb-app/locales/app.toml` (`wh.*` keys)

**Interfaces:**
- Consumes: Tasks 1–3, `admin::role_graph`, `admin_tab::{Target, Load, explain}`, `PlanView`, `Session::actual_plan`.
- Produces:
  - `WarehouseTab::new(conn: ConnTarget, goto: Rc<dyn Fn(Target, &mut Window, &mut App)>, window, cx)`
  - `conn_id()`, `title()`
  - Workspace:
    - `fn open_warehouses(&mut self, conn_id, window, cx)` opens or focuses one per connection.
    - `fn admin_goto(&mut self, conn_id, target: Target, window, cx)` opens or focuses Users & Roles and calls `AdminTab::open(target)`. For it to work after a fresh open, `AdminTab` needs `pending: Option<Target>`, which is opened once the user and role lists load. Implement that and record it as a ruling.

- [ ] **Step 1: Rail and tab wiring.**
  - The rail button `rail-wh` stops being disabled, shows only for Snowflake, and calls `open_warehouses`; it gets a new tooltip.
  - Add arms to `TabView`'s `title`/`conn_id`/`render_main`.
- [ ] **Step 2: List.**
  - The filter, the counter `wh.of`, and rows `name` / `SIZE · state · N queries`, the same structure as the admin list.
  - ↻ bumps the generation and clears the caches.
  - `SHOW WAREHOUSES` runs on the `session`.
- [ ] **Step 3: Details.**
  - Header with the state pill: Tag success for STARTED, warning for RESIZING or SUSPENDING, secondary otherwise.
  - Configuration rows via a `kv` helper. That's a copy of the admin one; move both to a small shared `fn kv` in `admin_tab.rs` made `pub(crate)`, and reuse it.
  - Grants by privilege, each role a clickable Tag → `goto(Target::Role)`.
  - "N users reach it": an expandable list where each user → `goto(Target::User)`. It uses the role graph and the user grants, both loaded once per tab on the `usage_session`.
- [ ] **Step 4: Credits and usage.**
  - Both load when a warehouse is selected (cached per name), on the `usage_session`.
  - Credits:
    - a strip of 14 boxes (opacity by credits relative to the max);
    - the total;
    - a tooltip `day · X credits`.
  - Usage:
    - top users;
    - queueing & spill rows, plus the warning `wh.undersized`;
    - the slow queries list. Each row shows the text (one line, ellipsis), user, time and date, the copyable query ID (reuse the `Query ID … ⧉` button, moved to a small `pub(crate)` helper) and a **Plan** button. The button opens `window.open_dialog` holding a `PlanView` loaded via `Session::actual_plan(&id, Some(elapsed))`; while loading it shows "loading…", and on error it shows the message with the `wh.hint_monitor` hint.
- [ ] **Step 5:** i18n keys (en/pt/es) for every new string.
  - Run `cargo build -p rb-app`, then `cargo test -p rb-app` and `cargo test -p rb-core --lib`: all pass.
- [ ] **Step 6:** Release build. Ask the user to check by hand:
  - the rail opens the tab;
  - the list and the filter;
  - the configuration;
  - clicking a role or user opens Users & Roles;
  - the credits strip;
  - usage;
  - the Plan dialog;
  - ↻.

## Final review

One opus reviewer over the diff of all four tasks, with the spec, this plan, the Review Focus above and the ledger's rulings.
