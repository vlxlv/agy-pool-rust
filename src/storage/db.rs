use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const STATE_DB_SCHEMA_VERSION: &str = "1";
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;

pub const INFLIGHT_LEASE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS inflight_leases (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	account_id TEXT NOT NULL,
	pid INTEGER NOT NULL,
	started_at INTEGER NOT NULL,
	pid_ns TEXT NOT NULL DEFAULT ''
);"#;

const SCHEMA_DDL: &str = r#"
		CREATE TABLE IF NOT EXISTS meta (
			key TEXT PRIMARY KEY,
			value TEXT NOT NULL
		);

		CREATE TABLE IF NOT EXISTS accounts (
			id TEXT PRIMARY KEY,
			name TEXT NOT NULL DEFAULT '',
			email TEXT NOT NULL DEFAULT '',
			access_token TEXT NOT NULL DEFAULT '',
			refresh_token TEXT NOT NULL DEFAULT '',
			id_token TEXT NOT NULL DEFAULT '',
			token_expiry REAL,
			created_at INTEGER,
			updated_at INTEGER,
			extra TEXT
		);

		CREATE TABLE IF NOT EXISTS runtime_global (
			key TEXT PRIMARY KEY,
			value TEXT
		);

		CREATE TABLE IF NOT EXISTS account_runtime (
			account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
			request_count INTEGER NOT NULL DEFAULT 0,
			gen_count INTEGER,
			error_count INTEGER NOT NULL DEFAULT 0,
			last_used_at INTEGER,
			status TEXT NOT NULL DEFAULT '',
			validation_url TEXT,
			rate_limited_until REAL
		);

		CREATE TABLE IF NOT EXISTS account_quota (
			account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
			gemini_5h_fraction REAL,
			gemini_5h_reset TEXT,
			gemini_weekly_fraction REAL,
			gemini_weekly_reset TEXT,
			third_party_5h_fraction REAL,
			third_party_5h_reset TEXT,
			third_party_weekly_fraction REAL,
			third_party_weekly_reset TEXT,
			remaining_fraction REAL,
			reset_time TEXT,
			updated_at INTEGER,
			raw_extra TEXT
		);
"#;

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Config(crate::config::ConfigError),
    WriteBusy,
    Other(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "{}", e),
            StorageError::Sqlite(e) => write!(f, "{}", e),
            StorageError::Json(e) => write!(f, "{}", e),
            StorageError::Config(e) => write!(f, "{}", e),
            StorageError::WriteBusy => write!(f, "state database write lock busy"),
            StorageError::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        StorageError::Sqlite(e)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        StorageError::Json(e)
    }
}

impl From<crate::config::ConfigError> for StorageError {
    fn from(e: crate::config::ConfigError) -> Self {
        StorageError::Config(e)
    }
}

/// ensure_lease_columns adds pid_ns to lease tables created before it existed.
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

/// EnsureFilePermissions enforces 0600 on the database file and sidecars, and 0700 on the parent directory.
pub fn ensure_file_permissions(db_path: &Path) -> Result<(), StorageError> {
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)
            .map_err(|e| StorageError::Other(format!("failed to create state directory: {}", e)))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    if let Ok(meta) = std::fs::metadata(db_path)
        && meta.permissions().mode() & 0o777 != 0o600
    {
        let _ = std::fs::set_permissions(db_path, std::fs::Permissions::from_mode(0o600));
    }
    let wal_path = format!("{}-wal", db_path.display());
    if let Ok(meta) = std::fs::metadata(&wal_path)
        && meta.permissions().mode() & 0o777 != 0o600
    {
        let _ = std::fs::set_permissions(&wal_path, std::fs::Permissions::from_mode(0o600));
    }
    let shm_path = format!("{}-shm", db_path.display());
    if let Ok(meta) = std::fs::metadata(&shm_path)
        && meta.permissions().mode() & 0o777 != 0o600
    {
        let _ = std::fs::set_permissions(&shm_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn current_rfc3339() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

struct ConnectionPool {
    path: PathBuf,
    conns: Mutex<Vec<rusqlite::Connection>>,
    open_count: Mutex<usize>,
    cv: Condvar,
    max_conns: usize,
}

impl ConnectionPool {
    fn new(path: PathBuf, initial_conn: rusqlite::Connection) -> Self {
        Self {
            path,
            conns: Mutex::new(vec![initial_conn]),
            open_count: Mutex::new(1),
            cv: Condvar::new(),
            max_conns: 10,
        }
    }

    fn open_connection(path: &Path) -> Result<rusqlite::Connection, StorageError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA journal_size_limit = 1048576;",
        )?;
        Ok(conn)
    }

    fn acquire(&self) -> Result<PooledConnection<'_>, StorageError> {
        let mut count = self.open_count.lock().unwrap();
        loop {
            {
                let mut conns = self.conns.lock().unwrap();
                if let Some(conn) = conns.pop() {
                    return Ok(PooledConnection {
                        pool: self,
                        conn: Some(conn),
                    });
                }
            }
            if *count < self.max_conns {
                *count += 1;
                drop(count);
                match Self::open_connection(&self.path) {
                    Ok(conn) => {
                        return Ok(PooledConnection {
                            pool: self,
                            conn: Some(conn),
                        });
                    }
                    Err(e) => {
                        *self.open_count.lock().unwrap() -= 1;
                        self.cv.notify_one();
                        return Err(e);
                    }
                }
            }
            count = self.cv.wait(count).unwrap();
        }
    }

    fn close(&self) {
        let mut conns = self.conns.lock().unwrap();
        conns.clear();
    }
}

