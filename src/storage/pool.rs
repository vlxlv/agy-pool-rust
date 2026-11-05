use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use crate::storage::db::{StateDb, StorageError, ensure_file_permissions};
use crate::storage::model::{
    Account, GenerationEvent, MigrationResult, Pool, QuotaState, QuotaWindow, STATUS_VERIFICATION_PENDING,
    TargetQuotaFile, TargetRuntimeFile, deserialize_any, serialize_any,
};

static POOL_MUTEX: Mutex<()> = Mutex::new(());
static FILE_LOCKS_MU: Mutex<()> = Mutex::new(());
static PATH_MUTEX_MAP: LazyLock<Mutex<HashMap<PathBuf, Arc<std::sync::RwLock<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn get_path_mutex(path: &Path) -> Arc<std::sync::RwLock<()>> {
    let _guard = FILE_LOCKS_MU.lock().unwrap();
    let clean = crate::config::clean_path(path);
    let mut map = PATH_MUTEX_MAP.lock().unwrap();
    map.entry(clean)
        .or_insert_with(|| Arc::new(std::sync::RwLock::new(())))
        .clone()
}

/// EnsureDirs creates the configured state directory with mode 0700.
pub fn ensure_dirs() -> Result<(), StorageError> {
    let dir = crate::config::state_dir();
    crate::config::assert_safe_write_path(&dir)?;
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// WithFileLock acquires both an in-memory RWMutex and a POSIX flock on the stable sidecar lock file.
pub fn with_file_lock<T, F>(path: &Path, exclusive: bool, f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError>,
{
    if let Err(e) = crate::config::assert_safe_write_path(path)
        && crate::config::assert_safe_native_token_write(path).is_err()
    {
        return Err(StorageError::Config(e));
    }

    let mu = get_path_mutex(path);
    let _mem_guard = if exclusive { Some(mu.write().unwrap()) } else { None };
    let _mem_read_guard = if !exclusive { Some(mu.read().unwrap()) } else { None };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));

    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let lock_op = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };

    // SAFETY: fd is a valid open file descriptor for the lock file.
    let ret = unsafe { libc::flock(fd, lock_op) };
    if ret != 0 {
        return Err(StorageError::Io(std::io::Error::last_os_error()));
    }

    struct FlockGuard(std::os::unix::io::RawFd);
    impl Drop for FlockGuard {
        fn drop(&mut self) {
            // SAFETY: RawFd was locked, unlocking is safe.
            unsafe {
                libc::flock(self.0, libc::LOCK_UN);
            }
        }
    }

    let _flock_guard = FlockGuard(fd);
    f()
}

/// ReadPoolUnlocked reads and unmarshals a legacy or transitional pool JSON file without locking.
pub fn read_pool_unlocked(path: &Path) -> Result<Pool, StorageError> {
    if !path.exists() {
        return Ok(Pool::new_empty());
    }

    let data = std::fs::read(path)?;
    let raw: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&data)
        .map_err(|e| StorageError::Other(format!("corrupt pool state in {:?}: {}", path, e)))?;
    if !raw.contains_key("accounts") {
        return Err(StorageError::Other(
            "pool state must be an object containing an accounts list".to_string(),
        ));
    }

    let pool: Pool = serde_json::from_slice(&data)
        .map_err(|e| StorageError::Other(format!("corrupt pool state in {:?}: {}", path, e)))?;
    Ok(pool)
}

/// AtomicJSONWrite writes data to a file atomically via temp file with mode 0600 and fsync.
pub fn atomic_json_write<T: serde::Serialize>(path: &Path, data: &T) -> Result<(), StorageError> {
    if let Err(e) = crate::config::assert_safe_write_path(path)
        && crate::config::assert_safe_native_token_write(path).is_err()
    {
        return Err(StorageError::Config(e));
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    let file_stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let pid = std::process::id();
    let mut counter: u32 = 0;
    let (mut temp_file, tmp_path) = loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = dir.join(format!("{}.tmp.{}.{}.{}", file_stem, pid, nanos, counter));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break (f, candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                counter += 1;
                continue;
            }
            Err(e) => return Err(StorageError::Io(e)),
        }
    };

    struct TmpGuard(Option<PathBuf>);
    impl Drop for TmpGuard {
        fn drop(&mut self) {
            if let Some(ref p) = self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let mut guard = TmpGuard(Some(tmp_path.clone()));

    let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));

    let json_bytes = serde_json::to_vec_pretty(data)?;
    use std::io::Write;
    temp_file.write_all(&json_bytes)?;
    temp_file.write_all(b"\n")?;
    temp_file.sync_all()?;
    drop(temp_file);

    std::fs::rename(&tmp_path, path)?;
    guard.0 = None;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));

    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

