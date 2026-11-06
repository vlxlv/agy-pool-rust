use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::DaemonError;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct DaemonInfo {
    pub pid: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_mtime: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<super::runtime::RuntimeStats>,
}

pub struct PidLock {
    file: File,
    pub lock_path: PathBuf,
}

impl Drop for PidLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl PidLock {
    pub fn unlock(self) {
        // Drops self, triggering flock unlock
    }
}

/// Returns the canonical path of the agy-pool binary.
pub fn get_entrypoint_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent()
            && parent.file_name().and_then(|n| n.to_str()) == Some("deps")
            && let Some(target_dir) = parent.parent()
        {
            let binary = target_dir.join("agy-pool");
            if binary.exists() {
                return binary;
            }
        }
        if let Ok(eval) = exe.canonicalize() {
            return eval;
        }
        return exe;
    }
    PathBuf::from("agy-pool")
}

/// Writes the PID metadata atomically with mode 0600.
pub fn write_pid_file(path: &Path, mut info: DaemonInfo) -> Result<(), DaemonError> {
    crate::config::assert_safe_write_path(path)?;
    if info.listen_host.is_none() && info.listen.is_some() {
        info.listen_host = info.listen.clone();
    }
    if info.listen.is_none() && info.listen_host.is_some() {
        info.listen = info.listen_host.clone();
    }
    crate::storage::atomic_json_write(path, &info).map_err(DaemonError::Storage)
}

/// Reads the PID metadata, supporting both JSON and legacy integer format.
pub fn read_pid_file(path: &Path) -> Result<DaemonInfo, DaemonError> {
    crate::config::assert_safe_read_path(path)?;
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DaemonError::PIDNotFound);
        }
        Err(e) => return Err(DaemonError::Io(e)),
    };

    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err(DaemonError::PIDMalformed("empty pid file".to_string()));
    }

    if trimmed.starts_with('{') {
        let mut info: DaemonInfo =
            serde_json::from_str(trimmed).map_err(|e| DaemonError::PIDMalformed(format!("{e}")))?;
        if info.pid <= 0 {
            return Err(DaemonError::PIDMalformed(format!("invalid pid {}", info.pid)));
        }
        if info.listen_host.is_none() && info.listen.is_some() {
            info.listen_host = info.listen.clone();
        }
        if info.listen.is_none() && info.listen_host.is_some() {
            info.listen = info.listen_host.clone();
        }
        return Ok(info);
    }

    // Legacy format: raw integer PID
    match trimmed.parse::<i32>() {
        Ok(pid) if pid > 0 => Ok(DaemonInfo {
            pid,
            ..Default::default()
        }),
        _ => Err(DaemonError::PIDMalformed(format!("invalid integer pid {trimmed:?}"))),
    }
}

/// Reports whether a process with the given PID is currently alive (and not a zombie).
pub fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    match super::systemd::send_signal(pid, 0) {
        Ok(()) => {}
        Err(e) => {
            if e.raw_os_error() == Some(libc::EPERM) {
                // Process exists but is owned by another user
                return true;
            }
            return false;
        }
    }

    // On Linux, verify the process is not a zombie (terminated waiting for parent reap)
    let status_path = format!("/proc/{}/status", pid);
    if let Ok(data) = std::fs::read_to_string(status_path) {
        for line in data.lines() {
            if line.starts_with("State:") {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 2 && fields[1] == "Z" {
                    return false;
                }
                break;
            }
        }
    }

    true
}

/// Validates that a live PID belongs to agy-pool, avoiding false positives on PID reuse.
pub fn is_agy_pool_process(pid: i32) -> bool {
    if pid <= 1 || !is_process_alive(pid) {
        return false;
    }

    let cmdline_path = format!("/proc/{}/cmdline", pid);
    let data = match std::fs::read(cmdline_path) {
        Ok(d) => d,
        Err(_) => return true, // On non-Linux or /proc unavailable, assume valid if alive
    };

    let cmd = String::from_utf8_lossy(&data).replace('\0', " ");
    cmd.contains("agy-pool")
        || cmd.contains("agy_pool")
        || cmd.contains("python")
        || cmd.contains(".test")
        || cmd.contains("daemon_child")
}

