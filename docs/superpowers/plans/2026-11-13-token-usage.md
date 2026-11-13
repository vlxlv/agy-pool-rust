# Token Usage Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist token usage per account in hourly buckets and show it for 1h / 24h / 7d / total in the dashboard (`GET /api/v1/tokens`), a new `agy-pool tokens` command, and summary lines in `agy-pool status -v` and `agy-pool stats`.

**Architecture:** The proxy adds each finished generation attempt's usage to an in-memory map keyed by (UTC hour bucket, account id) — a mutex-guarded `HashMap`, no I/O on the request path. The daemon flushes that map into a new `token_usage_hourly` table in `state.db` every 10 s in one transaction (and once at shutdown), so token accounting never competes with account selection (100 ms budget) for the state.db write lock. Readers sum the table plus the not-yet-flushed map.

**Tech Stack:** Rust 2024, rusqlite (bundled), tokio, hyper 1.12, serde/serde_json, jiff; single-file HTML/JS dashboard. No new crates.

**Spec:** this plan's "Design decisions" section (the reviewed design; there is no separate spec file).

## Design decisions

1. Table `token_usage_hourly`, primary key `(hour_bucket, account_id)`, `WITHOUT ROWID`; `hour_bucket = ts - ts.rem_euclid(3600)` (UTC). Created with `CREATE TABLE IF NOT EXISTS` on **every** open (like `inflight_leases`), no schema-version bump, so older binaries keep working on the same database.
2. Counters: prompt, cached, output, thinking, request_count, aborted_count. **Cached is part of prompt**: total = prompt + output + thinking. All inputs are clamped to `>= 0` (`UsageTap` reports "cached not seen" as `-1`).
3. Only generation attempts that got a 2xx response are recorded. A stream that ends early (client disconnect, upstream truncation) is recorded with whatever usage was seen and counted in `aborted_count`.
4. Ranges: `1h` = the current clock hour (since HH:00); `24h` / `7d` = the current hour plus the 23 / 167 before it; `total` = everything.
5. Account ids are reused (`next_account_id` takes the lowest free `acc_N`). When an account is removed, its rows are folded into account id `_removed` in the same transaction, so pool totals stay intact and a reused id starts at zero. A flush whose account no longer exists also goes to `_removed`.
6. Rows are kept forever (about 9k rows per account per year).

## Global Constraints

- Work only in `/home/claude/work/agy-pool-rust-tokens` on branch `feat/token-usage`. Never touch `/home/claude/work/agy-pool-rust` or any other directory.
- Run tests **only** through `timeout 900 scripts/test.sh <cargo args>` (cargo is `~/.cargo/bin/cargo`). Never run `cargo test` directly.
- Never start, stop or restart the live `agy-pool` gateway; never run release/LTO builds or benchmarks on this host.
- Before each commit: `~/.cargo/bin/cargo fmt` (`max_width = 120`), then `touch` one changed `.rs` file and run `timeout 900 scripts/test.sh clippy --all-targets -- -D warnings` (must be clean).
- No new dependencies. Do not create backup files (`*.bak*`). Do not push. Commit only the files the task lists. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Nothing on the proxy request path may touch the database or block; `record_token_usage` only locks the in-memory map.
- Dashboard responses never contain storage error text: log it with `crate::observability::log_message_level(LogLevel::Error, "DASHBOARD", …)` and return a fixed message. Account emails are never returned by `/api/v1/tokens`.
- Use `crate::clock::now_unix()` (the test-controllable clock), not `SystemTime::now()` / `jiff::Timestamp::now()`, for bucket times.

## Review Focus

1. **A client disconnects mid-stream.** The attempt must be recorded once, as aborted, with the usage seen so far — never twice (completion + drop). `emit_audit` already runs exactly once per stream; the reviewer must confirm the hook lives there and nowhere else on the stream path.
2. **An account is removed and its id reused.** The new account must start at zero and the pool total must not shrink. Pinned by `removed_account_usage_moves_to_removed_row` (Task 1).
3. **state.db write fails during a flush.** The counts must stay in memory for the next flush, not vanish. Reviewer checks the merge-back in `flush_token_usage`; there is no deterministic test.
4. **Usage reported without cached tokens (`-1`) or with all-zero counts.** Sums must never go negative. Pinned by `record_clamps_negative_counts` (Task 1) and the `acc_2` case in `test_tokens_endpoint` (Task 3).
5. **Dashboard asked for a range it does not know (`?range=2d`).** Must be `400 INVALID_ARGUMENT`, not a silent default. Pinned in `test_tokens_endpoint` (Task 3).

---

## File Structure

| File | Responsibility |
|---|---|
| `src/storage/tokens.rs` (new) | Table DDL, in-memory pending map, record / flush / query / fold-on-remove, ranges, display name |
| `src/storage/mod.rs` | `pub mod tokens;` |
| `src/storage/db.rs` | Create the table on every open |
| `src/storage/pool.rs` | Fold a removed account's usage before deleting it |
| `src/proxy/body.rs` | Record usage when a generation stream finishes or aborts |
| `src/proxy/dispatch.rs` | Record usage for a buffered generation response (safety net) |
| `src/daemon/server.rs` | 10 s flush task; final flush at shutdown |
| `src/proxy/tests/tokens.rs` (new), `src/proxy/tests/mod.rs` | Proxy-level tests |
| `src/dashboard/dto.rs`, `src/dashboard/mod.rs` | `GET /api/v1/tokens` |
| `src/dashboard/assets/index.html` | Token usage panel |
| `tests/dashboard.rs` | Endpoint and asset tests |
| `src/cli/tokens.rs` (new), `src/cli/mod.rs`, `src/cli/status.rs`, `src/cli/stats.rs` | `agy-pool tokens`, summary lines |

---

### Task 1: Storage — table, pending map, flush, queries, removal fold

**Files:**
- Create: `src/storage/tokens.rs`
- Modify: `src/storage/mod.rs` (add `pub mod tokens;` next to `pub mod pool;`)
- Modify: `src/storage/db.rs` (in `StateDb::open`, right after `test_conn.execute_batch(INFLIGHT_LEASE_DDL)?;`)
- Modify: `src/storage/pool.rs` (in `save_pool_to_tx`, before `tx.execute("DELETE FROM accounts WHERE id = ?1", [&existing_id])?;`)

**Interfaces:**
- Produces (used by Tasks 2–4), all in `crate::storage::tokens`:
  - `pub const TOKEN_USAGE_DDL: &str`, `pub const REMOVED_ACCOUNT_ID: &str = "_removed"`, `pub const FLUSH_INTERVAL: Duration` (10 s)
  - `pub struct TokenCounts { pub prompt: i64, pub cached: i64, pub output: i64, pub thinking: i64, pub requests: i64, pub aborted: i64 }` with `fn total(&self) -> i64`, `fn add(&mut self, other: &TokenCounts)`; derives `Debug, Default, Clone, Copy, PartialEq, Eq`
  - `pub enum TokenRange { Hour, Day, Week, Total }` with `fn parse(&str) -> Option<Self>`, `fn as_str(self) -> &'static str`, `fn start_bucket(self, now_unix: i64) -> Option<i64>`
  - `pub fn hour_bucket(ts: i64) -> i64`
  - `pub fn record_token_usage(account_id: &str, prompt: i64, cached: i64, output: i64, thinking: i64, aborted: bool)`
  - `pub fn record_token_usage_at(account_id: &str, prompt: i64, cached: i64, output: i64, thinking: i64, aborted: bool, now_unix: i64)`
  - `pub fn clear_pending()`
  - `pub fn flush_token_usage() -> Result<usize, StorageError>`; `pub fn flush_token_usage_logged()`
  - `pub fn fold_removed_account_tx(tx: &rusqlite::Transaction, account_id: &str) -> Result<(), StorageError>`
  - `pub fn usage_by_account(range: TokenRange, now_unix: i64) -> Result<BTreeMap<String, TokenCounts>, StorageError>`
  - `pub fn sum(usage: &BTreeMap<String, TokenCounts>) -> TokenCounts`
  - `pub fn account_label(pool: &crate::storage::Pool, account_id: &str) -> String`

