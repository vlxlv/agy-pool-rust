use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditTokens {
    pub prompt: i64,
    pub cached: i64,
    #[serde(default)]
    pub output: i64,
    #[serde(default)]
    pub thinking: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q5: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q7: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r5_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r7_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: String,
    pub request_id: String,
    pub session_key: String,
    pub method: String,
    pub path: String,
    pub model: String,
    pub strategy: String,
    pub attempt: usize,
    pub account_id: String,
    pub account_name: String,
    pub status_code: u16,
    pub ttfb_ms: u64,
    pub total_ms: u64,
    pub tokens: AuditTokens,
    pub quota_before: AuditQuota,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub failover_reason: String,
}

enum AuditCommand {
    Event(Box<AuditEvent>),
    Flush(SyncSender<()>),
    Shutdown(SyncSender<()>),
}

struct AuditChannel {
    tx: SyncSender<AuditCommand>,
    _worker: JoinHandle<()>,
}

static ACTIVE_AUDIT: LazyLock<Mutex<Option<AuditChannel>>> = LazyLock::new(|| Mutex::new(None));
static AUDIT_DROPPED: AtomicU64 = AtomicU64::new(0);

pub struct AuditHandle;

impl Drop for AuditHandle {
    fn drop(&mut self) {
        shutdown_audit_worker();
    }
}