/// LoadPoolFromTx reads the full pool state from an active read transaction.
pub fn load_pool_from_tx(tx: &rusqlite::Transaction) -> Result<Pool, StorageError> {
    let mut stmt = tx.prepare("SELECT key, value FROM runtime_global")?;
    let mut rows = stmt.query([])?;

    let mut strategy = "smart".to_string();
    let mut active_id: Option<String> = None;
    let mut show_email: Option<bool> = None;
    let mut account_order: Vec<String> = Vec::new();
    let mut gen_events: Vec<GenerationEvent> = Vec::new();
    let mut pool_extra: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

    while let Some(row) = rows.next()? {
        let k: Option<String> = row.get(0)?;
        let v: Option<String> = row.get(1)?;
        let key = match k {
            Some(k) => k,
            None => continue,
        };
        match key.as_str() {
            "strategy" => {
                if let Some(val) = v
                    && !val.is_empty()
                {
                    strategy = crate::config::migrate_legacy_strategy(&val);
                }
            }
            "active_account_id" => {
                if let Some(val) = v
                    && !val.is_empty()
                {
                    active_id = Some(val);
                }
            }
            "round_robin_last_account_id" => {
                // written by older versions; ignored
            }
            "show_email" => {
                if let Some(val) = v
                    && !val.is_empty()
                {
                    show_email = Some(val == "1" || val == "true");
                }
            }
            "account_order" => {
                if let Some(val) = v
                    && let Ok(ord) = serde_json::from_str::<Vec<String>>(&val)
                {
                    account_order = ord;
                }
            }
            "generation_events" => {
                if let Some(val) = v
                    && let Ok(evs) = serde_json::from_str::<Vec<GenerationEvent>>(&val)
                {
                    gen_events = evs;
                }
            }
            "pool_extra" => {
                if let Some(val) = v
                    && let Ok(ex) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&val)
                {
                    pool_extra = ex;
                }
            }
            _ => {}
        }
    }
    drop(rows);
    drop(stmt);

    let query = "
	SELECT
		a.id, a.name, a.email, a.access_token, a.refresh_token, a.id_token, a.token_expiry, a.created_at, a.updated_at, a.extra,
		r.request_count, r.gen_count, r.error_count, r.last_used_at, r.status, r.validation_url, r.rate_limited_until,
		q.gemini_5h_fraction, q.gemini_5h_reset, q.gemini_weekly_fraction, q.gemini_weekly_reset,
		q.third_party_5h_fraction, q.third_party_5h_reset, q.third_party_weekly_fraction, q.third_party_weekly_reset,
		q.remaining_fraction, q.reset_time, q.updated_at, q.raw_extra
	FROM accounts a
	LEFT JOIN account_runtime r ON a.id = r.account_id
	LEFT JOIN account_quota q ON a.id = q.account_id
	ORDER BY a.rowid ASC
    ";
    let mut acc_stmt = tx.prepare(query)?;
    let mut acc_rows = acc_stmt.query([])?;

    let mut accounts_list: Vec<Account> = Vec::new();
    while let Some(row) = acc_rows.next()? {
        let id: String = row.get(0)?;
        let name: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let email: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let access_token: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        let refresh_token: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
        let id_token: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
        let token_expiry: Option<f64> = row.get(6)?;
        let created_at: Option<i64> = row.get(7)?;
        let updated_at: Option<i64> = row.get(8)?;
        let extra_json: Option<String> = row.get(9)?;

        let request_count: i64 = row.get::<_, Option<i64>>(10)?.unwrap_or(0);
        let gen_count: Option<i64> = row.get(11)?;
        let error_count: i64 = row.get::<_, Option<i64>>(12)?.unwrap_or(0);
        let last_used_at: Option<i64> = row.get(13)?;
        let status: String = row.get::<_, Option<String>>(14)?.unwrap_or_default();
        let validation_url: Option<String> = row.get(15)?;
        let rate_limited_until: Option<f64> = row.get(16)?;

        let g5h_frac: Option<f64> = row.get(17)?;
        let g5h_reset: Option<String> = row.get(18)?;
        let gw_frac: Option<f64> = row.get(19)?;
        let gw_reset: Option<String> = row.get(20)?;
        let tp5h_frac: Option<f64> = row.get(21)?;
        let tp5h_reset: Option<String> = row.get(22)?;
        let tpw_frac: Option<f64> = row.get(23)?;
        let tpw_reset: Option<String> = row.get(24)?;
        let rem_frac: Option<f64> = row.get(25)?;
        let q_reset: Option<String> = row.get(26)?;
        let q_updated_at: Option<i64> = row.get(27)?;
        let raw_extra: Option<String> = row.get(28)?;

        let mut acc = Account {
            id,
            name,
            email,
            access_token,
            refresh_token,
            id_token,
            token_expiry,
            created_at,
            updated_at,
            status,
            validation_url,
            rate_limited_until,
            request_count,
            gen_count,
            error_count,
            last_used_at,
            last_quota: None,
            quota: None,
            gemini_5h_pct: None,
            gemini_weekly_pct: None,
            gemini_5h_reset: None,
            gemini_weekly_reset: None,
            gemini_5h_reset_sec: None,
            gemini_weekly_reset_sec: None,
            extra: serde_json::Map::new(),
        };

        if let Some(ref extra_str) = extra_json
            && !extra_str.is_empty()
            && let Ok(ex) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(extra_str)
        {
            acc.extra = ex;
        }

        let has_quota = g5h_frac.is_some()
            || gw_frac.is_some()
            || rem_frac.is_some()
            || q_updated_at.is_some()
            || raw_extra.is_some();
        if has_quota {
            let gemini_5h = if g5h_frac.is_some() || g5h_reset.is_some() {
                Some(QuotaWindow {
                    fraction: g5h_frac,
                    reset_time: deserialize_any(g5h_reset.as_deref()),
                    extra: serde_json::Map::new(),
                })
            } else {
                None
            };
            let gemini_weekly = if gw_frac.is_some() || gw_reset.is_some() {
                Some(QuotaWindow {
                    fraction: gw_frac,
                    reset_time: deserialize_any(gw_reset.as_deref()),
                    extra: serde_json::Map::new(),
                })
            } else {
                None
            };
            let third_party_5h = if tp5h_frac.is_some() || tp5h_reset.is_some() {
                Some(QuotaWindow {
                    fraction: tp5h_frac,
                    reset_time: deserialize_any(tp5h_reset.as_deref()),
                    extra: serde_json::Map::new(),
                })
            } else {
                None
            };
            let third_party_weekly = if tpw_frac.is_some() || tpw_reset.is_some() {
                Some(QuotaWindow {
                    fraction: tpw_frac,
                    reset_time: deserialize_any(tpw_reset.as_deref()),
                    extra: serde_json::Map::new(),
                })
            } else {
                None
            };
            let extra = if let Some(ref raw_str) = raw_extra
                && !raw_str.is_empty()
                && let Ok(qex) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw_str)
            {
                qex
            } else {
                serde_json::Map::new()
            };
            acc.last_quota = Some(QuotaState {
                gemini_5h,
                gemini_weekly,
                third_party_5h,
                third_party_weekly,
                remaining_fraction: rem_frac,
                reset_time: deserialize_any(q_reset.as_deref()),
                updated_at: q_updated_at,
                extra,
            });
        }

        accounts_list.push(acc);
    }
    drop(acc_rows);
    drop(acc_stmt);

    if !account_order.is_empty() {
        let mut acc_map: HashMap<String, Account> = accounts_list.into_iter().map(|a| (a.id.clone(), a)).collect();
        let mut ordered_list: Vec<Account> = Vec::with_capacity(acc_map.len());
        let mut used = std::collections::HashSet::new();
        for aid in &account_order {
            if let Some(a) = acc_map.remove(aid) {
                used.insert(aid.clone());
                ordered_list.push(a);
            }
        }
        for (_, a) in acc_map {
            ordered_list.push(a);
        }
        accounts_list = ordered_list;
    }

    Ok(Pool {
        version: 1,
        strategy,
        active_account_id: active_id,
        show_email,
        accounts: accounts_list,
        generation_events: gen_events,
        round_robin_last_account_id: None,
        extra: pool_extra,
    })
}