- [ ] **Step 1: Write the module with its tests, implementation stubbed.** Create `src/storage/tokens.rs` with exactly the code from Step 3 **except** make the bodies of `flush_token_usage`, `fold_removed_account_tx` and `usage_by_account` be `todo!()`, then append this test module at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            clear_pending();
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn env() -> Env {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        clear_pending();
        Env { _lock: lock, _dir: dir }
    }

    fn save_accounts(ids: &[&str]) {
        let pool = crate::storage::Pool {
            accounts: ids
                .iter()
                .map(|id| crate::storage::Account {
                    id: id.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();
    }

    const NOW: i64 = 1_800_000_000 + 1234; // some time past an hour boundary

    #[test]
    fn hour_bucket_aligns_down() {
        assert_eq!(hour_bucket(7200), 7200);
        assert_eq!(hour_bucket(7199), 3600);
        assert_eq!(hour_bucket(0), 0);
        assert_eq!(hour_bucket(-1), -3600);
    }

    #[test]
    fn range_windows() {
        let cur = hour_bucket(NOW);
        assert_eq!(TokenRange::Hour.start_bucket(NOW), Some(cur));
        assert_eq!(TokenRange::Day.start_bucket(NOW), Some(cur - 23 * 3600));
        assert_eq!(TokenRange::Week.start_bucket(NOW), Some(cur - 167 * 3600));
        assert_eq!(TokenRange::Total.start_bucket(NOW), None);
        for (s, r) in [
            ("1h", TokenRange::Hour),
            ("24h", TokenRange::Day),
            ("7d", TokenRange::Week),
            ("total", TokenRange::Total),
        ] {
            assert_eq!(TokenRange::parse(s), Some(r));
            assert_eq!(r.as_str(), s);
        }
        assert_eq!(TokenRange::parse("2d"), None);
        assert_eq!(TokenRange::parse(""), None);
    }

    #[test]
    fn record_clamps_negative_counts() {
        let _e = env();
        save_accounts(&["acc_1"]);
        // -1 is UsageTap's "cached not reported"; a sum must never go negative.
        record_token_usage_at("acc_1", 100, -1, 20, -5, false, NOW);
        let usage = usage_by_account(TokenRange::Hour, NOW).unwrap();
        let c = usage["acc_1"];
        assert_eq!(
            c,
            TokenCounts {
                prompt: 100,
                cached: 0,
                output: 20,
                thinking: 0,
                requests: 1,
                aborted: 0
            }
        );
        assert_eq!(c.total(), 120, "cached is part of prompt, not added to the total");
    }

    #[test]
    fn flush_aggregates_per_hour_and_account() {
        let _e = env();
        save_accounts(&["acc_1", "acc_2"]);
        record_token_usage_at("acc_1", 1000, 400, 200, 100, false, NOW);
        record_token_usage_at("acc_1", 10, 0, 1, 0, true, NOW + 5);
        record_token_usage_at("acc_2", 50, 0, 5, 0, false, NOW);
        assert_eq!(flush_token_usage().unwrap(), 2, "two (hour, account) keys");
        assert_eq!(flush_token_usage().unwrap(), 0, "pending map is empty after a flush");

        let rows: i64 = StateDb::global()
            .unwrap()
            .view(|tx| Ok(tx.query_row("SELECT COUNT(*) FROM token_usage_hourly", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(rows, 2);

        let usage = usage_by_account(TokenRange::Hour, NOW).unwrap();
        assert_eq!(
            usage["acc_1"],
            TokenCounts {
                prompt: 1010,
                cached: 400,
                output: 201,
                thinking: 100,
                requests: 2,
                aborted: 1
            }
        );
        assert_eq!(usage["acc_2"].requests, 1);
        assert_eq!(sum(&usage).total(), 1010 + 201 + 100 + 50 + 5);

        // A second flush into the same hour adds to the row instead of replacing it.
        record_token_usage_at("acc_2", 50, 0, 5, 0, false, NOW + 10);
        flush_token_usage().unwrap();
        assert_eq!(usage_by_account(TokenRange::Hour, NOW).unwrap()["acc_2"].prompt, 100);
    }

    #[test]
    fn unflushed_usage_is_visible_to_readers() {
        let _e = env();
        save_accounts(&["acc_1"]);
        record_token_usage_at("acc_1", 7, 0, 3, 0, false, NOW);
        flush_token_usage().unwrap();
        record_token_usage_at("acc_1", 1, 0, 1, 0, false, NOW);
        let usage = usage_by_account(TokenRange::Total, NOW).unwrap();
        assert_eq!(usage["acc_1"].prompt, 8);
        assert_eq!(usage["acc_1"].requests, 2);
    }

    #[test]
    fn ranges_select_hour_buckets() {
        let _e = env();
        save_accounts(&["acc_1"]);
        record_token_usage_at("acc_1", 1, 0, 0, 0, false, NOW); // this hour
        record_token_usage_at("acc_1", 10, 0, 0, 0, false, NOW - 2 * 3600); // 2h ago
        record_token_usage_at("acc_1", 100, 0, 0, 0, false, NOW - 30 * 3600); // 30h ago
        record_token_usage_at("acc_1", 1000, 0, 0, 0, false, NOW - 200 * 3600); // ~8 days ago
        flush_token_usage().unwrap();
        let prompt = |r| usage_by_account(r, NOW).unwrap().get("acc_1").map(|c| c.prompt).unwrap_or(0);
        assert_eq!(prompt(TokenRange::Hour), 1);
        assert_eq!(prompt(TokenRange::Day), 11);
        assert_eq!(prompt(TokenRange::Week), 111);
        assert_eq!(prompt(TokenRange::Total), 1111);
    }

    #[test]
    fn removed_account_usage_moves_to_removed_row() {
        let _e = env();
        save_accounts(&["acc_1", "acc_2"]);
        record_token_usage_at("acc_1", 100, 0, 10, 0, false, NOW);
        record_token_usage_at("acc_2", 200, 0, 20, 0, false, NOW);
        record_token_usage_at("acc_2", 300, 0, 30, 0, false, NOW - 5 * 3600);
        flush_token_usage().unwrap();
        let before = sum(&usage_by_account(TokenRange::Total, NOW).unwrap());

        // Remove acc_2, then add a new account that reuses the id.
        save_accounts(&["acc_1"]);
        let usage = usage_by_account(TokenRange::Total, NOW).unwrap();
        assert!(!usage.contains_key("acc_2"));
        assert_eq!(usage[REMOVED_ACCOUNT_ID].prompt, 500);
        assert_eq!(sum(&usage), before, "pool totals survive the removal");

        save_accounts(&["acc_1", "acc_2"]);
        assert!(!usage_by_account(TokenRange::Total, NOW).unwrap().contains_key("acc_2"));

        // Removing a second account merges into the same _removed rows.
        record_token_usage_at("acc_2", 1, 0, 0, 0, false, NOW);
        flush_token_usage().unwrap();
        save_accounts(&["acc_1"]);
        assert_eq!(
            usage_by_account(TokenRange::Total, NOW).unwrap()[REMOVED_ACCOUNT_ID].prompt,
            501
        );
    }

    #[test]
    fn flush_for_unknown_account_goes_to_removed() {
        let _e = env();
        save_accounts(&["acc_1"]);
        record_token_usage_at("acc_9", 42, 0, 0, 0, false, NOW);
        flush_token_usage().unwrap();
        let usage = usage_by_account(TokenRange::Total, NOW).unwrap();
        assert_eq!(usage[REMOVED_ACCOUNT_ID].prompt, 42);
        assert!(!usage.contains_key("acc_9"));
    }

    #[test]
    fn empty_account_id_is_ignored() {
        let _e = env();
        record_token_usage_at("", 42, 0, 0, 0, false, NOW);
        assert_eq!(flush_token_usage().unwrap(), 0);
    }

    #[test]
    fn account_label_prefers_name() {
        let pool = crate::storage::Pool {
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    name: "Work".to_string(),
                    email: "w@example.com".to_string(),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    email: "x@example.com".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(account_label(&pool, "acc_1"), "Work");
        assert_eq!(account_label(&pool, "acc_2"), "acc_2", "never the email");
        assert_eq!(account_label(&pool, REMOVED_ACCOUNT_ID), "(removed accounts)");
        assert_eq!(account_label(&pool, "acc_7"), "acc_7");
    }
}
```

Also add `pub mod tokens;` to `src/storage/mod.rs`.

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --lib storage::tokens`
Expected: `hour_bucket_aligns_down`, `range_windows`, `empty_account_id_is_ignored` and `account_label_prefers_name` pass; the others FAIL (panic `not yet implemented`).

- [ ] **Step 3: Implement.** The full module (replace the `todo!()` bodies with this code; keep the test module from Step 1 at the end):

```rust
//! Hourly token usage per account (`token_usage_hourly` in state.db).
//!
//! The proxy adds each finished generation attempt to an in-memory map (no I/O on the request path);
//! the daemon flushes the map every FLUSH_INTERVAL in one transaction, so token accounting never
//! competes with account selection for the state.db write lock.

use super::{StateDb, StorageError};
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

pub const TOKEN_USAGE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS token_usage_hourly (
	hour_bucket INTEGER NOT NULL,
	account_id TEXT NOT NULL,
	prompt_tokens INTEGER NOT NULL DEFAULT 0,
	cached_tokens INTEGER NOT NULL DEFAULT 0,
	output_tokens INTEGER NOT NULL DEFAULT 0,
	thinking_tokens INTEGER NOT NULL DEFAULT 0,
	request_count INTEGER NOT NULL DEFAULT 0,
	aborted_count INTEGER NOT NULL DEFAULT 0,
	PRIMARY KEY (hour_bucket, account_id)
) WITHOUT ROWID;"#;

/// Usage of removed accounts is kept under this id, because account ids are reused (`next_account_id`).
pub const REMOVED_ACCOUNT_ID: &str = "_removed";

/// How often the daemon writes pending usage to state.db.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

const UPSERT_SQL: &str = "INSERT INTO token_usage_hourly
	(hour_bucket, account_id, prompt_tokens, cached_tokens, output_tokens, thinking_tokens, request_count, aborted_count)
	VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
	ON CONFLICT(hour_bucket, account_id) DO UPDATE SET
		prompt_tokens = prompt_tokens + excluded.prompt_tokens,
		cached_tokens = cached_tokens + excluded.cached_tokens,
		output_tokens = output_tokens + excluded.output_tokens,
		thinking_tokens = thinking_tokens + excluded.thinking_tokens,
		request_count = request_count + excluded.request_count,
		aborted_count = aborted_count + excluded.aborted_count";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenCounts {
    pub prompt: i64,
    /// The part of `prompt` served from the context cache (already included in `prompt`).
    pub cached: i64,
    pub output: i64,
    pub thinking: i64,
    pub requests: i64,
    /// Streams that ended early; their usage is what was seen before the end.
    pub aborted: i64,
}

impl TokenCounts {
    /// Prompt + output + thinking. Cached tokens are inside prompt and are not added again.
    pub fn total(&self) -> i64 {
        self.prompt + self.output + self.thinking
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.prompt += other.prompt;
        self.cached += other.cached;
        self.output += other.output;
        self.thinking += other.thinking;
        self.requests += other.requests;
        self.aborted += other.aborted;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRange {
    Hour,
    Day,
    Week,
    Total,
}

impl TokenRange {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1h" => Some(TokenRange::Hour),
            "24h" => Some(TokenRange::Day),
            "7d" => Some(TokenRange::Week),
            "total" => Some(TokenRange::Total),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TokenRange::Hour => "1h",
            TokenRange::Day => "24h",
            TokenRange::Week => "7d",
            TokenRange::Total => "total",
        }
    }

    /// First hour bucket inside the range: 1h is the current clock hour; 24h and 7d are the current
    /// hour plus the 23 / 167 before it; total has no start.
    pub fn start_bucket(self, now_unix: i64) -> Option<i64> {
        let cur = hour_bucket(now_unix);
        match self {
            TokenRange::Hour => Some(cur),
            TokenRange::Day => Some(cur - 23 * 3600),
            TokenRange::Week => Some(cur - 167 * 3600),
            TokenRange::Total => None,
        }
    }
}

/// Start of the UTC hour containing `ts`.
pub fn hour_bucket(ts: i64) -> i64 {
    ts - ts.rem_euclid(3600)
}

type PendingMap = HashMap<(i64, String), TokenCounts>;

static PENDING: LazyLock<Mutex<PendingMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending() -> MutexGuard<'static, PendingMap> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds one finished generation attempt to the pending usage. Never touches the database.
pub fn record_token_usage(account_id: &str, prompt: i64, cached: i64, output: i64, thinking: i64, aborted: bool) {
    let now = crate::clock::now_unix() as i64;
    record_token_usage_at(account_id, prompt, cached, output, thinking, aborted, now);
}

pub fn record_token_usage_at(
    account_id: &str,
    prompt: i64,
    cached: i64,
    output: i64,
    thinking: i64,
    aborted: bool,
    now_unix: i64,
) {
    if account_id.is_empty() {
        return;
    }
    // UsageTap reports "not seen" as -1 (cached) or 0; never let that make a sum negative.
    let delta = TokenCounts {
        prompt: prompt.max(0),
        cached: cached.max(0),
        output: output.max(0),
        thinking: thinking.max(0),
        requests: 1,
        aborted: i64::from(aborted),
    };
    pending()
        .entry((hour_bucket(now_unix), account_id.to_string()))
        .or_default()
        .add(&delta);
}

/// Drops pending usage (tests).
pub fn clear_pending() {
    pending().clear();
}

/// Writes pending usage to state.db in one transaction and returns how many (hour, account) rows it
/// touched. On failure the usage is put back for the next flush.
pub fn flush_token_usage() -> Result<usize, StorageError> {
    let batch = std::mem::take(&mut *pending());
    if batch.is_empty() {
        return Ok(0);
    }
    let res = StateDb::global().and_then(|db| {
        db.update(|tx| {
            let mut exists = tx.prepare("SELECT 1 FROM accounts WHERE id = ?1")?;
            let mut upsert = tx.prepare(UPSERT_SQL)?;
            for ((bucket, id), c) in &batch {
                // The account was removed after the request finished: keep the usage with the other removed accounts.
                let id = if exists.exists([id.as_str()])? {
                    id.as_str()
                } else {
                    REMOVED_ACCOUNT_ID
                };
                upsert.execute(rusqlite::params![
                    bucket, id, c.prompt, c.cached, c.output, c.thinking, c.requests, c.aborted
                ])?;
            }
            Ok(())
        })
    });
    match res {
        Ok(()) => Ok(batch.len()),
        Err(e) => {
            let mut p = pending();
            for (key, c) in batch {
                p.entry(key).or_default().add(&c);
            }
            Err(e)
        }
    }
}

/// flush_token_usage for the daemon's flush task: failures are logged, the usage is retried later.
pub fn flush_token_usage_logged() {
    if let Err(e) = flush_token_usage() {
        crate::observability::log_message_level(
            crate::observability::LogLevel::Warn,
            "TOKENS",
            &format!("failed to persist token usage (kept for the next flush): {e}"),
        );
    }
}

/// Moves a removed account's usage to REMOVED_ACCOUNT_ID inside the removing transaction, so pool
/// totals stay intact and a reused id starts from zero.
pub fn fold_removed_account_tx(tx: &rusqlite::Transaction, account_id: &str) -> Result<(), StorageError> {
    if account_id == REMOVED_ACCOUNT_ID {
        return Ok(());
    }
    // Databases created by tests or older code paths may not have the table yet.
    tx.execute_batch(TOKEN_USAGE_DDL)?;
    tx.execute(
        "INSERT INTO token_usage_hourly
			(hour_bucket, account_id, prompt_tokens, cached_tokens, output_tokens, thinking_tokens, request_count, aborted_count)
		SELECT hour_bucket, ?2, prompt_tokens, cached_tokens, output_tokens, thinking_tokens, request_count, aborted_count
		FROM token_usage_hourly WHERE account_id = ?1
		ON CONFLICT(hour_bucket, account_id) DO UPDATE SET
			prompt_tokens = prompt_tokens + excluded.prompt_tokens,
			cached_tokens = cached_tokens + excluded.cached_tokens,
			output_tokens = output_tokens + excluded.output_tokens,
			thinking_tokens = thinking_tokens + excluded.thinking_tokens,
			request_count = request_count + excluded.request_count,
			aborted_count = aborted_count + excluded.aborted_count",
        rusqlite::params![account_id, REMOVED_ACCOUNT_ID],
    )?;
    tx.execute("DELETE FROM token_usage_hourly WHERE account_id = ?1", [account_id])?;
    Ok(())
}

/// Usage per account id within `range`, including usage this process has not flushed yet.
pub fn usage_by_account(range: TokenRange, now_unix: i64) -> Result<BTreeMap<String, TokenCounts>, StorageError> {
    let start = range.start_bucket(now_unix).unwrap_or(i64::MIN);
    let mut out = StateDb::global()?.view(|tx| {
        let mut stmt = tx.prepare(
            "SELECT account_id, SUM(prompt_tokens), SUM(cached_tokens), SUM(output_tokens), SUM(thinking_tokens),
				SUM(request_count), SUM(aborted_count)
			FROM token_usage_hourly WHERE hour_bucket >= ?1 GROUP BY account_id",
        )?;
        let rows = stmt.query_map([start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TokenCounts {
                    prompt: r.get(1)?,
                    cached: r.get(2)?,
                    output: r.get(3)?,
                    thinking: r.get(4)?,
                    requests: r.get(5)?,
                    aborted: r.get(6)?,
                },
            ))
        })?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (id, counts) = row?;
            map.insert(id, counts);
        }
        Ok(map)
    })?;
    for ((bucket, id), c) in pending().iter() {
        if *bucket >= start {
            out.entry(id.clone()).or_default().add(c);
        }
    }
    Ok(out)
}

/// Pool-wide total of a usage map.
pub fn sum(usage: &BTreeMap<String, TokenCounts>) -> TokenCounts {
    let mut total = TokenCounts::default();
    for c in usage.values() {
        total.add(c);
    }
    total
}

/// Display name for a usage row: the account's name, else its id (never the email).
pub fn account_label(pool: &crate::storage::Pool, account_id: &str) -> String {
    if account_id == REMOVED_ACCOUNT_ID {
        return "(removed accounts)".to_string();
    }
    pool.accounts
        .iter()
        .find(|a| a.id == account_id && !a.name.is_empty())
        .map(|a| a.name.clone())
        .unwrap_or_else(|| account_id.to_string())
}
```

In `src/storage/db.rs`, `StateDb::open`, directly after the existing line `test_conn.execute_batch(INFLIGHT_LEASE_DDL)?;` add:

```rust
        test_conn.execute_batch(super::tokens::TOKEN_USAGE_DDL)?;
```

In `src/storage/pool.rs`, `save_pool_to_tx`, replace

```rust
            tx.execute("DELETE FROM accounts WHERE id = ?1", [&existing_id])?;
```

with

```rust
            super::tokens::fold_removed_account_tx(tx, &existing_id)?;
            tx.execute("DELETE FROM accounts WHERE id = ?1", [&existing_id])?;
```

If `LogLevel::Warn` does not exist, use the warning-level variant `crate::observability::LogLevel` actually has (list it in your report).

- [ ] **Step 4: Run the tests**

Run: `timeout 900 scripts/test.sh test --lib storage::`
Expected: all pass, including the 10 tests in `storage::tokens::tests`.

- [ ] **Step 5: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
touch src/storage/tokens.rs && timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/storage/tokens.rs src/storage/mod.rs src/storage/db.rs src/storage/pool.rs
git commit -m "feat(storage): hourly token usage table with batched flush

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Record usage on the proxy path and flush from the daemon

**Files:**
- Modify: `src/proxy/body.rs` (`StreamBody::emit_audit`)
- Modify: `src/proxy/dispatch.rs` (buffered 2xx path, right after `let tokens = Tokens { … };`)
- Modify: `src/daemon/server.rs` (flush task after the runtime stats writer; final flush in teardown)
- Create: `src/proxy/tests/tokens.rs`; Modify: `src/proxy/tests/mod.rs` (add `pub mod tokens;`)

**Interfaces:**
- Consumes (Task 1): `crate::storage::tokens::{record_token_usage, flush_token_usage, flush_token_usage_logged, usage_by_account, clear_pending, TokenRange, TokenCounts, FLUSH_INTERVAL}`
- Produces: nothing new.

- [ ] **Step 1: Write the failing proxy tests.** Create `src/proxy/tests/tokens.rs`:

```rust
use std::sync::Arc;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::tokens::{TokenCounts, TokenRange, clear_pending, flush_token_usage, usage_by_account};
use crate::storage::{Account, Pool};
use crate::testutil::TestServer;

fn save_two_accounts() {
    let pool = Pool {
        accounts: vec![
            Account {
                id: "acc_1".to_string(),
                email: "1@example.com".to_string(),
                ..Default::default()
            },
            Account {
                id: "acc_2".to_string(),
                email: "2@example.com".to_string(),
                ..Default::default()
            },
        ],
        active_account_id: Some("acc_1".to_string()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

fn usage_now() -> std::collections::BTreeMap<String, TokenCounts> {
    flush_token_usage().unwrap();
    usage_by_account(TokenRange::Total, crate::clock::now_unix() as i64).unwrap()
}

#[tokio::test]
async fn test_generation_usage_recorded_once_for_successful_attempt_only() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    // acc_1 is rate limited and fails over; acc_2 answers with usage.
    let server = TestServer::start(move |req| {
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        if auth.contains("token-acc_1") {
            (
                429,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"error":{"code":429,"message":"rate limit","status":"RESOURCE_EXHAUSTED"}}"#.to_vec(),
            )
        } else {
            (
                200,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"candidates":[],"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":80,"candidatesTokenCount":45,"thoughtsTokenCount":30}}"#.to_vec(),
            )
        }
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1/models/gemini-2.5-pro:generateContent",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let usage = usage_now();
    assert!(!usage.contains_key("acc_1"), "a failed attempt records no usage: {usage:?}");
    assert_eq!(
        usage["acc_2"],
        TokenCounts {
            prompt: 120,
            cached: 80,
            output: 45,
            thinking: 30,
            requests: 1,
            aborted: 0
        }
    );
    clear_pending();
}

#[tokio::test]
async fn test_streamed_generation_usage_uses_last_usage_metadata() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    // Gemini streams cumulative usageMetadata; the last value is the total.
    let server = TestServer::start(move |_req| {
        (
            200,
            vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            concat!(
                "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":500,\"candidatesTokenCount\":10}}\n\n",
                "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":500,\"candidatesTokenCount\":90,\"thoughtsTokenCount\":40}}\n\n"
            )
            .as_bytes()
            .to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let usage = usage_now();
    let total = crate::storage::tokens::sum(&usage);
    assert_eq!(total.requests, 1, "recorded exactly once: {usage:?}");
    assert_eq!(total.prompt, 500);
    assert_eq!(total.output, 90);
    assert_eq!(total.thinking, 40);
    assert_eq!(total.cached, 0, "cached not reported (-1) is stored as 0");
    assert_eq!(total.aborted, 0);
    clear_pending();
}

#[tokio::test]
async fn test_non_generation_requests_record_no_usage() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    let server = TestServer::start(move |_req| {
        (
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            br#"{"usageMetadata":{"promptTokenCount":999}}"#.to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1internal:loadCodeAssist",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(usage_now().is_empty());
    clear_pending();
}
```

Add `pub mod tokens;` to `src/proxy/tests/mod.rs` next to the other `pub mod` lines.

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --lib proxy::tests::tokens`
Expected: `test_generation_usage_recorded_once_for_successful_attempt_only` and `test_streamed_generation_usage_uses_last_usage_metadata` FAIL (no usage recorded); `test_non_generation_requests_record_no_usage` passes. If a test fails for another reason (e.g. the mock response is not routed as expected), STOP and report the output.

- [ ] **Step 3: Implement the hooks.**

In `src/proxy/body.rs`, at the **start** of `fn emit_audit(&self, reason: &str, total: Duration, record_latency: bool) {` add:

```rust
        // Runs exactly once per stream: on completion (reason "") or when it ended early.
        if self.params.is_generation && self.params.status < 400 {
            crate::storage::tokens::record_token_usage(
                &self.params.account_id,
                self.usage.prompt,
                self.usage.cached,
                self.usage.output,
                self.usage.thinking,
                !reason.is_empty(),
            );
        }
```

In `src/proxy/dispatch.rs`, in the buffered 2xx path, directly after the statement `let tokens = Tokens { prompt: usage.prompt, cached: usage.cached, output: usage.output, thinking: usage.thinking, };` add:

```rust
    if params.is_generation {
        crate::storage::tokens::record_token_usage(
            &params.acc.id,
            tokens.prompt,
            tokens.cached,
            tokens.output,
            tokens.thinking,
            false,
        );
    }
```

In `src/daemon/server.rs`, directly after the block that starts the runtime stats writer and sets `set_on_in_flight_change` (the comment `// 5. Periodic runtime stats writer with bounded coalescing` … `})));`), add:

```rust
    // 5b. Token usage: write the in-memory hourly counters to state.db off the request path.
    let token_flush_task = tokio::spawn(async {
        let mut ticker = tokio::time::interval(crate::storage::tokens::FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let _ = tokio::task::spawn_blocking(crate::storage::tokens::flush_token_usage_logged).await;
        }
    });
```

and in the teardown, directly after `stats_writer.stop().await;`, add:

```rust
    token_flush_task.abort();
    let _ = token_flush_task.await;
    let _ = tokio::task::spawn_blocking(crate::storage::tokens::flush_token_usage_logged).await;
```

- [ ] **Step 4: Run the tests**

Run: `timeout 900 scripts/test.sh test --lib proxy::` then `timeout 900 scripts/test.sh test --lib daemon::`
Expected: all pass, including the 3 new tests.

- [ ] **Step 5: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
touch src/proxy/body.rs && timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/proxy/body.rs src/proxy/dispatch.rs src/daemon/server.rs src/proxy/tests/tokens.rs src/proxy/tests/mod.rs
git commit -m "feat(proxy): record token usage per generation attempt and flush it from the daemon

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: `GET /api/v1/tokens` and the dashboard panel

**Files:**
- Modify: `src/dashboard/dto.rs` (new DTOs + `build_tokens_response`)
- Modify: `src/dashboard/mod.rs` (route + handler)
- Modify: `src/dashboard/assets/index.html` (CSS, panel markup, JS)
- Test: `tests/dashboard.rs`

**Interfaces:**
- Consumes (Task 1): `crate::storage::tokens::{usage_by_account, sum, account_label, TokenCounts, TokenRange}`
- Produces: `pub fn build_tokens_response(range: TokenRange, now_unix: i64) -> Result<TokensResponse, StorageError>` in `crate::dashboard::dto`; JSON shape below.

- [ ] **Step 1: Write the failing tests.** Append to `tests/dashboard.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_tokens_endpoint() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    agy_pool::storage::tokens::clear_pending();
    let now = agy_pool::clock::now_unix() as i64;
    agy_pool::storage::tokens::record_token_usage_at("acc_1", 1000, 400, 200, 100, false, now);
    // cached not reported (-1) and an aborted stream
    agy_pool::storage::tokens::record_token_usage_at("acc_2", 500, -1, 100, 0, true, now);
    // three days ago: only in 7d and total
    agy_pool::storage::tokens::record_token_usage_at("acc_1", 7000, 0, 0, 0, false, now - 3 * 86400);
    agy_pool::storage::tokens::flush_token_usage().unwrap();

    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/tokens", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "reads need the token");

    let auth = bearer(TEST_TOKEN);
    let get = |path: &'static str| {
        let headers = hdrs(&[("authorization", auth.as_str())]);
        async move { send_request(addr, Method::GET, path, headers, Bytes::new()).await }
    };

    // Default range is 24h.
    let (status, headers, body) = get("/api/v1/tokens").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["range"], "24h");
    assert!(json["window_start"].as_i64().unwrap() <= now);
    let summary = &json["summary"];
    assert_eq!(summary["prompt_tokens"], 1500);
    assert_eq!(summary["cached_tokens"], 400);
    assert_eq!(summary["output_tokens"], 300);
    assert_eq!(summary["thinking_tokens"], 100);
    assert_eq!(summary["total_tokens"], 1900, "cached is part of prompt");
    assert_eq!(summary["request_count"], 2);
    assert_eq!(summary["aborted_count"], 1);
    let accounts = json["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["account_id"], "acc_1", "sorted by total, largest first");
    assert_eq!(accounts[0]["total_tokens"], 1300);
    assert!((accounts[0]["pool_share"].as_f64().unwrap() - 1300.0 / 1900.0).abs() < 1e-9);
    assert_eq!(accounts[1]["cached_tokens"], 0);
    assert!(
        !String::from_utf8_lossy(&body).contains("@example.com"),
        "never exposes emails"
    );

    let (_, _, body) = get("/api/v1/tokens?range=7d").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["summary"]["prompt_tokens"], 8500);

    let (_, _, body) = get("/api/v1/tokens?range=total").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["window_start"].is_null());

    let (status, _, body) = get("/api/v1/tokens?range=2d").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "INVALID_ARGUMENT");

    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/tokens",
        hdrs(&[("authorization", auth.as_str())]),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    agy_pool::storage::tokens::clear_pending();
    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

and in `test_embedded_spa_asset_constraints`, directly after the existing line
`assert!(body_str.contains("/api/v1/events"), "missing /api/v1/events reference");` add:

```rust
    assert!(body_str.contains("/api/v1/tokens"), "missing /api/v1/tokens reference");
    assert!(body_str.contains("token-usage-body"), "missing token usage table");
    assert!(body_str.contains("data-token-range"), "missing token range buttons");
```

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: `test_tokens_endpoint` FAILS (404 instead of 401/200) and `test_embedded_spa_asset_constraints` FAILS (`missing /api/v1/tokens reference`); the rest pass.

- [ ] **Step 3: Implement the endpoint.** Append to `src/dashboard/dto.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenTotalsDto {
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    /// Part of prompt_tokens; not added to total_tokens.
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub thinking_tokens: i64,
    pub request_count: i64,
    pub aborted_count: i64,
}

impl From<&crate::storage::tokens::TokenCounts> for TokenTotalsDto {
    fn from(c: &crate::storage::tokens::TokenCounts) -> Self {
        TokenTotalsDto {
            total_tokens: c.total(),
            prompt_tokens: c.prompt,
            cached_tokens: c.cached,
            output_tokens: c.output,
            thinking_tokens: c.thinking,
            request_count: c.requests,
            aborted_count: c.aborted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAccountDto {
    pub account_id: String,
    pub name: String,
    #[serde(flatten)]
    pub totals: TokenTotalsDto,
    /// This account's share of the pool's total_tokens; null when the pool used nothing.
    pub pool_share: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokensResponse {
    pub range: String,
    /// Unix start of the first hour bucket in the range; null for "total".
    pub window_start: Option<i64>,
    pub summary: TokenTotalsDto,
    pub accounts: Vec<TokenAccountDto>,
}

pub fn build_tokens_response(
    range: crate::storage::tokens::TokenRange,
    now_unix: i64,
) -> Result<TokensResponse, crate::storage::StorageError> {
    let usage = crate::storage::tokens::usage_by_account(range, now_unix)?;
    let pool = crate::storage::load_pool().unwrap_or_default();
    let summary = crate::storage::tokens::sum(&usage);
    let pool_total = summary.total();
    let mut accounts: Vec<TokenAccountDto> = usage
        .iter()
        .map(|(id, c)| TokenAccountDto {
            account_id: id.clone(),
            name: crate::storage::tokens::account_label(&pool, id),
            totals: c.into(),
            pool_share: (pool_total > 0).then(|| c.total() as f64 / pool_total as f64),
        })
        .collect();
    accounts.sort_by(|a, b| {
        b.totals
            .total_tokens
            .cmp(&a.totals.total_tokens)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    Ok(TokensResponse {
        range: range.as_str().to_string(),
        window_start: range.start_bucket(now_unix),
        summary: (&summary).into(),
        accounts,
    })
}
```

In `src/dashboard/mod.rs`, add the handler next to `forecast_response`:

```rust
async fn tokens_response(query: Option<&str>) -> Response<ProxyBody> {
    let raw = query
        .and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "range")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_else(|| "24h".to_string());
    let Some(range) = crate::storage::tokens::TokenRange::parse(&raw) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "range must be one of 1h, 24h, 7d, total",
        );
    };
    let now = crate::clock::now_unix() as i64;
    let Ok(result) = tokio::task::spawn_blocking(move || dto::build_tokens_response(range, now)).await else {
        return internal_error();
    };
    match result {
        Ok(body) => json_response(serde_json::to_vec(&body).unwrap_or_default()),
        Err(e) => {
            crate::observability::log_message_level(
                crate::observability::LogLevel::Error,
                "DASHBOARD",
                &format!("failed to read token usage: {e}"),
            );
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORAGE_ERROR",
                "could not read token usage; see the gateway log",
            )
        }
    }
}
```

Routing in `serve`:
1. In the `if !is_get` arm that lists `"/dashboard" | "/api/v1/auth/status" | "/api/v1/status" | "/api/v1/accounts" | "/api/v1/forecast" | "/api/v1/events"`, add `| "/api/v1/tokens"`.
2. Change the read arm `"/api/v1/status" | "/api/v1/accounts" | "/api/v1/forecast" | "/api/v1/events" => {` to also match `| "/api/v1/tokens"`, and inside its inner `match route.as_str()` add the arm `"/api/v1/tokens" => tokens_response(req.uri().query()).await,` before the `_ => events_response(),` arm.

- [ ] **Step 4: Implement the panel** in `src/dashboard/assets/index.html`.

CSS — insert immediately before `  </style>`:

```css
    button.btn.active {
      color: var(--accent-text);
      border-color: var(--accent-border);
      background-color: var(--accent-soft);
    }
    .token-summary {
      display: flex;
      flex-wrap: wrap;
      gap: 18px;
      padding: 10px 16px 4px;
      font-size: 12px;
      color: var(--text-muted);
    }
    .token-summary strong {
      color: var(--text-main);
      font-size: 15px;
    }
    .token-bar {
      display: flex;
      height: 8px;
      margin: 8px 16px 12px;
      border-radius: 4px;
      overflow: hidden;
      background-color: var(--border-subtle);
    }
    .token-bar span { height: 100%; }
    .token-bar .seg-prompt { background-color: #6366f1; }
    .token-bar .seg-cached { background-color: #a5b4fc; }
    .token-bar .seg-output { background-color: var(--accent); }
    .token-bar .seg-thinking { background-color: #f59e0b; }
    .token-legend i {
      display: inline-block;
      width: 8px;
      height: 8px;
      border-radius: 2px;
      margin-right: 4px;
    }
```

Markup — insert immediately before the line `    <!-- Live Request Stream (Bottom Panel) -->`:

```html
    <!-- Token Usage -->
    <div class="section-panel">
      <div class="section-header">
        <div class="section-title">
          <span>Token Usage</span>
          <span class="pill pill-mono" id="token-window">—</span>
        </div>
        <div class="btn-group">
          <button type="button" class="btn" data-token-range="1h" title="Since the start of the current hour">This hour</button>
          <button type="button" class="btn active" data-token-range="24h">24h</button>
          <button type="button" class="btn" data-token-range="7d">7d</button>
          <button type="button" class="btn" data-token-range="total">All time</button>
        </div>
      </div>
      <div class="token-summary" id="token-summary">Loading token usage...</div>
      <div class="token-bar" id="token-bar"></div>
      <div class="table-container">
        <table>
          <thead>
            <tr>
              <th>Account</th>
              <th>Requests</th>
              <th>Prompt</th>
              <th>Cached</th>
              <th>Output</th>
              <th>Thinking</th>
              <th>Total</th>
              <th>Share</th>
            </tr>
          </thead>
          <tbody id="token-usage-body">
            <tr><td colspan="8" class="empty-state">Loading token usage...</td></tr>
          </tbody>
        </table>
      </div>
    </div>

```

JS — insert directly after the closing `}` of `async function fetchForecast() { … }`:

```js
    let tokenRange = '24h';

    function fmtTokens(n) {
      n = Number(n) || 0;
      if (n < 1000) return String(n);
      if (n < 1e6) return (n / 1e3).toFixed(1) + 'K';
      if (n < 1e9) return (n / 1e6).toFixed(2) + 'M';
      return (n / 1e9).toFixed(2) + 'B';
    }

    async function fetchTokens() {
      try {
        const res = await agyFetch('/api/v1/tokens?range=' + encodeURIComponent(tokenRange));
        if (!res.ok) return;
        renderTokens(await res.json());
      } catch (err) {
        console.error('Failed to fetch token usage:', err);
      }
    }

    function renderTokens(data) {
      const s = data.summary || {};
      const total = Number(s.total_tokens) || 0;
      document.getElementById('token-window').textContent = data.window_start
        ? 'since ' + new Date(data.window_start * 1000).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
        : 'all time';

      const summary = document.getElementById('token-summary');
      summary.textContent = '';
      const parts = [
        ['Total', total, null],
        ['Prompt', s.prompt_tokens, 'seg-prompt'],
        ['Cached (in prompt)', s.cached_tokens, 'seg-cached'],
        ['Output', s.output_tokens, 'seg-output'],
        ['Thinking', s.thinking_tokens, 'seg-thinking'],
        ['Requests', s.request_count, null],
      ];
      for (const [label, value, seg] of parts) {
        const span = document.createElement('span');
        span.className = 'token-legend';
        if (seg) {
          const swatch = document.createElement('i');
          swatch.className = seg;
          span.appendChild(swatch);
        }
        span.appendChild(document.createTextNode(label + ' '));
        const strong = document.createElement('strong');
        strong.className = 'num';
        strong.textContent = label === 'Requests' ? String(Number(value) || 0) : fmtTokens(value);
        span.appendChild(strong);
        summary.appendChild(span);
      }
      if (s.aborted_count) {
        const note = document.createElement('span');
        note.textContent = s.aborted_count + ' aborted (partial usage)';
        summary.appendChild(note);
      }

      // Cached is a part of prompt: the bar shows uncached prompt + cached + output + thinking = total.
      const bar = document.getElementById('token-bar');
      bar.textContent = '';
      const cached = Number(s.cached_tokens) || 0;
      const segments = [
        ['seg-prompt', (Number(s.prompt_tokens) || 0) - cached],
        ['seg-cached', cached],
        ['seg-output', Number(s.output_tokens) || 0],
        ['seg-thinking', Number(s.thinking_tokens) || 0],
      ];
      if (total > 0) {
        for (const [cls, value] of segments) {
          if (value <= 0) continue;
          const seg = document.createElement('span');
          seg.className = cls;
          seg.style.width = (value * 100 / total).toFixed(2) + '%';
          seg.title = fmtTokens(value);
          bar.appendChild(seg);
        }
      }

      const tbody = document.getElementById('token-usage-body');
      const rows = data.accounts || [];
      if (rows.length === 0) {
        tbody.innerHTML = '<tr><td colspan="8" class="empty-state">No token usage in this range.</td></tr>';
        return;
      }
      tbody.innerHTML = rows.map(a => `
        <tr>
          <td>${escapeHtml(a.name || a.account_id)}</td>
          <td class="num">${Number(a.request_count) || 0}</td>
          <td class="num">${fmtTokens(a.prompt_tokens)}</td>
          <td class="num">${fmtTokens(a.cached_tokens)}</td>
          <td class="num">${fmtTokens(a.output_tokens)}</td>
          <td class="num">${fmtTokens(a.thinking_tokens)}</td>
          <td class="num">${fmtTokens(a.total_tokens)}</td>
          <td class="num">${a.pool_share == null ? '—' : (a.pool_share * 100).toFixed(1) + '%'}</td>
        </tr>`).join('');
    }

    document.querySelectorAll('button[data-token-range]').forEach(btn => {
      btn.addEventListener('click', () => {
        tokenRange = btn.dataset.tokenRange;
        document.querySelectorAll('button[data-token-range]').forEach(b => b.classList.toggle('active', b === btn));
        fetchTokens();
      });
    });
```

In `function start()`, change
`Promise.all([fetchStatus(), fetchAccounts(), fetchForecast(), checkForUpdate()]).catch(console.error);`
to
`Promise.all([fetchStatus(), fetchAccounts(), fetchForecast(), fetchTokens(), checkForUpdate()]).catch(console.error);`
and directly after the 5 s polling `setInterval(() => { … }, 5000);` block add:

```js
      // Token usage changes slowly (the gateway flushes every 10s); poll every 30s.
      setInterval(() => {
        if (!authOk) return;
        fetchTokens();
      }, 30000);
```

- [ ] **Step 5: Syntax-check and run the tests**

```bash
python3 -I -c "import re;s=open('src/dashboard/assets/index.html').read();open('/tmp/agy-tokens.js','w').write(re.findall(r'<script>(.*?)</script>',s,re.S)[-1])" && node --check /tmp/agy-tokens.js && echo JS-OK
timeout 900 scripts/test.sh test --test dashboard
timeout 900 scripts/test.sh test --lib dashboard::
```
Expected: `JS-OK`; all dashboard tests pass.

- [ ] **Step 6: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
touch src/dashboard/dto.rs && timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/dto.rs src/dashboard/mod.rs src/dashboard/assets/index.html tests/dashboard.rs
git commit -m "feat(dashboard): token usage panel and GET /api/v1/tokens

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: `agy-pool tokens`, plus summary lines in `status -v` and `stats`

**Files:**
- Create: `src/cli/tokens.rs`
- Modify: `src/cli/mod.rs` (module, dispatch, help text, usage strings)
- Modify: `src/cli/status.rs`, `src/cli/stats.rs`

**Interfaces:**
- Consumes (Task 1): `crate::storage::tokens::{usage_by_account, sum, account_label, TokenCounts, TokenRange}`
- Produces: `pub async fn tokens_cmd(io: &mut Io<'_>, args: &[String]) -> i32`, `pub fn summary_line(range: TokenRange, now_unix: i64) -> Option<String>`, `pub fn format_tokens(n: i64) -> String` in `crate::cli::tokens`.

- [ ] **Step 1: Write the module with failing tests.** Create `src/cli/tokens.rs` with this code, but with the body of `tokens_cmd` replaced by `todo!()` for now:

```rust
use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_RED, CLR_RESET};
use crate::storage::tokens::{self, TokenCounts, TokenRange};
use std::io::Write;

const USAGE: &str = "usage: agy-pool tokens [--range 1h|24h|7d|total]\n\n\
Show token usage per account and for the whole pool (default range: 24h).\n\
  1h is the current clock hour; 24h and 7d are the current hour plus the 23 / 167 before it.\n\
  Cached tokens are part of prompt tokens; Total = prompt + output + thinking.\n";

/// Formats a token count compactly: 950, 12.3K, 1.85M, 2.10B.
pub fn format_tokens(n: i64) -> String {
    let f = n as f64;
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}K", f / 1e3)
    } else if n < 1_000_000_000 {
        format!("{:.2}M", f / 1e6)
    } else {
        format!("{:.2}B", f / 1e9)
    }
}

fn range_label(range: TokenRange, now_unix: i64) -> String {
    let name = match range {
        TokenRange::Hour => "this hour",
        TokenRange::Day => "last 24h",
        TokenRange::Week => "last 7d",
        TokenRange::Total => return "all time".to_string(),
    };
    let since = range
        .start_bucket(now_unix)
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    format!("{name} (since {since})")
}

fn share(part: &TokenCounts, total: &TokenCounts) -> String {
    if total.total() > 0 {
        format!("{:.1}%", part.total() as f64 * 100.0 / total.total() as f64)
    } else {
        "-".to_string()
    }
}

/// One-line pool summary for `status -v` and `stats`; None when state.db cannot be read.
pub fn summary_line(range: TokenRange, now_unix: i64) -> Option<String> {
    let s = tokens::sum(&tokens::usage_by_account(range, now_unix).ok()?);
    Some(format!(
        "Tokens  : {} {} total · prompt {} (cached {}) · output {} · thinking {} · {} requests",
        range.as_str(),
        format_tokens(s.total()),
        format_tokens(s.prompt),
        format_tokens(s.cached),
        format_tokens(s.output),
        format_tokens(s.thinking),
        crate::observability::format_number(s.requests.max(0) as u64)
    ))
}

pub async fn tokens_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (_ctx, rem) = match crate::cli::resolve_context_for_cmd(args) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let mut range = TokenRange::Day;
    let mut i = 0;
    while i < rem.len() {
        let arg = rem[i].as_str();
        let value = if arg == "-h" || arg == "--help" {
            let _ = write!(io.stdout, "{USAGE}");
            return 0;
        } else if arg == "--range" || arg == "-r" {
            i += 1;
            rem.get(i).map(String::as_str)
        } else if let Some(v) = arg.strip_prefix("--range=") {
            Some(v)
        } else {
            let _ = writeln!(io.stderr, "agy-pool tokens: error: unrecognized argument: {arg}");
            return 2;
        };
        match value.and_then(TokenRange::parse) {
            Some(r) => range = r,
            None => {
                let _ = writeln!(io.stderr, "agy-pool tokens: error: --range must be one of 1h, 24h, 7d, total");
                return 2;
            }
        }
        i += 1;
    }

    let now = crate::clock::now_unix() as i64;
    let usage = match tokens::usage_by_account(range, now) {
        Ok(u) => u,
        Err(e) => {
            let _ = writeln!(io.stderr, "{}[Error] Cannot read token usage: {}{}", CLR_RED, e, CLR_RESET);
            return 1;
        }
    };
    let pool = crate::storage::load_pool().unwrap_or_default();
    let total = tokens::sum(&usage);

    let _ = writeln!(io.stdout, "{}Token usage — {}{}\n", CLR_BOLD, range_label(range, now), CLR_RESET);
    if usage.is_empty() {
        let _ = writeln!(io.stdout, "No token usage recorded in this range.");
        return 0;
    }

    let header = format!(
        "{:<20} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6}",
        "Account", "Requests", "Prompt", "Cached", "Output", "Thinking", "Total", "Share"
    );
    let _ = writeln!(io.stdout, "{header}");
    let mut rows: Vec<(&String, &TokenCounts)> = usage.iter().collect();
    rows.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then_with(|| a.0.cmp(b.0)));
    let mut print_row = |name: &str, c: &TokenCounts| {
        let name: String = name.chars().take(20).collect();
        let _ = writeln!(
            io.stdout,
            "{:<20} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6}",
            name,
            c.requests,
            format_tokens(c.prompt),
            format_tokens(c.cached),
            format_tokens(c.output),
            format_tokens(c.thinking),
            format_tokens(c.total()),
            share(c, &total)
        );
    };
    for (id, c) in rows {
        print_row(&tokens::account_label(&pool, id), c);
    }
    print_row("Pool", &total);
    if total.aborted > 0 {
        let _ = writeln!(
            io.stdout,
            "\n{} aborted stream(s) counted with the usage seen before they ended.",
            total.aborted
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_is_compact() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(1_850_240), "1.85M");
        assert_eq!(format_tokens(2_100_000_000), "2.10B");
    }

    #[tokio::test]
    async fn test_tokens_cmd_table_and_ranges() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::storage::tokens::clear_pending();
        let pool = crate::storage::Pool {
            accounts: vec![crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Work".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();
        let now = crate::clock::now_unix() as i64;
        crate::storage::tokens::record_token_usage_at("acc_1", 12_000, 2_000, 3_000, 1_000, false, now);
        crate::storage::tokens::record_token_usage_at("acc_1", 50_000, 0, 0, 0, false, now - 3 * 86400);
        crate::storage::tokens::flush_token_usage().unwrap();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["tokens"], "").await;
        assert_eq!(code, 0, "stderr: {stderr}");
        assert!(stdout.contains("last 24h"), "{stdout}");
        assert!(stdout.contains("Work"), "{stdout}");
        assert!(stdout.contains("16.0K"), "24h total = 12K + 3K + 1K: {stdout}");
        assert!(stdout.contains("Pool"), "{stdout}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--range", "7d"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("66.0K"), "7d includes the 3-day-old 50K: {stdout}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--range=total"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("all time"), "{stdout}");

        let (code, _, stderr) = crate::cli::test_support::run_cli(&["tokens", "--range", "2d"], "").await;
        assert_eq!(code, 2);
        assert!(stderr.contains("1h, 24h, 7d, total"), "{stderr}");

        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "--help"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("usage: agy-pool tokens"));

        let line = summary_line(TokenRange::Day, now).unwrap();
        assert!(line.contains("16.0K total"), "{line}");
        assert!(line.contains("1 requests"), "{line}");
        crate::storage::tokens::clear_pending();
    }

    #[tokio::test]
    async fn test_tokens_cmd_empty_range() {
        let _env = crate::cli::test_support::cli_test_env();
        crate::storage::tokens::clear_pending();
        let (code, stdout, _) = crate::cli::test_support::run_cli(&["tokens", "-r", "1h"], "").await;
        assert_eq!(code, 0);
        assert!(stdout.contains("No token usage recorded"), "{stdout}");
    }
}
```

In `src/cli/mod.rs`: add `pub mod tokens;` (alphabetically after `pub mod term;`), and in `main`'s `match cmd.as_str()` add, next to the `"stats" | "audit" | "effectiveness"` arm:

```rust
        "tokens" => tokens::tokens_cmd(io, &args[1..]).await,
```

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --lib cli::tokens`
Expected: `format_tokens_is_compact` passes; `test_tokens_cmd_table_and_ranges` and `test_tokens_cmd_empty_range` FAIL (`not yet implemented`).

- [ ] **Step 3: Implement.**
1. Restore the full `tokens_cmd` body from Step 1.
2. `src/cli/mod.rs` help text: in `print_help`, add after the `agy-pool stats [--json]` example line:
   `"  agy-pool tokens [-r 24h]  Show token usage per account (ranges: 1h, 24h, 7d, total)\n",`
   and in **both** usage strings (the first line of `print_help` and the `invalid choice` message in `main`) insert `tokens,` directly after `stats,`.
3. `src/cli/status.rs`: directly after the `writeln!` that prints `"State   : {}  (schema v{})"`, add:

```rust
    if verbose
        && let Some(line) =
            crate::cli::tokens::summary_line(crate::storage::tokens::TokenRange::Day, crate::clock::now_unix() as i64)
    {
        let _ = writeln!(io.stdout, "{line}");
    }
```

4. `src/cli/stats.rs`: in `stats_cmd`, replace

```rust
            } else {
                render_terminal_report(io.stdout, &report);
                0
            }
```

with

```rust
            } else {
                render_terminal_report(io.stdout, &report);
                if let Some(line) = crate::cli::tokens::summary_line(
                    crate::storage::tokens::TokenRange::Day,
                    crate::clock::now_unix() as i64,
                ) {
                    let _ = writeln!(io.stdout, "\n{line}");
                }
                0
            }
```

- [ ] **Step 4: Run the tests**

Run: `timeout 900 scripts/test.sh test --lib cli::` then the full suite once: `timeout 900 scripts/test.sh`
Expected: all pass. If an existing CLI test compares the full usage/help text and now fails only because of the added `tokens` entries, update that expected text to include them (and list it in your report); do not weaken any other assertion.

- [ ] **Step 5: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
touch src/cli/tokens.rs && timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/cli/tokens.rs src/cli/mod.rs src/cli/status.rs src/cli/stats.rs
git commit -m "feat(cli): agy-pool tokens and token summary in status -v and stats

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(If Step 4 required updating an existing test's expected help text, add that test file to `git add` too.)

---

## After all tasks (Claude, not agy)

- Opus review of Tasks 1–2 (write path, flush/merge-back, removal fold, stream hook); Sonnet review of Tasks 3–4.
- README: one paragraph on token usage (where it shows, range semantics, cached ⊆ prompt).
- Throwaway-gateway browser check of the panel; push, PR, CI.
