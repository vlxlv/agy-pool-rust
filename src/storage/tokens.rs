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
// shortcut: usage is attributed by account id at flush time, so if an id is removed and reused within one
// flush interval (or during a stream that outlives the removal) that usage lands on the new account; key
// pending entries by a never-reused account identity if that matters.
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
        let prompt = |r| {
            usage_by_account(r, NOW)
                .unwrap()
                .get("acc_1")
                .map(|c| c.prompt)
                .unwrap_or(0)
        };
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
