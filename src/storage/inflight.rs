use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use crate::storage::db::{StateDb, StorageError};

/// leaseMaxAge bounds how long a lease can outlive its request, so a lease whose release failed
/// inside a long-lived process stops counting against the account.
pub const LEASE_MAX_AGE: Duration = Duration::from_secs(3600);

/// reserveBusyTimeout bounds how long selection waits for the shared write lock; past it the caller
/// falls back to in-process in-flight counts instead of stalling every generation request.
pub const RESERVE_BUSY_TIMEOUT: Duration = Duration::from_millis(100);

/// deadLeasePruneInterval throttles pruneDeadLeases off the per-request path; a crashed process's leases
/// keep counting for at most this long.
pub const DEAD_LEASE_PRUNE_INTERVAL_SECS: i64 = 5;

// ownStaleLeasesCleared records that leases left by a dead process whose PID and namespace this
// process now reuses were deleted; it is set only after that delete commits, so a failed attempt retries.
static OWN_STALE_LEASES_CLEARED: AtomicBool = AtomicBool::new(false);

// lastDeadLeasePrune is the Unix second of the last committed prune.
static LAST_DEAD_LEASE_PRUNE: AtomicI64 = AtomicI64::new(0);

static PID_NAMESPACE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    std::fs::read_link("/proc/self/ns/pid")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
});

/// pid_namespace identifies this process's PID namespace. A PID, and kill(pid, 0), only identify a
/// process within one namespace, so processes sharing the DB from different containers are told apart.
pub fn pid_namespace() -> &'static str {
    &PID_NAMESPACE
}

/// Sets whether own stale leases were cleared (matching Go package-level ownStaleLeasesCleared).
pub fn set_own_stale_leases_cleared(cleared: bool) {
    OWN_STALE_LEASES_CLEARED.store(cleared, Ordering::SeqCst);
}

/// Sets the Unix second of the last committed prune (matching Go package-level lastDeadLeasePrune).
pub fn set_last_dead_lease_prune(timestamp: i64) {
    LAST_DEAD_LEASE_PRUNE.store(timestamp, Ordering::SeqCst);
}

fn current_time_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Checks whether a process with the given PID is alive in the current PID namespace.
pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: libc::kill(pid, 0) performs error checking only without sending any signal;
    // it does not mutate memory or process state.
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if res == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error().raw_os_error();
    err == Some(libc::EPERM)
}

/// ensure_lease_columns adds pid_ns to lease tables created before it existed. Rows from those versions
/// keep "" (namespace unknown) and are never pruned by PID, only by leaseMaxAge.
pub fn ensure_lease_columns(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('inflight_leases')")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        if name == "pid_ns" {
            return Ok(());
        }
    }
    conn.execute(
        "ALTER TABLE inflight_leases ADD COLUMN pid_ns TEXT NOT NULL DEFAULT ''",
        [],
    )?;
    Ok(())
}

fn prune_dead_leases(tx: &rusqlite::Transaction<'_>, self_pid: i32, pid_ns: &str) -> Result<(), rusqlite::Error> {
    let cutoff = current_time_secs() - LEASE_MAX_AGE.as_secs() as i64;
    tx.execute("DELETE FROM inflight_leases WHERE started_at < ?1", [cutoff])?;

    // Only leases from this PID namespace can be checked with kill(pid, 0); others age out.
    let mut stmt = tx.prepare("SELECT DISTINCT pid FROM inflight_leases WHERE pid != ?1 AND pid_ns = ?2")?;
    let mut rows = stmt.query(rusqlite::params![self_pid, pid_ns])?;
    let mut dead = Vec::new();
    while let Some(row) = rows.next()? {
        let pid: i32 = row.get(0)?;
        if !process_alive(pid) {
            dead.push(pid);
        }
    }
    drop(rows);
    drop(stmt);

    for pid in dead {
        tx.execute(
            "DELETE FROM inflight_leases WHERE pid = ?1 AND pid_ns = ?2",
            rusqlite::params![pid, pid_ns],
        )?;
    }
    Ok(())
}

/// LeaseGuard holds an in-flight lease row in inflight_leases.
/// When dropped, the lease row is automatically deleted from the database.
pub struct LeaseGuard {
    lease_id: i64,
    db: Option<StateDb>,
}