/// Returns validated DaemonInfo if a live agy-pool daemon is running.
pub fn get_daemon_info(pid_file: Option<&Path>) -> Option<DaemonInfo> {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };

    let info = read_pid_file(&resolved).ok()?;
    if !is_process_alive(info.pid) {
        return None;
    }
    if !is_agy_pool_process(info.pid) {
        return None;
    }
    Some(info)
}

/// Returns the verified daemon PID, or 0 if not running.
pub fn get_daemon_pid(pid_file: Option<&Path>) -> i32 {
    get_daemon_info(pid_file).map(|i| i.pid).unwrap_or(0)
}

/// Unlinks the PID file ONLY if it currently contains expected_pid.
pub fn remove_pid_file_if_owned(path: &Path, expected_pid: i32) -> Result<bool, DaemonError> {
    crate::config::assert_safe_write_path(path)?;

    match read_pid_file(path) {
        Ok(info) => {
            if info.pid != expected_pid {
                return Err(DaemonError::PIDNotOwned(format!(
                    "pid file contains pid {}, expected {}",
                    info.pid, expected_pid
                )));
            }
        }
        Err(DaemonError::PIDNotFound) => return Ok(true),
        Err(DaemonError::PIDMalformed(msg)) => {
            if expected_pid <= 0 {
                let _ = std::fs::remove_file(path);
                return Ok(true);
            }
            return Err(DaemonError::PIDMalformed(msg));
        }
        Err(e) => return Err(e),
    }

    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(DaemonError::Io(e));
    }
    Ok(true)
}

/// Attempts to acquire an exclusive, non-blocking file lock on pid_file + ".lock".
pub fn try_acquire_pid_lock(pid_file: &Path) -> Result<PidLock, DaemonError> {
    crate::config::assert_safe_write_path(pid_file)?;

    let lock_path = PathBuf::from(format!("{}.lock", pid_file.display()));
    crate::config::assert_safe_write_path(&lock_path)?;

    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
            return Err(DaemonError::AlreadyRunning);
        }
        return Err(DaemonError::Io(err));
    }

    Ok(PidLock { file, lock_path })
}