/// SavePoolToTx writes the entire pool state within an active write transaction.
pub fn save_pool_to_tx(tx: &rusqlite::Transaction, pool: &Pool) -> Result<(), StorageError> {
    let mut strat = crate::config::migrate_legacy_strategy(&pool.strategy);
    if strat.is_empty() {
        strat = "smart".to_string();
    }
    tx.execute(
        "INSERT INTO runtime_global (key, value) VALUES ('strategy', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [&strat],
    )?;

    if let Some(ref aid) = pool.active_account_id {
        if !aid.is_empty() {
            tx.execute(
                "INSERT INTO runtime_global (key, value) VALUES ('active_account_id', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [aid],
            )?;
        } else {
            tx.execute("DELETE FROM runtime_global WHERE key = 'active_account_id'", [])?;
        }
    } else {
        tx.execute("DELETE FROM runtime_global WHERE key = 'active_account_id'", [])?;
    }

    tx.execute(
        "DELETE FROM runtime_global WHERE key = 'round_robin_last_account_id'",
        [],
    )?;

    if let Some(show) = pool.show_email {
        let val = if show { "1" } else { "0" };
        tx.execute(
            "INSERT INTO runtime_global (key, value) VALUES ('show_email', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [val],
        )?;
    } else {
        tx.execute("DELETE FROM runtime_global WHERE key = 'show_email'", [])?;
    }

    let account_ids: Vec<String> = pool
        .accounts
        .iter()
        .filter(|a| !a.id.is_empty())
        .map(|a| a.id.clone())
        .collect();
    if let Ok(b) = serde_json::to_string(&account_ids) {
        tx.execute(
            "INSERT INTO runtime_global (key, value) VALUES ('account_order', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&b],
        )?;
    }

    if !pool.generation_events.is_empty() {
        if let Ok(b) = serde_json::to_string(&pool.generation_events) {
            tx.execute(
                "INSERT INTO runtime_global (key, value) VALUES ('generation_events', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&b],
            )?;
        }
    } else {
        tx.execute("DELETE FROM runtime_global WHERE key = 'generation_events'", [])?;
    }

    if !pool.extra.is_empty() {
        if let Ok(b) = serde_json::to_string(&pool.extra) {
            tx.execute(
                "INSERT INTO runtime_global (key, value) VALUES ('pool_extra', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&b],
            )?;
        }
    } else {
        tx.execute("DELETE FROM runtime_global WHERE key = 'pool_extra'", [])?;
    }

    let mut stmt = tx.prepare("SELECT id FROM accounts")?;
    let mut rows = stmt.query([])?;
    let mut existing_ids = Vec::new();
    while let Some(row) = rows.next()? {
        let aid: String = row.get(0)?;
        existing_ids.push(aid);
    }
    drop(rows);
    drop(stmt);

    let new_id_set: std::collections::HashSet<&str> = pool.accounts.iter().map(|a| a.id.as_str()).collect();
    for existing_id in existing_ids {
        if !new_id_set.contains(existing_id.as_str()) {
            tx.execute("DELETE FROM accounts WHERE id = ?1", [&existing_id])?;
        }
    }

    for a in &pool.accounts {
        if a.id.is_empty() {
            continue;
        }

        let extra_str = if !a.extra.is_empty() {
            serde_json::to_string(&a.extra).ok()
        } else {
            None
        };

        tx.execute(
            "INSERT INTO accounts (id, name, email, access_token, refresh_token, id_token, token_expiry, created_at, updated_at, extra)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 name=excluded.name,
                 email=excluded.email,
                 access_token=excluded.access_token,
                 refresh_token=excluded.refresh_token,
                 id_token=excluded.id_token,
                 token_expiry=excluded.token_expiry,
                 created_at=excluded.created_at,
                 updated_at=excluded.updated_at,
                 extra=excluded.extra;",
            rusqlite::params![
                &a.id,
                &a.name,
                &a.email,
                &a.access_token,
                &a.refresh_token,
                &a.id_token,
                &a.token_expiry,
                &a.created_at,
                &a.updated_at,
                &extra_str,
            ],
        )?;

        tx.execute(
            "INSERT INTO account_runtime (account_id, request_count, gen_count, error_count, last_used_at, status, validation_url, rate_limited_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account_id) DO UPDATE SET
                 request_count=excluded.request_count,
                 gen_count=excluded.gen_count,
                 error_count=excluded.error_count,
                 last_used_at=excluded.last_used_at,
                 status=excluded.status,
                 validation_url=excluded.validation_url,
                 rate_limited_until=excluded.rate_limited_until;",
            rusqlite::params![
                &a.id,
                a.request_count,
                a.gen_count,
                a.error_count,
                a.last_used_at,
                &a.status,
                a.validation_url.as_deref(),
                a.rate_limited_until,
            ],
        )?;

        if let Some(ref lq) = a.last_quota {
            let g5h_frac = lq.gemini_5h.as_ref().and_then(|w| w.fraction);
            let g5h_reset = serialize_any(&lq.gemini_5h.as_ref().and_then(|w| w.reset_time.clone()));
            let gw_frac = lq.gemini_weekly.as_ref().and_then(|w| w.fraction);
            let gw_reset = serialize_any(&lq.gemini_weekly.as_ref().and_then(|w| w.reset_time.clone()));
            let tp5h_frac = lq.third_party_5h.as_ref().and_then(|w| w.fraction);
            let tp5h_reset = serialize_any(&lq.third_party_5h.as_ref().and_then(|w| w.reset_time.clone()));
            let tpw_frac = lq.third_party_weekly.as_ref().and_then(|w| w.fraction);
            let tpw_reset = serialize_any(&lq.third_party_weekly.as_ref().and_then(|w| w.reset_time.clone()));
            let q_reset = serialize_any(&lq.reset_time);

            let raw_extra_str = if !lq.extra.is_empty() {
                serde_json::to_string(&lq.extra).ok()
            } else {
                None
            };

            tx.execute(
                "INSERT INTO account_quota (account_id, gemini_5h_fraction, gemini_5h_reset, gemini_weekly_fraction, gemini_weekly_reset, third_party_5h_fraction, third_party_5h_reset, third_party_weekly_fraction, third_party_weekly_reset, remaining_fraction, reset_time, updated_at, raw_extra)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(account_id) DO UPDATE SET
                     gemini_5h_fraction=excluded.gemini_5h_fraction,
                     gemini_5h_reset=excluded.gemini_5h_reset,
                     gemini_weekly_fraction=excluded.gemini_weekly_fraction,
                     gemini_weekly_reset=excluded.gemini_weekly_reset,
                     third_party_5h_fraction=excluded.third_party_5h_fraction,
                     third_party_5h_reset=excluded.third_party_5h_reset,
                     third_party_weekly_fraction=excluded.third_party_weekly_fraction,
                     third_party_weekly_reset=excluded.third_party_weekly_reset,
                     remaining_fraction=excluded.remaining_fraction,
                     reset_time=excluded.reset_time,
                     updated_at=excluded.updated_at,
                     raw_extra=excluded.raw_extra;",
                rusqlite::params![
                    &a.id,
                    g5h_frac,
                    g5h_reset,
                    gw_frac,
                    gw_reset,
                    tp5h_frac,
                    tp5h_reset,
                    tpw_frac,
                    tpw_reset,
                    lq.remaining_fraction,
                    q_reset,
                    lq.updated_at,
                    raw_extra_str,
                ],
            )?;
        } else {
            tx.execute("DELETE FROM account_quota WHERE account_id = ?1", [&a.id])?;
        }
    }

    Ok(())
}

