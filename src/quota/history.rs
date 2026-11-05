use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::config;
use crate::storage::{QuotaState, QuotaWindow};

/// QuotaHistoryFile is the JSONL log of Gemini quota changes in the data dir.
pub const QUOTA_HISTORY_FILE: &str = "quota-history.jsonl";

pub const DEFAULT_MAX_QUOTA_HISTORY_BYTES: u64 = 5 << 20; // 5 MB

static MAX_QUOTA_HISTORY_BYTES: RwLock<Option<u64>> = RwLock::new(None);

pub struct MaxQuotaHistoryBytesGuard(Option<u64>);

impl Drop for MaxQuotaHistoryBytesGuard {
    fn drop(&mut self) {
        *MAX_QUOTA_HISTORY_BYTES.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_max_quota_history_bytes_for_test(bytes: u64) -> MaxQuotaHistoryBytesGuard {
    let mut w = MAX_QUOTA_HISTORY_BYTES.write().unwrap_or_else(|e| e.into_inner());
    let old = w.replace(bytes);
    MaxQuotaHistoryBytesGuard(old)
}

fn get_max_quota_history_bytes() -> u64 {
    MAX_QUOTA_HISTORY_BYTES
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or(DEFAULT_MAX_QUOTA_HISTORY_BYTES)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotaHistoryRecord {
    pub account: String,
    pub g5: f64,
    pub g5_reset: Option<serde_json::Value>,
    pub gw: f64,
    pub gw_reset: Option<serde_json::Value>,
    pub ts: i64,
}

fn format_reset_value(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => "<nil>".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn same_window(a: Option<&QuotaWindow>, b: Option<&QuotaWindow>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    let (Some(fa), Some(fb)) = (a.fraction, b.fraction) else {
        return false;
    };
    if (fa - fb).abs() > 1e-9 {
        return false;
    }
    format_reset_value(a.reset_time.as_ref()) == format_reset_value(b.reset_time.as_ref())
}

/// append_quota_history records a quota-summary snapshot when it differs from the previous one.
pub fn append_quota_history(account_id: &str, prev: Option<&QuotaState>, cur: Option<&QuotaState>) {
    let Some(cur) = cur else {
        return;
    };
    let (Some(g5), Some(gw)) = (&cur.gemini_5h, &cur.gemini_weekly) else {
        return;
    };
    let (Some(frac5), Some(fracw)) = (g5.fraction, gw.fraction) else {
        return;
    };

    if let Some(prev) = prev
        && same_window(prev.gemini_5h.as_ref(), Some(g5))
        && same_window(prev.gemini_weekly.as_ref(), Some(gw))
    {
        return;
    }

    let record = QuotaHistoryRecord {
        account: account_id.to_string(),
        g5: frac5,
        g5_reset: g5.reset_time.clone(),
        gw: fracw,
        gw_reset: gw.reset_time.clone(),
        ts: crate::clock::now_unix() as i64,
    };

    let line = match serde_json::to_string(&record) {
        Ok(l) => l,
        Err(_) => return,
    };

    let path = config::data_dir().join(QUOTA_HISTORY_FILE);
    let max_bytes = get_max_quota_history_bytes();

    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() >= max_bytes
    {
        let lock_path_str = format!("{}.lock", path.display());
        let _ = crate::storage::with_file_lock(Path::new(&lock_path_str), true, || {
            if let Ok(cur_meta) = std::fs::metadata(&path)
                && cur_meta.len() >= max_bytes
            {
                let rot_path_str = format!("{}.1", path.display());
                let _ = std::fs::rename(&path, PathBuf::from(rot_path_str));
            }
            Ok(())
        });
    }

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(0o600);

    if let Ok(mut f) = opts.open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{}", line);
    }
}

/// read_quota_history reads all history records from the given or default path.
pub fn read_quota_history(path: Option<&Path>) -> Result<Vec<QuotaHistoryRecord>, std::io::Error> {
    let p = path
        .map(PathBuf::from)
        .unwrap_or_else(|| config::data_dir().join(QUOTA_HISTORY_FILE));
    let content = std::fs::read_to_string(p)?;
    let mut records = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && let Ok(rec) = serde_json::from_str::<QuotaHistoryRecord>(trimmed)
        {
            records.push(rec);
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_quota_test_dir() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        TestEnv { _lock: lock, dir }
    }

    #[test]
    fn test_append_quota_history() {
        let env = setup_quota_test_dir();

        let snap = |g5: f64, gw: f64| -> QuotaState {
            QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(g5),
                    reset_time: Some(serde_json::Value::String("2030-01-01T05:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(gw),
                    reset_time: Some(serde_json::Value::String("2030-01-07T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };

        append_quota_history("acc-1", None, Some(&snap(0.9, 0.8)));
        append_quota_history("acc-1", Some(&snap(0.9, 0.8)), Some(&snap(0.9, 0.8))); // unchanged: skipped
        append_quota_history("acc-1", Some(&snap(0.9, 0.8)), Some(&snap(0.85, 0.79))); // changed: recorded
        let no_weekly = QuotaState {
            gemini_5h: snap(1.0, 1.0).gemini_5h,
            ..Default::default()
        };
        append_quota_history("acc-1", None, Some(&no_weekly)); // no weekly: skipped

        let file_path = env.dir.path().join(QUOTA_HISTORY_FILE);
        let b = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = b.trim().lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines, got {}:\n{}", lines.len(), b);
        assert!(
            lines[1].contains(r#""g5":0.85"#)
                && lines[1].contains(r#""gw":0.79"#)
                && lines[1].contains(r#""gw_reset":"2030-01-07T00:00:00Z""#),
            "unexpected second line: {}",
            lines[1]
        );
    }

    #[test]
    fn test_append_quota_history_rotation() {
        let env = setup_quota_test_dir();
        let _rg = set_max_quota_history_bytes_for_test(200);

        let snap = |g5: f64, gw: f64| -> QuotaState {
            QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(g5),
                    reset_time: Some(serde_json::Value::String("2030-01-01T05:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(gw),
                    reset_time: Some(serde_json::Value::String("2030-01-07T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };

        let mut prev: Option<QuotaState> = None;
        for i in 0..5 {
            let cur = snap(0.9 - (i as f64) * 0.01, 0.8 - (i as f64) * 0.01);
            append_quota_history("acc-1", prev.as_ref(), Some(&cur));
            prev = Some(cur);
        }

        let rot_path = env.dir.path().join(format!("{}.1", QUOTA_HISTORY_FILE));
        let live_path = env.dir.path().join(QUOTA_HISTORY_FILE);

        let rot_meta = std::fs::metadata(&rot_path).expect("expected .1 rotation file to exist");
        assert!(
            rot_meta.len() >= 200,
            ".1 file size {} is less than cap 200",
            rot_meta.len()
        );

        let live_meta = std::fs::metadata(&live_path).expect("expected live history file to exist");
        assert!(
            live_meta.len() < 200,
            "expected live file size {} to be smaller than cap 200",
            live_meta.len()
        );
    }

    #[test]
    fn test_quota_history_byte_for_byte_matches_go() {
        // Step 4 cross-check:
        // Expected line produced by Go's TestZZHistory in internal/quota/zz_history_test.go:
        // {"account":"acc-1","g5":0.85,"g5_reset":"2030-01-01T05:00:00Z","gw":0.79,"gw_reset":"2030-01-07T00:00:00Z","ts":1700000000}
        let record = QuotaHistoryRecord {
            account: "acc-1".to_string(),
            g5: 0.85,
            g5_reset: Some(serde_json::Value::String("2030-01-01T05:00:00Z".to_string())),
            gw: 0.79,
            gw_reset: Some(serde_json::Value::String("2030-01-07T00:00:00Z".to_string())),
            ts: 1700000000,
        };

        let json_line = serde_json::to_string(&record).unwrap();
        let expected = r#"{"account":"acc-1","g5":0.85,"g5_reset":"2030-01-01T05:00:00Z","gw":0.79,"gw_reset":"2030-01-07T00:00:00Z","ts":1700000000}"#;
        assert_eq!(json_line, expected);
    }
}