/// Checks if an existing running daemon matches the requested configuration, data dir, and port.
pub fn check_daemon_mismatch(
    pid_file: Option<&Path>,
    expected_port: u16,
    expected_config_path: Option<&Path>,
) -> String {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };

    let info = match get_daemon_info(Some(&resolved)) {
        Some(i) if i.pid > 0 => i,
        _ => return String::new(),
    };

    if !is_process_alive(info.pid) {
        return String::new();
    }

    // 1. Port mismatch
    if expected_port > 0 && info.port.is_some() && info.port != Some(expected_port) {
        return format!(
            "daemon port mismatch: running on port {}, requested {}",
            info.port.unwrap_or(0),
            expected_port
        );
    }

    // 2. Data directory mismatch
    let curr_data_dir = crate::config::data_dir();
    if let Some(info_dir) = &info.data_dir
        && !info_dir.is_empty()
    {
        let info_cleaned = crate::config::clean_path(Path::new(info_dir));
        let curr_cleaned = crate::config::clean_path(&curr_data_dir);
        if info_cleaned != curr_cleaned {
            return format!(
                "daemon data directory mismatch: running with \"{}\", requested \"{}\"",
                info_dir,
                curr_data_dir.display()
            );
        }
    }

    // 3. Config path mismatch
    if let Some(exp_cfg) = expected_config_path
        && let Some(info_cfg) = &info.config_path
    {
        let exp_cleaned = crate::config::clean_path(exp_cfg);
        let info_cleaned = crate::config::clean_path(Path::new(info_cfg));
        if exp_cleaned != info_cleaned {
            return format!(
                "daemon config mismatch: running with \"{}\", requested \"{}\"",
                info_cfg,
                exp_cfg.display()
            );
        }
        // 4. Config hash mismatch
        if let Some(info_hash) = &info.config_hash
            && let Ok(curr_hash) = crate::config::compute_config_file_hash(exp_cfg)
            && curr_hash != *info_hash
        {
            return format!(
                "daemon config file \"{}\" modified since start (hash mismatch)",
                exp_cfg.display()
            );
        }
    }

    // 5. Version mismatch
    if let Some(info_ver) = &info.version
        && !info_ver.is_empty()
        && info_ver != crate::VERSION
    {
        return format!(
            "daemon version mismatch: running {}, binary version is {}",
            info_ver,
            crate::VERSION
        );
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_file_write_and_read_roundtrip() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("test.pid");

        let info = DaemonInfo {
            pid: std::process::id() as i32,
            version: Some("0.1.0-test".to_string()),
            script_mtime: Some(1234567890),
            ..Default::default()
        };

        write_pid_file(&pid_file, info.clone()).unwrap();

        let meta = std::fs::metadata(&pid_file).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(&meta) & 0o777,
            0o600,
            "expected permissions 0600"
        );

        let read_info = read_pid_file(&pid_file).unwrap();
        assert_eq!(read_info.pid, info.pid);
        assert_eq!(read_info.version, info.version);
        assert_eq!(read_info.script_mtime, info.script_mtime);
    }

    #[test]
    fn test_pid_file_legacy_format() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("legacy.pid");

        std::fs::write(&pid_file, b"98765\n").unwrap();

        let info = read_pid_file(&pid_file).unwrap();
        assert_eq!(info.pid, 98765);
    }

    #[test]
    fn test_pid_file_malformed_and_missing() {
        let env = crate::daemon::test_env();
        let missing = env.temp_dir.path().join("missing.pid");
        assert!(matches!(read_pid_file(&missing), Err(DaemonError::PIDNotFound)));

        let malformed = env.temp_dir.path().join("malformed.pid");
        std::fs::write(&malformed, b"{not json").unwrap();
        assert!(matches!(read_pid_file(&malformed), Err(DaemonError::PIDMalformed(_))));
    }

    #[test]
    fn test_pid_file_ownership_safe_removal() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("ownership.pid");

        write_pid_file(
            &pid_file,
            DaemonInfo {
                pid: 999999,
                version: Some("test".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        // Attempt removal with our pid should fail closed with ErrPIDNotOwned
        let our_pid = std::process::id() as i32;
        let res = remove_pid_file_if_owned(&pid_file, our_pid);
        assert!(
            matches!(res, Err(DaemonError::PIDNotOwned(_))),
            "expected PIDNotOwned, got: {:?}",
            res
        );
        assert!(pid_file.exists());

        // Removal with matching PID should succeed
        let res = remove_pid_file_if_owned(&pid_file, 999999).unwrap();
        assert!(res);
        assert!(!pid_file.exists());
    }

    #[test]
    fn test_stale_pid_detection() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("stale.pid");

        // Dead pid
        let mut dead_pid = 4194303;
        while is_process_alive(dead_pid) {
            dead_pid -= 1;
        }

        write_pid_file(
            &pid_file,
            DaemonInfo {
                pid: dead_pid,
                version: Some("test".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(get_daemon_info(Some(&pid_file)).is_none());

        // Live pid for current process
        write_pid_file(
            &pid_file,
            DaemonInfo {
                pid: std::process::id() as i32,
                version: Some("test".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let info = get_daemon_info(Some(&pid_file));
        assert!(info.is_some());
        assert_eq!(info.unwrap().pid, std::process::id() as i32);
    }

    #[test]
    fn test_try_acquire_pid_lock() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("lock.pid");

        let lock1 = try_acquire_pid_lock(&pid_file).unwrap();

        // Second acquisition while held must fail with AlreadyRunning
        let lock2_res = try_acquire_pid_lock(&pid_file);
        assert!(matches!(lock2_res, Err(DaemonError::AlreadyRunning)));

        lock1.unlock();

        // Third acquisition after release must succeed
        let lock3 = try_acquire_pid_lock(&pid_file);
        assert!(lock3.is_ok());
    }

    #[test]
    fn test_fail_closed_production_pid_path() {
        let prod_dir = crate::config::real_production_gemini_dir();
        let prod_pid = prod_dir.join("test-should-fail.pid");

        assert!(
            write_pid_file(
                &prod_pid,
                DaemonInfo {
                    pid: std::process::id() as i32,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(try_acquire_pid_lock(&prod_pid).is_err());
        assert!(remove_pid_file_if_owned(&prod_pid, std::process::id() as i32).is_err());
    }

    #[test]
    fn test_is_process_alive_esrch() {
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(-1));
        assert!(is_process_alive(1));
    }

    #[test]
    fn test_is_agy_pool_process_pid1() {
        if Path::new("/proc/1/cmdline").exists() {
            assert!(!is_agy_pool_process(1));
        }
    }
}
