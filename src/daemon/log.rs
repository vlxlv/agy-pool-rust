use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::DaemonError;

pub const LOG_ROTATE_INTERVAL: Duration = Duration::from_secs(30);

static LAST_LOG_ROTATE_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

/// Formats a byte count into a human-readable string matching Python reference.
pub fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Rotates log file if it exceeds max_bytes (or force=true), using copytruncate semantics.
pub fn rotate_log_if_needed(
    log_path: Option<&Path>,
    mut max_bytes: i64,
    mut backup_count: i32,
    force: bool,
) -> Result<bool, DaemonError> {
    let resolved_path = match log_path {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::log_file(),
    };

    if max_bytes <= 0 {
        max_bytes = crate::config::DEFAULT_MAX_LOG_BYTES;
    }
    if backup_count < 0 {
        backup_count = crate::config::DEFAULT_BACKUP_LOG_COUNT as i32;
    }

    crate::config::assert_safe_write_path(&resolved_path)?;

    if !resolved_path.exists() {
        return Ok(false);
    }

    let meta = std::fs::metadata(&resolved_path)?;
    if !force && (meta.len() as i64) < max_bytes {
        return Ok(false);
    }

    let lock_file = PathBuf::from(format!("{}.lock", resolved_path.display()));
    crate::storage::with_file_lock(&lock_file, true, || {
        let meta = match std::fs::metadata(&resolved_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(crate::storage::StorageError::Io(e)),
        };

        if !force && (meta.len() as i64) < max_bytes {
            return Ok(false);
        }

        // 1. Shift existing backups: e.g. .2 -> .3, .1 -> .2
        for i in (1..backup_count).rev() {
            let src = format!("{}.{}", resolved_path.display(), i);
            let dst = format!("{}.{}", resolved_path.display(), i + 1);
            if Path::new(&src).exists() {
                let _ = std::fs::rename(&src, &dst);
            }
        }

        // 2. Copy active log to .1
        if backup_count > 0 {
            let backup_dst = format!("{}.1", resolved_path.display());
            if let Some(parent) = Path::new(&backup_dst).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&resolved_path, &backup_dst)?;
        }

        // 3. In-place truncation preserving open file descriptors (copytruncate)
        let file = OpenOptions::new().read(true).write(true).open(&resolved_path)?;
        file.set_len(0)?;
        file.sync_all()?;

        Ok(true)
    })
    .map_err(DaemonError::Storage)
}

/// Truncates the active log file and removes backup logs.
pub fn clear_log(log_path: Option<&Path>, mut backup_count: i32) -> Result<(), DaemonError> {
    let resolved_path = match log_path {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::log_file(),
    };

    if backup_count < 0 {
        backup_count = crate::config::DEFAULT_BACKUP_LOG_COUNT as i32;
    }

    crate::config::assert_safe_write_path(&resolved_path)?;

    let lock_file = PathBuf::from(format!("{}.lock", resolved_path.display()));
    crate::storage::with_file_lock(&lock_file, true, || {
        if resolved_path.exists()
            && let Ok(file) = OpenOptions::new().write(true).open(&resolved_path)
        {
            let _ = file.set_len(0);
            let _ = file.sync_all();
        }

        for i in 1..=(backup_count + 2) {
            let b_path = format!("{}.{}", resolved_path.display(), i);
            let _ = std::fs::remove_file(b_path);
        }
        Ok(())
    })
    .map_err(DaemonError::Storage)
}