pub struct PooledConnection<'a> {
    pool: &'a ConnectionPool,
    conn: Option<rusqlite::Connection>,
}

impl<'a> std::ops::Deref for PooledConnection<'a> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &rusqlite::Connection {
        self.conn.as_ref().unwrap()
    }
}

impl<'a> std::ops::DerefMut for PooledConnection<'a> {
    fn deref_mut(&mut self) -> &mut rusqlite::Connection {
        self.conn.as_mut().unwrap()
    }
}

impl<'a> Drop for PooledConnection<'a> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.conns.lock().unwrap().push(conn);
            self.pool.cv.notify_one();
        }
    }
}

struct DbInner {
    path: PathBuf,
    pool: ConnectionPool,
    write_mu: Mutex<()>,
}

#[derive(Clone)]
enum DbMode {
    Direct(Arc<DbInner>),
    Global,
}

#[derive(Clone)]
pub struct StateDb {
    mode: DbMode,
}

static ACTIVE_GLOBAL: Mutex<Option<(PathBuf, Arc<DbInner>)>> = Mutex::new(None);
static GLOBAL_DB: StateDb = StateDb { mode: DbMode::Global };

impl StateDb {
    /// OpenStateDB opens (and initializes if needed) the state database at path.
    /// Fails closed if the database is corrupt, 0 bytes, or has an unsupported schema version.
    pub fn open(db_path: &Path) -> Result<StateDb, StorageError> {
        crate::config::assert_safe_write_path(db_path)?;

        let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| StorageError::Other(format!("failed to create state directory {:?}: {}", dir, e)))?;
        }

        let exists = db_path.exists();
        if exists {
            let meta = std::fs::metadata(db_path)
                .map_err(|e| StorageError::Other(format!("failed to stat state database {:?}: {}", db_path, e)))?;
            if meta.len() == 0 {
                return Err(StorageError::Other(format!(
                    "corrupt state database {:?}: file is 0 bytes",
                    db_path
                )));
            }
        }

        let test_conn = rusqlite::Connection::open(db_path)
            .map_err(|e| StorageError::Other(format!("failed to open state database {:?}: {}", db_path, e)))?;

        if let Err(e) = test_conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA journal_size_limit = 1048576;",
        ) {
            return Err(StorageError::Other(format!(
                "corrupt or uninitialized state database in {:?}: {}",
                db_path, e
            )));
        }

        if exists {
            let schema_ver: Result<String, _> =
                test_conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                    row.get(0)
                });
            let schema_ver = match schema_ver {
                Ok(v) => v,
                Err(e) => {
                    return Err(StorageError::Other(format!(
                        "corrupt or uninitialized state database in {:?}: {}",
                        db_path, e
                    )));
                }
            };
            if schema_ver != STATE_DB_SCHEMA_VERSION {
                return Err(StorageError::Other(format!(
                    "unsupported schema version {:?} in {:?} (expected {})",
                    schema_ver, db_path, STATE_DB_SCHEMA_VERSION
                )));
            }
        } else {
            let tx = test_conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA_DDL)?;
            let now_str = current_rfc3339();
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('created_at', ?2);",
                [STATE_DB_SCHEMA_VERSION, &now_str],
            )?;
            tx.execute(
                "INSERT INTO runtime_global (key, value) VALUES ('strategy', 'smart');",
                [],
            )?;
            tx.commit()?;
        }

        test_conn.execute_batch(INFLIGHT_LEASE_DDL)?;
        test_conn.execute_batch(super::tokens::TOKEN_USAGE_DDL)?;
        ensure_lease_columns(&test_conn)?;
        ensure_file_permissions(db_path)?;

        let inner = Arc::new(DbInner {
            path: db_path.to_path_buf(),
            pool: ConnectionPool::new(db_path.to_path_buf(), test_conn),
            write_mu: Mutex::new(()),
        });

        Ok(StateDb {
            mode: DbMode::Direct(inner),
        })
    }

    fn get_inner(&self) -> Result<Arc<DbInner>, StorageError> {
        match self.mode {
            DbMode::Direct(ref inner) => Ok(inner.clone()),
            DbMode::Global => {
                // Resolve the path before locking: a panic while holding the lock would poison it for every later caller.
                let current_path = crate::config::state_db_file();
                let mut lock = ACTIVE_GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((ref p, ref inner)) = *lock
                    && p == &current_path
                {
                    return Ok(inner.clone());
                }

                *lock = None;

                if !current_path.exists() {
                    let data_dir = crate::config::data_dir();
                    let split_accounts = data_dir.join("accounts.json");
                    let legacy_accounts = data_dir.join("agy-pool-accounts.json");
                    if split_accounts.exists() {
                        let _ = crate::storage::pool::migrate_split_json_to_db(&data_dir, &data_dir);
                    } else if legacy_accounts.exists() {
                        let _ = crate::storage::pool::migrate_legacy_pool(&legacy_accounts, &data_dir);
                    }
                }

                let sdb = StateDb::open(&current_path)?;
                let inner = match sdb.mode {
                    DbMode::Direct(inner) => inner,
                    DbMode::Global => unreachable!(),
                };
                *lock = Some((current_path, inner.clone()));
                Ok(inner)
            }
        }
    }

    /// global returns the active StateDb singleton for the configured data directory (Go GetStateDB).
    pub fn global() -> Result<&'static StateDb, StorageError> {
        GLOBAL_DB.get_inner()?;
        Ok(&GLOBAL_DB)
    }

    /// path returns the path to the state database file.
    pub fn path(&self) -> Result<PathBuf, StorageError> {
        Ok(self.get_inner()?.path.clone())
    }

    /// CheckpointPassive copies committed WAL frames into the main database without waiting on readers or writers.
    pub fn checkpoint_passive(&self) -> Result<(), StorageError> {
        let inner = self.get_inner()?;
        let conn = inner.pool.acquire()?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }

    /// View executes a read-only transaction against the database.
    pub fn view<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T, StorageError>,
    {
        let inner = self.get_inner()?;
        let conn = inner.pool.acquire()?;
        let tx = conn.unchecked_transaction()?;
        let res = f(&tx)?;
        let _ = tx.commit();
        Ok(res)
    }

    /// Update executes an immediate write transaction (BEGIN IMMEDIATE).
    pub fn update<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T, StorageError>,
    {
        let inner = self.get_inner()?;
        let _guard = inner.write_mu.lock().unwrap();

        let res = (|| -> Result<T, StorageError> {
            let conn = inner.pool.acquire()?;
            let mut attempt = 0;
            let tx = loop {
                match rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate) {
                    Ok(tx) => break tx,
                    Err(e) => {
                        let err_str = e.to_string();
                        if (err_str.contains("busy") || err_str.contains("locked")) && attempt < 10 {
                            attempt += 1;
                            std::thread::sleep(Duration::from_millis(50 * attempt as u64));
                            continue;
                        }
                        return Err(StorageError::Sqlite(e));
                    }
                }
            };
            let out = f(&tx)?;
            tx.commit()?;
            let _ = ensure_file_permissions(&inner.path);
            Ok(out)
        })();

        if res.is_err() {
            crate::observability::record_persistence_error();
        }
        res
    }

    /// TryUpdate runs fn in a BEGIN IMMEDIATE transaction like Update, but waits at most busy for the
    /// SQLite write lock or other in-process writers, returning WriteBusy instead of queueing.
    pub fn try_update<T, F>(&self, busy: Duration, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T, StorageError>,
    {
        let inner = self.get_inner()?;
        let deadline = Instant::now() + busy;

        let _guard = loop {
            match inner.write_mu.try_lock() {
                Ok(g) => break g,
                Err(_) => {
                    if Instant::now() >= deadline {
                        return Err(StorageError::WriteBusy);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        };

        let remaining_busy = deadline.saturating_duration_since(Instant::now());
        let busy_ms = remaining_busy.as_millis().max(1) as u64;

        let res = (|| -> Result<T, StorageError> {
            let conn = inner.pool.acquire()?;
            conn.execute_batch(&format!("PRAGMA busy_timeout = {};", busy_ms))?;

            let tx = match rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate) {
                Ok(tx) => tx,
                Err(e) => {
                    let err_str = e.to_string();
                    let _ = conn.execute_batch(&format!("PRAGMA busy_timeout = {};", DEFAULT_BUSY_TIMEOUT_MS));
                    if err_str.contains("busy") || err_str.contains("locked") {
                        return Err(StorageError::WriteBusy);
                    }
                    return Err(StorageError::Sqlite(e));
                }
            };

            let out = match f(&tx) {
                Ok(val) => {
                    tx.commit().map_err(StorageError::Sqlite)?;
                    val
                }
                Err(e) => {
                    let _ = tx.rollback();
                    let _ = conn.execute_batch(&format!("PRAGMA busy_timeout = {};", DEFAULT_BUSY_TIMEOUT_MS));
                    return Err(e);
                }
            };

            let _ = conn.execute_batch(&format!("PRAGMA busy_timeout = {};", DEFAULT_BUSY_TIMEOUT_MS));
            let _ = ensure_file_permissions(&inner.path);
            Ok(out)
        })();

        if res.is_err() {
            crate::observability::record_persistence_error();
        }
        res
    }

    /// Close closes all idle connections in the pool.
    pub fn close(&self) -> Result<(), StorageError> {
        if let DbMode::Direct(ref inner) = self.mode {
            inner.pool.close();
        }
        Ok(())
    }
}