/// RecordSuccess bumps one account's success counters in place instead of rewriting the whole pool.
pub fn record_success(account: &Account, is_generation: bool, now: i64) -> Result<(), StorageError> {
    ensure_dirs()?;
    let sdb = StateDb::global()?;
    let _pool_guard = POOL_MUTEX.lock().unwrap();

    sdb.update(|tx| {
        let mut id = String::new();
        if !account.id.is_empty() {
            let res: Result<String, _> =
                tx.query_row("SELECT id FROM accounts WHERE id = ?1", [&account.id], |r| r.get(0));
            if let Ok(found) = res {
                id = found;
            }
        }
        if id.is_empty() && !account.email.is_empty() {
            let res: Result<String, _> = tx.query_row(
                "SELECT id FROM accounts WHERE email = ?1 LIMIT 1",
                [&account.email],
                |r| r.get(0),
            );
            if let Ok(found) = res {
                id = found;
            }
        }
        if id.is_empty() {
            return Ok(());
        }

        tx.execute(
            "INSERT INTO account_runtime (account_id) VALUES (?1) ON CONFLICT(account_id) DO NOTHING",
            [&id],
        )?;

        if is_generation {
            tx.execute(
                "UPDATE account_runtime
                 SET request_count = request_count + 1,
                     gen_count = COALESCE(gen_count, 0) + 1,
                     last_used_at = ?1,
                     status = CASE WHEN status = ?2 THEN '' ELSE status END
                 WHERE account_id = ?3",
                rusqlite::params![now, STATUS_VERIFICATION_PENDING, &id],
            )?;
        } else {
            tx.execute(
                "UPDATE account_runtime
                 SET request_count = request_count + 1
                 WHERE account_id = ?1",
                [&id],
            )?;
        }

        tx.execute(
            "UPDATE account_runtime
             SET rate_limited_until = NULL
             WHERE account_id = ?1 AND status = '' AND rate_limited_until > 0",
            [&id],
        )?;

        if is_generation {
            let raw_val: Option<String> = tx
                .query_row(
                    "SELECT value FROM runtime_global WHERE key = 'generation_events'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let evs: Vec<GenerationEvent> = raw_val
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            let mut p = Pool::new_empty();
            p.generation_events = evs;
            p.record_generation_telemetry(&id, now);
            if let Ok(evs_bytes) = serde_json::to_string(&p.generation_events) {
                tx.execute(
                    "INSERT INTO runtime_global (key, value)
                     VALUES ('generation_events', ?1)
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [&evs_bytes],
                )?;
            }
        }

        let active_val: Option<String> = tx
            .query_row(
                "SELECT value FROM runtime_global WHERE key = 'active_account_id'",
                [],
                |r| r.get(0),
            )
            .ok();
        let active_id = active_val.unwrap_or_default();

        let mut should_promote = false;
        if active_id.is_empty() {
            should_promote = true;
        } else if active_id != id {
            let rlu: Option<f64> = tx
                .query_row(
                    "SELECT rate_limited_until FROM account_runtime WHERE account_id = ?1",
                    [&active_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            if let Some(limit) = rlu
                && limit > now as f64
            {
                should_promote = true;
            }
        }

        if should_promote {
            tx.execute(
                "INSERT INTO runtime_global (key, value)
                 VALUES ('active_account_id', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&id],
            )?;
        }

        Ok(())
    })
}

/// PoolTransaction executes an atomic read-modify-write transaction on the authoritative state database.
pub fn pool_transaction<F>(mutator: F) -> Result<(), StorageError>
where
    F: FnOnce(&mut Pool) -> Result<(), StorageError>,
{
    ensure_dirs()?;
    let sdb = StateDb::global()?;
    let _guard = POOL_MUTEX.lock().unwrap();

    sdb.update(|tx| {
        let mut pool = load_pool_from_tx(tx)?;
        mutator(&mut pool)?;
        save_pool_to_tx(tx, &pool)?;
        Ok(())
    })
}

/// LoadPool reads the pool state from the authoritative SQLite state store under a read transaction.
pub fn load_pool() -> Result<Pool, StorageError> {
    ensure_dirs()?;
    let sdb = StateDb::global()?;
    sdb.view(load_pool_from_tx)
}

/// SavePool replaces the entire pool under an immediate write transaction.
pub fn save_pool(pool: &Pool) -> Result<(), StorageError> {
    pool_transaction(|p| {
        *p = pool.clone();
        Ok(())
    })
}

/// FindAccount searches for an account by ID or email.
pub fn find_account<'a>(pool: &'a Pool, account: &Account) -> Option<&'a Account> {
    pool.accounts.iter().find(|a| {
        (!account.id.is_empty() && a.id == account.id) || (!account.email.is_empty() && a.email == account.email)
    })
}

/// NextAccountID allocates the lowest positive integer n such that "acc_n" is unused.
pub fn next_account_id(accounts: &[Account]) -> String {
    let used: std::collections::HashSet<&str> = accounts
        .iter()
        .map(|a| a.id.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let mut num = 1;
    loop {
        let candidate = format!("acc_{num}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
        num += 1;
    }
}

/// LoadRuntimeValue returns a runtime_global entry kept outside the pool snapshot, or "" if absent.
pub fn load_runtime_value(key: &str) -> Result<String, StorageError> {
    let sdb = StateDb::global()?;
    sdb.view(|tx| {
        let res: Result<String, _> =
            tx.query_row("SELECT value FROM runtime_global WHERE key = ?1", [key], |r| r.get(0));
        match res {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(String::new()),
            Err(e) => Err(StorageError::Sqlite(e)),
        }
    })
}

/// SaveRuntimeValue writes a runtime_global entry kept outside the pool snapshot; "" deletes it.
pub fn save_runtime_value(key: &str, value: &str) -> Result<(), StorageError> {
    let sdb = StateDb::global()?;
    sdb.update(|tx| {
        if value.is_empty() {
            tx.execute("DELETE FROM runtime_global WHERE key = ?1", [key])?;
        } else {
            tx.execute(
                "INSERT INTO runtime_global (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, value],
            )?;
        }
        Ok(())
    })
}

/// MigrateLegacyPool reads legacy Python state as read-only input and converts it into state.db under target_data_dir.
pub fn migrate_legacy_pool(source_path: &Path, target_data_dir: &Path) -> Result<MigrationResult, StorageError> {
    if source_path.as_os_str().is_empty() {
        return Err(StorageError::Other("source path is required".into()));
    }
    if target_data_dir.as_os_str().is_empty() {
        return Err(StorageError::Other("target data directory is required".into()));
    }

    let abs_source = if source_path.is_absolute() {
        source_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(source_path)
    };

    let source_data = std::fs::read(&abs_source)
        .map_err(|e| StorageError::Other(format!("failed to read legacy pool source {:?}: {}", abs_source, e)))?;

    let mut pool: Pool = serde_json::from_slice(&source_data)
        .map_err(|e| StorageError::Other(format!("corrupt or invalid legacy pool source {:?}: {}", abs_source, e)))?;
    pool.strategy = crate::config::migrate_legacy_strategy(&pool.strategy);

    crate::config::assert_safe_write_path(target_data_dir)?;
    let locks_dir = target_data_dir.join("locks");
    std::fs::create_dir_all(&locks_dir)?;
    let _ = std::fs::set_permissions(&locks_dir, std::fs::Permissions::from_mode(0o700));

    let target_db = target_data_dir.join("state.db");
    crate::config::assert_safe_write_path(&target_db)?;

    let pid = std::process::id();
    let tmp_db_path = target_data_dir.join(format!("state.db.tmp.{}", pid));
    let tmp_wal = format!("{}-wal", tmp_db_path.display());
    let tmp_shm = format!("{}-shm", tmp_db_path.display());
    let _ = std::fs::remove_file(&tmp_db_path);
    let _ = std::fs::remove_file(&tmp_wal);
    let _ = std::fs::remove_file(&tmp_shm);

    struct TmpCleaner(PathBuf);
    impl Drop for TmpCleaner {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(format!("{}-wal", self.0.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.0.display()));
        }
    }
    let cleaner = TmpCleaner(tmp_db_path.clone());

    let tmp_db = StateDb::open(&tmp_db_path)?;
    tmp_db.update(|tx| save_pool_to_tx(tx, &pool))?;

    let _ = tmp_db.view(|tx| {
        let _ = tx.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    });
    tmp_db.close()?;

    std::fs::rename(&tmp_db_path, &target_db)
        .map_err(|e| StorageError::Other(format!("failed to atomically publish state database: {}", e)))?;
    std::mem::forget(cleaner);
    ensure_file_permissions(&target_db)?;

    let target_accounts = target_data_dir.join("accounts.json");
    if crate::config::assert_safe_write_path(&target_accounts).is_ok() {
        let _ = atomic_json_write(&target_accounts, &pool);
    }

    let active_id = pool.active_account_id.clone().unwrap_or_default();
    Ok(MigrationResult {
        source_path: abs_source.display().to_string(),
        target_file: target_db.display().to_string(),
        accounts_count: pool.accounts.len(),
        active_account_id: active_id,
        strategy: pool.strategy,
    })
}

/// MigrateSplitJSONToDB imports preexisting Go split JSON state into state.db in target_data_dir.
pub fn migrate_split_json_to_db(source_dir: &Path, target_data_dir: &Path) -> Result<MigrationResult, StorageError> {
    if source_dir.as_os_str().is_empty() {
        return Err(StorageError::Other("source directory is required".into()));
    }
    if target_data_dir.as_os_str().is_empty() {
        return Err(StorageError::Other("target data directory is required".into()));
    }

    let accounts_file = source_dir.join("accounts.json");
    if !accounts_file.exists() {
        return Err(StorageError::Other(format!(
            "source accounts.json not found in {:?}",
            source_dir
        )));
    }

    let mut pool = read_pool_unlocked(&accounts_file)?;

    let runtime_file = source_dir.join("runtime.json");
    if let Ok(data) = std::fs::read(&runtime_file)
        && let Ok(rf) = serde_json::from_slice::<TargetRuntimeFile>(&data)
    {
        if rf.active_account_id.is_some() {
            pool.active_account_id = rf.active_account_id;
        }
        for acc in &mut pool.accounts {
            if let Some(r_rec) = rf.accounts.get(&acc.id) {
                acc.status = r_rec.status.clone();
                acc.validation_url = r_rec.validation_url.clone();
                acc.rate_limited_until = r_rec.rate_limited_until;
                acc.request_count = r_rec.request_count;
                acc.gen_count = r_rec.gen_count;
                acc.error_count = r_rec.error_count;
                acc.last_used_at = r_rec.last_used_at;
            }
        }
    }

    let quota_file = source_dir.join("quota.json");
    if let Ok(data) = std::fs::read(&quota_file)
        && let Ok(qf) = serde_json::from_slice::<TargetQuotaFile>(&data)
    {
        for acc in &mut pool.accounts {
            if let Some(q_rec) = qf.accounts.get(&acc.id) {
                acc.last_quota = Some(q_rec.clone());
            }
        }
    }

    crate::config::assert_safe_write_path(target_data_dir)?;
    let locks_dir = target_data_dir.join("locks");
    std::fs::create_dir_all(&locks_dir)?;
    let _ = std::fs::set_permissions(&locks_dir, std::fs::Permissions::from_mode(0o700));

    let target_db = target_data_dir.join("state.db");
    crate::config::assert_safe_write_path(&target_db)?;

    let pid = std::process::id();
    let tmp_db_path = target_data_dir.join(format!("state.db.tmp.{}", pid));
    let tmp_wal = format!("{}-wal", tmp_db_path.display());
    let tmp_shm = format!("{}-shm", tmp_db_path.display());
    let _ = std::fs::remove_file(&tmp_db_path);
    let _ = std::fs::remove_file(&tmp_wal);
    let _ = std::fs::remove_file(&tmp_shm);

    struct TmpCleaner(PathBuf);
    impl Drop for TmpCleaner {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(format!("{}-wal", self.0.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.0.display()));
        }
    }
    let cleaner = TmpCleaner(tmp_db_path.clone());

    let tmp_db = StateDb::open(&tmp_db_path)?;
    tmp_db.update(|tx| save_pool_to_tx(tx, &pool))?;

    let _ = tmp_db.view(|tx| {
        let _ = tx.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    });
    tmp_db.close()?;

    std::fs::rename(&tmp_db_path, &target_db)
        .map_err(|e| StorageError::Other(format!("failed to atomically rename state database: {}", e)))?;
    std::mem::forget(cleaner);
    ensure_file_permissions(&target_db)?;

    let active_id = pool.active_account_id.clone().unwrap_or_default();
    Ok(MigrationResult {
        source_path: source_dir.display().to_string(),
        target_file: target_db.display().to_string(),
        accounts_count: pool.accounts.len(),
        active_account_id: active_id,
        strategy: pool.strategy,
    })
}
