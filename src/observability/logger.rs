use std::io::Write;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    pub fn parse_lossy(s: &str) -> Self {
        let trimmed = s.trim().to_ascii_uppercase();
        match trimmed.as_str() {
            "DEBUG" => LogLevel::Debug,
            "WARN" | "WARNING" => LogLevel::Warn,
            "ERROR" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }
}

impl From<&str> for LogLevel {
    fn from(s: &str) -> Self {
        Self::parse_lossy(s)
    }
}

impl FromStr for LogLevel {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse_lossy(s))
    }
}

static CURRENT_LOG_LEVEL: AtomicUsize = AtomicUsize::new(LogLevel::Info as usize);

pub fn set_log_level(level: LogLevel) {
    CURRENT_LOG_LEVEL.store(level as usize, Ordering::Relaxed);
}

pub fn get_log_level() -> LogLevel {
    match CURRENT_LOG_LEVEL.load(Ordering::Relaxed) {
        0 => LogLevel::Debug,
        1 => LogLevel::Info,
        2 => LogLevel::Warn,
        _ => LogLevel::Error,
    }
}

enum LogCommand {
    Line(String),
    Flush(SyncSender<()>),
    Shutdown(SyncSender<()>),
}

struct LoggerChannel {
    tx: SyncSender<LogCommand>,
    _worker: JoinHandle<()>,
}

pub type DynWriter = Box<dyn Write + Send + Sync>;

static ACTIVE_LOGGER: LazyLock<Mutex<Option<LoggerChannel>>> = LazyLock::new(|| Mutex::new(None));
static TEST_WRITER: LazyLock<Mutex<Option<DynWriter>>> = LazyLock::new(|| Mutex::new(None));

pub struct LogWriterGuard;
impl Drop for LogWriterGuard {
    fn drop(&mut self) {
        *TEST_WRITER.lock().unwrap() = None;
    }
}

pub fn set_log_writer_for_test(w: DynWriter) -> LogWriterGuard {
    *TEST_WRITER.lock().unwrap() = Some(w);
    LogWriterGuard
}

pub fn init_async_logger_with_writer(mut writer: Box<dyn Write + Send>, level: LogLevel) -> LoggerGuard {
    set_log_level(level);

    let (tx, rx) = sync_channel::<LogCommand>(2048);

    let worker = thread::Builder::new()
        .name("agy-logger".to_string())
        .spawn(move || {
            run_logger_loop(rx, &mut writer);
        })
        .expect("failed to spawn agy-logger worker thread");

    let mut guard = ACTIVE_LOGGER.lock().unwrap();
    if let Some(old) = guard.take() {
        let (done_tx, done_rx) = sync_channel(1);
        let _ = old.tx.send(LogCommand::Shutdown(done_tx));
        let _ = done_rx.recv_timeout(Duration::from_millis(500));
    }
    *guard = Some(LoggerChannel { tx, _worker: worker });

    LoggerGuard
}

pub struct LoggerGuard;
impl Drop for LoggerGuard {
    fn drop(&mut self) {
        set_log_level(LogLevel::Info);
    }
}

fn run_logger_loop(rx: Receiver<LogCommand>, writer: &mut Box<dyn Write + Send>) {
    let mut batch = Vec::with_capacity(64);
    loop {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(LogCommand::Line(line)) => {
                batch.push(line);
                while batch.len() < 64 {
                    match rx.try_recv() {
                        Ok(LogCommand::Line(l)) => batch.push(l),
                        Ok(LogCommand::Flush(done)) => {
                            flush_batch(&mut batch, writer);
                            let _ = done.send(());
                        }
                        Ok(LogCommand::Shutdown(done)) => {
                            flush_batch(&mut batch, writer);
                            let _ = done.send(());
                            return;
                        }
                        Err(_) => break,
                    }
                }
                flush_batch(&mut batch, writer);
            }
            Ok(LogCommand::Flush(done)) => {
                flush_batch(&mut batch, writer);
                let _ = done.send(());
            }
            Ok(LogCommand::Shutdown(done)) => {
                flush_batch(&mut batch, writer);
                let _ = done.send(());
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if !batch.empty() {
                    flush_batch(&mut batch, writer);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                flush_batch(&mut batch, writer);
                return;
            }
        }
    }
}

trait VecExt {
    fn empty(&self) -> bool;
}

impl<T> VecExt for Vec<T> {
    fn empty(&self) -> bool {
        self.is_empty()
    }
}

fn flush_batch(batch: &mut Vec<String>, writer: &mut Box<dyn Write + Send>) {
    if batch.is_empty() {
        return;
    }
    for line in batch.drain(..) {
        let _ = writer.write_all(line.as_bytes());
    }
    let _ = writer.flush();
}

pub fn log_message_level(level: LogLevel, prefix: &str, msg: &str) {
    let now_str = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S");
    let line = format!("[{now_str}] [{}] [{prefix}] {msg}\n", level.as_str());

    // 1. If test writer is installed, synchronous write directly
    {
        let mut test_guard = TEST_WRITER.lock().unwrap();
        if let Some(w) = test_guard.as_mut() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
            return;
        }
    }

    if level < get_log_level() {
        return;
    }

    // 2. Otherwise route to async logger if active
    let logger_guard = ACTIVE_LOGGER.lock().unwrap();
    if let Some(logger) = logger_guard.as_ref() {
        if let Err(std::sync::mpsc::TrySendError::Full(_)) = logger.tx.try_send(LogCommand::Line(line.clone())) {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    } else {
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

pub fn flush_logs_sync_for_test() {
    let mut test_guard = TEST_WRITER.lock().unwrap();
    if let Some(w) = test_guard.as_mut() {
        let _ = w.flush();
        return;
    }
    drop(test_guard);

    let guard = ACTIVE_LOGGER.lock().unwrap();
    if let Some(logger) = guard.as_ref() {
        let (tx, rx) = sync_channel(1);
        if logger.tx.send(LogCommand::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_log_level_ordering_and_filtering() {
        assert!(LogLevel::Info >= LogLevel::Debug);
        assert!(LogLevel::Error >= LogLevel::Warn);
        assert_eq!(LogLevel::from_str("warn").unwrap(), LogLevel::Warn);
        assert_eq!(LogLevel::from_str("UNKNOWN").unwrap(), LogLevel::Info);
    }

    #[test]
    fn test_async_logger_writes_and_filters() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = TestWriter(buf.clone());
        let _handle = init_async_logger_with_writer(Box::new(writer), LogLevel::Warn);

        log_message_level(LogLevel::Info, "TEST", "should be dropped");
        log_message_level(LogLevel::Warn, "WARN", "warning line");
        log_message_level(LogLevel::Error, "ERR", "error line");

        flush_logs_sync_for_test();
        let content = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(!content.contains("should be dropped"));
        assert!(content.contains("[WARN] [WARN] warning line"));
        assert!(content.contains("[ERROR] [ERR] error line"));
    }
}