/// Rate-limits log rotation checks to at most once every LogRotateInterval.
pub fn maybe_rotate_log() -> bool {
    let mut check_guard = LAST_LOG_ROTATE_CHECK.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if let Some(last) = *check_guard
        && now.duration_since(last) < LOG_ROTATE_INTERVAL
    {
        return false;
    }
    *check_guard = Some(now);
    drop(check_guard);

    rotate_log_if_needed(None, 0, -1, false).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn test_format_size() {
        let cases = [
            (500, "500 B"),
            (1023, "1023 B"),
            (1024, "1.0 KB"),
            (2048, "2.0 KB"),
            (1536, "1.5 KB"),
            (1048576, "1.0 MB"),
            (5242880, "5.0 MB"),
        ];

        for (bytes, want) in cases {
            let got = format_size(bytes);
            assert_eq!(got, want, "format_size({}) = {}, want {}", bytes, got, want);
        }
    }

    #[test]
    fn test_rotate_log_if_needed_copytruncate_and_shift() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.log");

        // 1. Nonexistent file
        let rotated = rotate_log_if_needed(Some(&log_path), 100, 2, false).unwrap();
        assert!(!rotated, "nonexistent file should not rotate");

        // 2. Open file for writing to test copytruncate preserving open descriptor
        let mut writer = OpenOptions::new().create(true).append(true).open(&log_path).unwrap();
        writer.write_all(b"first generation log line\n").unwrap();
        writer.flush().unwrap();

        // 3. Below threshold: no rotation
        let rotated = rotate_log_if_needed(Some(&log_path), 1000, 2, false).unwrap();
        assert!(!rotated, "below threshold should not rotate");

        // 4. Force rotate: should rotate and copytruncate
        let rotated = rotate_log_if_needed(Some(&log_path), 1000, 2, true).unwrap();
        assert!(rotated, "force rotate should succeed");

        // Verify backup .1 exists and has first line
        let b1_path = format!("{}.1", log_path.display());
        let b1_content = std::fs::read_to_string(&b1_path).unwrap();
        assert!(
            b1_content.contains("first generation"),
            "backup .1 missing original content: {}",
            b1_content
        );

        // Verify active log was truncated to 0
        let meta = std::fs::metadata(&log_path).unwrap();
        assert_eq!(meta.len(), 0, "active log should be truncated to 0");

        // 5. CRITICAL: Write to original open writer without reopening
        writer
            .write_all(b"second generation log line after rotation\n")
            .unwrap();
        writer.flush().unwrap();

        // Verify active log has the new content
        let active_content = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            active_content.contains("second generation"),
            "active log missing content written to open writer: {}",
            active_content
        );

        // 6. Rotate again: .1 should shift to .2, and new .1 created
        let rotated = rotate_log_if_needed(Some(&log_path), 10, 2, true).unwrap();
        assert!(rotated, "second rotation should succeed");

        let b2_path = format!("{}.2", log_path.display());
        let b2_content = std::fs::read_to_string(&b2_path).unwrap();
        assert!(
            b2_content.contains("first generation"),
            "backup .2 should contain first generation data"
        );

        let b1_content = std::fs::read_to_string(&b1_path).unwrap();
        assert!(
            b1_content.contains("second generation"),
            "backup .1 should contain second generation data"
        );
    }

    #[test]
    fn test_clear_log_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("clear_test.log");

        std::fs::write(&log_path, b"active data\n").unwrap();
        std::fs::write(format!("{}.1", log_path.display()), b"backup 1\n").unwrap();
        std::fs::write(format!("{}.2", log_path.display()), b"backup 2\n").unwrap();

        let mut writer = OpenOptions::new().append(true).open(&log_path).unwrap();

        clear_log(Some(&log_path), 2).unwrap();

        assert!(!Path::new(&format!("{}.1", log_path.display())).exists());
        assert!(!Path::new(&format!("{}.2", log_path.display())).exists());

        let meta = std::fs::metadata(&log_path).unwrap();
        assert_eq!(meta.len(), 0, "active log should be size 0 after clear");

        writer.write_all(b"written after clear\n").unwrap();
        writer.flush().unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("written after clear"), "log not writable after clear");
    }

    #[test]
    fn test_fail_closed_production_log_path() {
        let prod_dir = crate::config::real_production_gemini_dir();
        let prod_log = prod_dir.join("test.log");

        assert!(rotate_log_if_needed(Some(&prod_log), 100, 1, true).is_err());
        assert!(clear_log(Some(&prod_log), 1).is_err());
    }

    #[test]
    fn test_maybe_rotate_log_rate_limiting() {
        // maybe_rotate_log resolves the default log path from the data dir; never let it see the live one.
        let _env = crate::daemon::test_env();
        let _ = maybe_rotate_log();

        // Second immediate call must be rate-limited and return false
        assert!(!maybe_rotate_log(), "immediate second call should be rate limited");

        // Reset timer
        let mut guard = LAST_LOG_ROTATE_CHECK.lock().unwrap();
        *guard = Some(Instant::now() - (LOG_ROTATE_INTERVAL * 2));
        drop(guard);

        // Call after interval executes
        let _ = maybe_rotate_log();
    }
}