impl std::fmt::Debug for LeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseGuard").field("lease_id", &self.lease_id).finish()
    }
}

impl LeaseGuard {
    /// empty creates a no-op guard with no associated database row.
    pub fn empty() -> Self {
        Self { lease_id: 0, db: None }
    }

    /// release deletes the lease from the database. Calling release multiple times is safe and idempotent.
    pub fn release(&mut self) {
        if self.lease_id > 0 {
            if let Some(db) = self.db.take() {
                let lease_id = self.lease_id;
                let _ = db.update(move |tx| {
                    tx.execute("DELETE FROM inflight_leases WHERE id = ?1", [lease_id])?;
                    Ok(())
                });
            }
            self.lease_id = 0;
        }
    }

    /// lease_id returns the row ID of this lease in the database, or 0 if empty.
    pub fn lease_id(&self) -> i64 {
        self.lease_id
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// reserve_in_flight_with_db runs pick inside a cross-process write transaction (BEGIN IMMEDIATE), passing the
/// in-flight counts held by other live processes, and records a lease for the account ID pick returns.
/// An empty ID records nothing. The returned LeaseGuard deletes the lease on Drop.
/// If the write lock stays busy past reserveBusyTimeout it returns WriteBusy without calling pick.
pub fn reserve_in_flight_with_db<F>(sdb: &StateDb, pick: F) -> Result<(String, LeaseGuard), StorageError>
where
    F: FnOnce(&HashMap<String, i64>) -> String,
{
    let self_pid = std::process::id() as i32;
    let pid_ns = pid_namespace();
    let now = current_time_secs();
    let mut pruned = false;
    let mut chosen_account = String::new();
    let mut lease_id: i64 = 0;

    let mut pick_opt = Some(pick);

    let update_res = sdb.try_update(RESERVE_BUSY_TIMEOUT, |tx| {
        if !OWN_STALE_LEASES_CLEARED.load(Ordering::SeqCst) {
            tx.execute(
                "DELETE FROM inflight_leases WHERE pid = ?1 AND pid_ns = ?2",
                rusqlite::params![self_pid, pid_ns],
            )?;
        }

        pruned = now - LAST_DEAD_LEASE_PRUNE.load(Ordering::SeqCst) >= DEAD_LEASE_PRUNE_INTERVAL_SECS;
        if pruned {
            prune_dead_leases(tx, self_pid, pid_ns)?;
        }

        let mut remote = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT account_id, COUNT(*) FROM inflight_leases WHERE NOT (pid = ?1 AND pid_ns = ?2) GROUP BY account_id",
            )?;
            let mut rows = stmt.query(rusqlite::params![self_pid, pid_ns])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                remote.insert(id, count);
            }
        }

        let pick_fn = pick_opt.take().expect("pick called once");
        let account_id = pick_fn(&remote);
        if account_id.is_empty() {
            return Ok(());
        }

        tx.execute(
            "INSERT INTO inflight_leases (account_id, pid, started_at, pid_ns) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&account_id, self_pid, now, pid_ns],
        )?;
        lease_id = tx.last_insert_rowid();
        chosen_account = account_id;
        Ok(())
    });

    update_res?;

    OWN_STALE_LEASES_CLEARED.store(true, Ordering::SeqCst);
    if pruned {
        LAST_DEAD_LEASE_PRUNE.store(now, Ordering::SeqCst);
    }

    if lease_id == 0 {
        return Ok((chosen_account, LeaseGuard::empty()));
    }

    Ok((
        chosen_account,
        LeaseGuard {
            lease_id,
            db: Some(sdb.clone()),
        },
    ))
}

/// reserve_in_flight runs pick against the global StateDb singleton.
pub fn reserve_in_flight<F>(pick: F) -> Result<(String, LeaseGuard), StorageError>
where
    F: FnOnce(&HashMap<String, i64>) -> String,
{
    let sdb = StateDb::global()?;
    reserve_in_flight_with_db(sdb, pick)
}

/// lease_in_flight_with_db records a lease on a specific database without a cross-process pick.
pub fn lease_in_flight_with_db(sdb: &StateDb, account_id: &str) -> Result<LeaseGuard, StorageError> {
    let (_, guard) = reserve_in_flight_with_db(sdb, |_| account_id.to_string())?;
    Ok(guard)
}