pub fn shutdown_audit_worker() {
    let mut guard = ACTIVE_AUDIT.lock().unwrap();
    if let Some(chan) = guard.take() {
        let (done_tx, done_rx) = sync_channel(1);
        if chan.tx.send(AuditCommand::Shutdown(done_tx)).is_ok() {
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

pub fn audit_dropped_count() -> u64 {
    AUDIT_DROPPED.load(Ordering::Relaxed)
}

pub fn is_audit_worker_active() -> bool {
    ACTIVE_AUDIT.lock().unwrap().is_some()
}

pub fn record_audit_event(event: AuditEvent) {
    let guard = ACTIVE_AUDIT.lock().unwrap();
    if let Some(chan) = guard.as_ref()
        && chan.tx.try_send(AuditCommand::Event(Box::new(event))).is_err()
    {
        AUDIT_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn init_audit_worker(path: PathBuf, max_bytes: u64, backup_count: i32) -> AuditHandle {
    let (tx, rx) = sync_channel::<AuditCommand>(4096);

    let worker = thread::Builder::new()
        .name("agy-audit".to_string())
        .spawn(move || {
            run_audit_loop(rx, path, max_bytes, backup_count);
        })
        .expect("failed to spawn agy-audit worker thread");

    let mut guard = ACTIVE_AUDIT.lock().unwrap();
    if let Some(old) = guard.take() {
        let (done_tx, done_rx) = sync_channel(1);
        let _ = old.tx.send(AuditCommand::Shutdown(done_tx));
        let _ = done_rx.recv_timeout(Duration::from_millis(500));
    }
    *guard = Some(AuditChannel { tx, _worker: worker });

    AuditHandle
}

fn run_audit_loop(rx: Receiver<AuditCommand>, path: PathBuf, max_bytes: u64, backup_count: i32) {
    let mut batch = Vec::with_capacity(128);

    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(AuditCommand::Event(ev)) => {
                batch.push(*ev);
                while batch.len() < 128 {
                    match rx.try_recv() {
                        Ok(AuditCommand::Event(e)) => batch.push(*e),
                        Ok(AuditCommand::Flush(done)) => {
                            write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                            let _ = done.send(());
                        }
                        Ok(AuditCommand::Shutdown(done)) => {
                            write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                            let _ = done.send(());
                            return;
                        }
                        Err(_) => break,
                    }
                }
                write_audit_batch(&mut batch, &path, max_bytes, backup_count);
            }
            Ok(AuditCommand::Flush(done)) => {
                write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                let _ = done.send(());
            }
            Ok(AuditCommand::Shutdown(done)) => {
                write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                let _ = done.send(());
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                write_audit_batch(&mut batch, &path, max_bytes, backup_count);
                return;
            }
        }
    }
}

fn write_audit_batch(batch: &mut Vec<AuditEvent>, path: &Path, max_bytes: u64, backup_count: i32) {
    if batch.is_empty() {
        return;
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Check if rotation needed before write
    if max_bytes > 0 && backup_count > 0 {
        let _ = crate::daemon::rotate_log_if_needed(Some(path), max_bytes as i64, backup_count, false);
    }

    let mut file = match OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
        Ok(f) => f,
        Err(e) => {
            AUDIT_DROPPED.fetch_add(batch.len() as u64, Ordering::Relaxed);
            eprintln!("warning: failed to open audit file {}: {}", path.display(), e);
            batch.clear();
            return;
        }
    };

    let count = batch.len();
    let mut buf = Vec::new();
    for ev in batch.drain(..) {
        if let Ok(line) = serde_json::to_string(&ev) {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
    }
    if let Err(e) = file.write_all(&buf) {
        AUDIT_DROPPED.fetch_add(count as u64, Ordering::Relaxed);
        eprintln!("warning: failed to write audit batch to {}: {}", path.display(), e);
    } else {
        let _ = file.flush();
    }
}

pub fn flush_audit_sync() {
    let guard = ACTIVE_AUDIT.lock().unwrap();
    if let Some(chan) = guard.as_ref() {
        let (tx, rx) = sync_channel(1);
        if chan.tx.send(AuditCommand::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_event_serialization_and_drop() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit-events.jsonl");
        let _handle = init_audit_worker(audit_path.clone(), 50000, 1);

        let event = AuditEvent {
            timestamp: jiff::Timestamp::now().to_string(),
            request_id: "req-123".to_string(),
            session_key: "sess-abc".to_string(),
            method: "POST".to_string(),
            path: "/v1internal:streamGenerateContent".to_string(),
            model: "gemini-2.5-pro".to_string(),
            strategy: "smart".to_string(),
            attempt: 1,
            account_id: "acc-1".to_string(),
            account_name: "Main".to_string(),
            status_code: 200,
            ttfb_ms: 120,
            total_ms: 850,
            tokens: AuditTokens {
                prompt: 100,
                cached: 50,
                output: 25,
                thinking: 10,
            },
            quota_before: AuditQuota {
                q5: Some(0.85),
                q7: Some(0.92),
                r5_secs: Some(3600),
                r7_secs: Some(86400),
            },
            failover_reason: String::new(),
        };

        record_audit_event(event);
        flush_audit_sync();

        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(content.contains("\"request_id\":\"req-123\""));
        assert!(content.contains("\"model\":\"gemini-2.5-pro\""));
        assert!(content.contains("\"cached\":50"));
        assert_eq!(audit_dropped_count(), 0);
    }

    #[test]
    fn test_audit_rotation() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit-events.jsonl");
        let _handle = init_audit_worker(audit_path.clone(), 150, 1);

        let event = AuditEvent {
            timestamp: jiff::Timestamp::now().to_string(),
            request_id: "req-rot-1".to_string(),
            session_key: "sess-1".to_string(),
            method: "POST".to_string(),
            path: "/path1".to_string(),
            model: "m1".to_string(),
            strategy: "s1".to_string(),
            attempt: 1,
            account_id: "a1".to_string(),
            account_name: "A1".to_string(),
            status_code: 200,
            ttfb_ms: 10,
            total_ms: 20,
            tokens: AuditTokens {
                prompt: 1,
                cached: 0,
                output: 0,
                thinking: 0,
            },
            quota_before: AuditQuota {
                q5: None,
                q7: None,
                r5_secs: None,
                r7_secs: None,
            },
            failover_reason: String::new(),
        };

        record_audit_event(event.clone());
        flush_audit_sync();

        // Write second event exceeding 150 bytes limit
        let mut event2 = event.clone();
        event2.request_id = "req-rot-2".to_string();
        record_audit_event(event2);
        flush_audit_sync();

        let backup_path = dir.path().join("audit-events.jsonl.1");
        assert!(
            backup_path.exists(),
            "backup file audit-events.jsonl.1 should exist after rotation"
        );
        assert!(audit_path.exists(), "active file should still exist");
    }
}
