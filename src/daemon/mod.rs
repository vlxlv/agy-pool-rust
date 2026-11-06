pub mod control;
pub mod instance;
pub mod lifecycle;
pub mod log;
pub mod pid;
pub mod runtime;
pub mod server;
pub mod systemd;

pub use control::*;
pub use instance::*;
pub use lifecycle::*;
pub use log::*;
pub use pid::*;
pub use runtime::*;
pub use server::*;
pub use systemd::*;

#[derive(Debug)]
pub enum DaemonError {
    NotRunning,
    PIDNotFound,
    PIDMalformed(String),
    ProcessNotRunning,
    PIDReused,
    PIDNotOwned(String),
    AlreadyRunning,
    ForeignPortOccupied(String),
    ConfigMismatch(String),
    RuntimeStatsNotFound,
    RuntimeStatsStale(String),
    RuntimeStatsMismatch(String),
    Io(std::io::Error),
    Config(crate::config::ConfigError),
    Storage(crate::storage::StorageError),
    Message(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::NotRunning => write!(f, "daemon is not running"),
            DaemonError::PIDNotFound => write!(f, "pid file not found"),
            DaemonError::PIDMalformed(msg) => write!(f, "pid file malformed: {msg}"),
            DaemonError::ProcessNotRunning => write!(f, "process not running"),
            DaemonError::PIDReused => write!(f, "pid reused by unrelated process"),
            DaemonError::PIDNotOwned(msg) => write!(f, "pid file not owned by current process: {msg}"),
            DaemonError::AlreadyRunning => write!(f, "daemon is already running"),
            DaemonError::ForeignPortOccupied(msg) => write!(f, "port is occupied by another process: {msg}"),
            DaemonError::ConfigMismatch(msg) => write!(f, "daemon configuration mismatch: {msg}"),
            DaemonError::RuntimeStatsNotFound => write!(f, "runtime stats not found"),
            DaemonError::RuntimeStatsStale(msg) => write!(f, "runtime stats are stale: {msg}"),
            DaemonError::RuntimeStatsMismatch(msg) => write!(f, "runtime stats PID mismatch: {msg}"),
            DaemonError::Io(e) => write!(f, "{e}"),
            DaemonError::Config(e) => write!(f, "{e}"),
            DaemonError::Storage(e) => write!(f, "{e}"),
            DaemonError::Message(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<std::io::Error> for DaemonError {
    fn from(e: std::io::Error) -> Self {
        DaemonError::Io(e)
    }
}

impl From<crate::config::ConfigError> for DaemonError {
    fn from(e: crate::config::ConfigError) -> Self {
        DaemonError::Config(e)
    }
}

impl From<crate::storage::StorageError> for DaemonError {
    fn from(e: crate::storage::StorageError) -> Self {
        DaemonError::Storage(e)
    }
}

#[cfg(test)]
pub struct TestEnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    pub temp_dir: tempfile::TempDir,
}

#[cfg(test)]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }
}

#[cfg(test)]
pub fn test_env() -> TestEnvGuard {
    let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::config::set_test_mode(true);
    let temp_dir = tempfile::tempdir().expect("failed to create temp test dir");
    crate::config::configure_data_dir(temp_dir.path()).expect("failed to configure test data dir");
    TestEnvGuard { _lock: lock, temp_dir }
}