/// lease_in_flight records a lease for an account chosen without a cross-process pick (e.g. failover).
pub fn lease_in_flight(account_id: &str) -> Result<LeaseGuard, StorageError> {
    let sdb = StateDb::global()?;
    lease_in_flight_with_db(sdb, account_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        #[allow(dead_code)]
        path: PathBuf,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_test_env() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        let path = dir.path().to_path_buf();
        TestEnv {
            _lock: lock,
            _dir: dir,
            path,
        }
    }

    #[test]
    fn test_reserve_in_flight_sees_other_processes_and_prunes_dead() {
        let _env = setup_test_env();
        let sdb = StateDb::global().unwrap();
        let now = current_time_secs();
        set_own_stale_leases_cleared(false);
        set_last_dead_lease_prune(0);

        // SAFETY: libc::getppid() reads the parent process ID; safe to call without side-effects.
        let ppid = unsafe { libc::getppid() as i32 };
        let self_pid = std::process::id() as i32;
        let ns = pid_namespace();
        let two_hours_ago = now - 2 * 3600;

        sdb.update(|tx| {
            tx.execute(
                "INSERT INTO inflight_leases (account_id, pid, started_at, pid_ns) VALUES
                 ('a', ?1, ?2, ?3), ('a', ?4, ?5, ?6), ('b', 99999999, ?7, ?8),
                 ('c', ?9, ?10, ?11), ('d', 99999999, ?12, 'pid:[other]'), ('e', ?13, ?14, ?15)",
                rusqlite::params![
                    ppid,
                    now,
                    ns,
                    ppid,
                    now,
                    ns,
                    now,
                    ns,
                    ppid,
                    two_hours_ago,
                    ns,
                    now,
                    self_pid,
                    now,
                    ns,
                ],
            )?;
            Ok(())
        })
        .unwrap();

        let mut seen = HashMap::new();
        let (id, mut release) = reserve_in_flight(|remote| {
            seen = remote.clone();
            "b".to_string()
        })
        .unwrap();

        assert_eq!(id, "b");
        assert_eq!(
            seen.get("a").copied().unwrap_or(0),
            2,
            "remote counts = {:?}, want a=2",
            seen
        );
        assert_eq!(
            seen.get("b").copied().unwrap_or(0),
            0,
            "dead b should be pruned, got {:?}",
            seen
        );
        assert_eq!(
            seen.get("c").copied().unwrap_or(0),
            0,
            "expired c should be pruned, got {:?}",
            seen
        );
        assert_eq!(
            seen.get("d").copied().unwrap_or(0),
            1,
            "foreign-namespace d should be kept, got {:?}",
            seen
        );
        assert_eq!(
            seen.get("e").copied().unwrap_or(0),
            0,
            "own e should be excluded, got {:?}",
            seen
        );

        let count = || -> i64 {
            sdb.view(|tx| {
                Ok(
                    tx.query_row("SELECT COUNT(*) FROM inflight_leases WHERE pid = ?1", [self_pid], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap()
        };

        assert_eq!(count(), 1, "own leases should be 1");
        release.release();
        release.release();
        assert_eq!(count(), 0, "own leases after release should be 0");
    }

    #[test]
    fn test_reserve_in_flight_gives_up_when_write_lock_busy() {
        let _env = setup_test_env();
        let sdb = StateDb::global().unwrap();
        let db_path = sdb.path().unwrap();

        // Another connection holding the SQLite write lock with BEGIN IMMEDIATE.
        let other = rusqlite::Connection::open(&db_path).unwrap();
        other
            .execute_batch("PRAGMA busy_timeout = 0; BEGIN IMMEDIATE;")
            .unwrap();

        let start = Instant::now();
        let mut called = false;
        let res = reserve_in_flight(|_| {
            called = true;
            "a".to_string()
        });
        let d = start.elapsed();

        assert!(
            matches!(res, Err(StorageError::WriteBusy)),
            "ReserveInFlight = {:?}, want WriteBusy",
            res.err()
        );
        assert!(!called, "pick should not be called when write lock is busy");
        assert!(
            d < Duration::from_secs(1),
            "ReserveInFlight waited {:?}, want about {:?}",
            d,
            RESERVE_BUSY_TIMEOUT
        );

        other.execute_batch("ROLLBACK;").unwrap();
        drop(other);

        let res2 = reserve_in_flight(|_| "a".to_string());
        assert!(res2.is_ok(), "ReserveInFlight after lock released: {:?}", res2.err());
    }

    #[test]
    fn test_reserve_in_flight_waits_out_brief_in_process_writer() {
        let _env = setup_test_env();

        let hold_write_mu = |d: Duration| {
            let sdb = StateDb::global().unwrap().clone();
            let ready = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let ready_clone = ready.clone();
            std::thread::spawn(move || {
                let _ = sdb.update(|_| {
                    {
                        let mut lock = ready_clone.0.lock().unwrap();
                        *lock = true;
                        ready_clone.1.notify_one();
                    }
                    std::thread::sleep(d);
                    Ok(())
                });
            });
            let mut lock = ready.0.lock().unwrap();
            while !*lock {
                lock = ready.1.wait(lock).unwrap();
            }
        };

        // A routine write (e.g. a lease release) finishing within the budget must not cause a fallback.
        hold_write_mu(Duration::from_millis(30));
        let res = reserve_in_flight(|_| "a".to_string());
        assert!(
            res.is_ok(),
            "ReserveInFlight behind a 30ms writer = {:?}, want success",
            res.err()
        );
        drop(res);

        // A writer holding past the budget still yields WriteBusy without calling pick.
        hold_write_mu(3 * RESERVE_BUSY_TIMEOUT);
        let mut called = false;
        let res = reserve_in_flight(|_| {
            called = true;
            "a".to_string()
        });
        assert!(
            matches!(res, Err(StorageError::WriteBusy)),
            "ReserveInFlight behind a long writer = {:?}, want WriteBusy",
            res.err()
        );
        assert!(!called, "pick should not be called when writer holds past budget");

        std::thread::sleep(3 * RESERVE_BUSY_TIMEOUT); // let the holder release before cleanup
    }

    #[test]
    fn test_ensure_lease_columns_migrates_old_table() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("old.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE inflight_leases (id INTEGER PRIMARY KEY AUTOINCREMENT,
             account_id TEXT NOT NULL, pid INTEGER NOT NULL, started_at INTEGER NOT NULL);
             INSERT INTO inflight_leases (account_id, pid, started_at) VALUES ('a', 1, 0);",
        )
        .unwrap();

        for i in 0..2 {
            // second call must be a no-op
            ensure_lease_columns(&conn).unwrap_or_else(|e| panic!("ensure_lease_columns #{}: {}", i + 1, e));
        }

        let ns: String = conn
            .query_row("SELECT pid_ns FROM inflight_leases WHERE account_id = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ns, "", "migrated row pid_ns = {:?}, want empty", ns);
    }

    #[test]
    fn test_reserve_in_flight_throttles_dead_lease_prune() {
        let _env = setup_test_env();
        let sdb = StateDb::global().unwrap();
        let ns = pid_namespace();

        let insert_dead = || {
            let now = current_time_secs();
            sdb.update(|tx| {
                tx.execute(
                    "INSERT INTO inflight_leases (account_id, pid, started_at, pid_ns) VALUES ('b', 99999999, ?1, ?2)",
                    rusqlite::params![now, ns],
                )?;
                Ok(())
            })
            .unwrap();
        };

        let reserve = || -> HashMap<String, i64> {
            let mut seen = HashMap::new();
            let (_, mut release) = reserve_in_flight(|remote| {
                seen = remote.clone();
                String::new()
            })
            .unwrap();
            release.release();
            seen
        };

        set_last_dead_lease_prune(0);
        insert_dead();
        let seen = reserve();
        assert_eq!(
            seen.get("b").copied().unwrap_or(0),
            0,
            "first reservation should prune the dead lease, got {:?}",
            seen
        );

        insert_dead();
        let seen = reserve();
        assert_eq!(
            seen.get("b").copied().unwrap_or(0),
            1,
            "reservation within {}s of a prune should skip it, got {:?}",
            DEAD_LEASE_PRUNE_INTERVAL_SECS,
            seen
        );

        let now = current_time_secs();
        set_last_dead_lease_prune(now - DEAD_LEASE_PRUNE_INTERVAL_SECS);
        let seen = reserve();
        assert_eq!(
            seen.get("b").copied().unwrap_or(0),
            0,
            "reservation after the interval should prune again, got {:?}",
            seen
        );
    }
}
