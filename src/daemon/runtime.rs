use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::DaemonError;

pub const STALE_RUNTIME_THRESHOLD: Duration = Duration::from_secs(60);
pub const RUNTIME_STATS_FLUSH_THROTTLE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStats {
    pub pid: i32,
    pub started_at: jiff::Timestamp,
    pub updated_at: jiff::Timestamp,
    pub goroutines: usize,
    pub alloc_bytes: u64,
    pub heap_bytes: u64,
    pub sys_bytes: u64,
    pub num_gc: u32,
    pub go_version: String,
    pub in_flight: Option<HashMap<String, i64>>,
    pub model_pauses: Option<HashMap<String, HashMap<String, jiff::Timestamp>>>,
    pub last_model: Option<String>,
    pub smart_rr_cursor: Option<u64>,
    pub snapshot: crate::observability::Snapshot,
}

impl Serialize for RuntimeStats {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("pid", &self.pid)?;
        let started_at_str = self.started_at.to_string();
        let updated_at_str = self.updated_at.to_string();
        map.serialize_entry("started_at", &started_at_str)?;
        map.serialize_entry("updated_at", &updated_at_str)?;
        map.serialize_entry("goroutines", &self.goroutines)?;
        map.serialize_entry("alloc_bytes", &self.alloc_bytes)?;
        map.serialize_entry("heap_bytes", &self.heap_bytes)?;
        map.serialize_entry("sys_bytes", &self.sys_bytes)?;
        map.serialize_entry("num_gc", &self.num_gc)?;
        map.serialize_entry("go_version", &self.go_version)?;

        if let Some(if_map) = &self.in_flight {
            map.serialize_entry("in_flight", if_map)?;
        }
        if let Some(pauses) = &self.model_pauses {
            let mut formatted: HashMap<&str, HashMap<&str, String>> = HashMap::new();
            for (acc, models) in pauses {
                let mut inner = HashMap::new();
                for (model, ts) in models {
                    inner.insert(model.as_str(), ts.to_string());
                }
                formatted.insert(acc.as_str(), inner);
            }
            map.serialize_entry("model_pauses", &formatted)?;
        }
        if let Some(lm) = &self.last_model {
            map.serialize_entry("last_model", lm)?;
        }
        if let Some(cur) = &self.smart_rr_cursor {
            map.serialize_entry("smart_rr_cursor", cur)?;
        }

        // Flatten snapshot fields matching Go JSON
        let snap = &self.snapshot;
        map.serialize_entry("generation_requests_total", &snap.generation_requests)?;
        map.serialize_entry("generation_success_total", &snap.generation_success)?;
        map.serialize_entry("routing_decisions_total", &snap.routing_decisions)?;
        map.serialize_entry("failover_attempts_total", &snap.failover_attempts)?;
        map.serialize_entry("failover_success_total", &snap.failover_success)?;
        map.serialize_entry("restricted_account_skips_total", &snap.restricted_skips)?;
        map.serialize_entry("quota_refresh_attempts_total", &snap.quota_refresh_attempts)?;
        map.serialize_entry("quota_refresh_success_total", &snap.quota_refresh_success)?;
        map.serialize_entry("quota_refresh_failure_total", &snap.quota_refresh_failure)?;
        map.serialize_entry("auth_refresh_attempts_total", &snap.auth_refresh_attempts)?;
        map.serialize_entry("auth_refresh_success_total", &snap.auth_refresh_success)?;
        map.serialize_entry("auth_refresh_failure_total", &snap.auth_refresh_failure)?;
        map.serialize_entry("persistence_errors_total", &snap.persistence_errors)?;
        map.serialize_entry("no_replay_prevented_total", &snap.no_replay_prevented)?;
        map.serialize_entry("lease_fallbacks_total", &snap.lease_fallbacks)?;

        map.end()
    }
}

impl<'de> Deserialize<'de> for RuntimeStats {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        let obj = val
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected object for RuntimeStats"))?;

        let pid = obj
            .get("pid")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| serde::de::Error::custom("missing or invalid pid"))? as i32;

        let started_at: jiff::Timestamp = obj
            .get("started_at")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| serde::de::Error::custom("missing or invalid started_at"))?;

        let updated_at: jiff::Timestamp = obj
            .get("updated_at")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(started_at);

        let goroutines = obj.get("goroutines").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let alloc_bytes = obj.get("alloc_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let heap_bytes = obj.get("heap_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let sys_bytes = obj.get("sys_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let num_gc = obj.get("num_gc").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let go_version = obj.get("go_version").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let in_flight = obj
            .get("in_flight")
            .and_then(|v| serde_json::from_value::<HashMap<String, i64>>(v.clone()).ok());

        let model_pauses = obj.get("model_pauses").and_then(|v| {
            let map: HashMap<String, HashMap<String, String>> = serde_json::from_value(v.clone()).ok()?;
            let mut parsed: HashMap<String, HashMap<String, jiff::Timestamp>> = HashMap::new();
            for (acc, models) in map {
                let mut inner = HashMap::new();
                for (model, ts_str) in models {
                    if let Ok(ts) = ts_str.parse::<jiff::Timestamp>() {
                        inner.insert(model, ts);
                    }
                }
                parsed.insert(acc, inner);
            }
            Some(parsed)
        });

        let last_model = obj.get("last_model").and_then(|v| v.as_str()).map(|s| s.to_string());
        let smart_rr_cursor = obj.get("smart_rr_cursor").and_then(|v| v.as_u64());

        let snapshot = crate::observability::Snapshot {
            generation_requests: obj
                .get("generation_requests_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            generation_success: obj
                .get("generation_success_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            routing_decisions: obj.get("routing_decisions_total").and_then(|v| v.as_u64()).unwrap_or(0),
            failover_attempts: obj.get("failover_attempts_total").and_then(|v| v.as_u64()).unwrap_or(0),
            failover_success: obj.get("failover_success_total").and_then(|v| v.as_u64()).unwrap_or(0),
            restricted_skips: obj
                .get("restricted_account_skips_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            quota_refresh_attempts: obj
                .get("quota_refresh_attempts_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            quota_refresh_success: obj
                .get("quota_refresh_success_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            quota_refresh_failure: obj
                .get("quota_refresh_failure_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            auth_refresh_attempts: obj
                .get("auth_refresh_attempts_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            auth_refresh_success: obj
                .get("auth_refresh_success_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            auth_refresh_failure: obj
                .get("auth_refresh_failure_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            persistence_errors: obj
                .get("persistence_errors_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            no_replay_prevented: obj
                .get("no_replay_prevented_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            lease_fallbacks: obj.get("lease_fallbacks_total").and_then(|v| v.as_u64()).unwrap_or(0),
        };

        Ok(RuntimeStats {
            pid,
            started_at,
            updated_at,
            goroutines,
            alloc_bytes,
            heap_bytes,
            sys_bytes,
            num_gc,
            go_version,
            in_flight,
            model_pauses,
            last_model,
            smart_rr_cursor,
            snapshot,
        })
    }
}

impl RuntimeStats {
    pub fn paused_for_last_model(&self, account_id: &str, now: jiff::Timestamp) -> bool {
        if let Some(last) = &self.last_model {
            if last.is_empty() {
                return false;
            }
            if let Some(pauses) = &self.model_pauses
                && let Some(acc_pauses) = pauses.get(account_id)
                && let Some(until) = acc_pauses.get(last)
            {
                return now < *until;
            }
        }
        false
    }
}

fn system_page_size() -> u64 {
    #[cfg(unix)]
    {
        let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if sz > 0 { sz as u64 } else { 4096 }
    }
    #[cfg(not(unix))]
    {
        4096
    }
}

fn read_statm_bytes() -> (u64, u64) {
    let page_size = system_page_size();
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return (0, 0);
    };
    let mut parts = statm.split_whitespace();
    if let (Some(Ok(total_pages)), Some(Ok(rss_pages))) = (
        parts.next().map(|s| s.parse::<u64>()),
        parts.next().map(|s| s.parse::<u64>()),
    ) {
        (
            total_pages.saturating_mul(page_size),
            rss_pages.saturating_mul(page_size),
        )
    } else {
        (0, 0)
    }
}

/// Returns current process resident set size (RSS) in bytes from `/proc/self/statm`.
pub fn process_rss_bytes() -> u64 {
    read_statm_bytes().1
}

/// Collects runtime stats from the current process and /proc/self.
pub fn collect_runtime_stats(pid: i32, started_at: Option<jiff::Timestamp>) -> RuntimeStats {
    let pid = if pid <= 0 { std::process::id() as i32 } else { pid };
    let started_at = started_at.unwrap_or_else(jiff::Timestamp::now);

    let (sys_bytes, alloc_bytes) = read_statm_bytes();
    let mut goroutines = 1usize;

    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Threads:") {
                if let Some(val) = line.split_whitespace().nth(1)
                    && let Ok(threads) = val.parse::<usize>()
                {
                    goroutines = threads;
                }
                break;
            }
        }
    }

    let in_flight = crate::observability::all_in_flight_generations();
    let model_pauses_raw = crate::proxy::active_model_pauses(std::time::SystemTime::now());
    let mut model_pauses = HashMap::new();
    for (acc_id, pauses) in model_pauses_raw {
        let mut inner = HashMap::new();
        for (m, t) in pauses {
            if let Ok(dur) = t.duration_since(std::time::UNIX_EPOCH)
                && let Ok(ts) = jiff::Timestamp::from_second(dur.as_secs() as i64)
            {
                inner.insert(m, ts);
            }
        }
        model_pauses.insert(acc_id, inner);
    }

    RuntimeStats {
        pid,
        started_at,
        updated_at: jiff::Timestamp::now(),
        goroutines,
        alloc_bytes,
        heap_bytes: alloc_bytes,
        sys_bytes,
        num_gc: 0,
        go_version: format!("rust {}", crate::VERSION),
        in_flight: Some(in_flight),
        model_pauses: Some(model_pauses),
        last_model: Some(crate::proxy::last_generation_model()),
        smart_rr_cursor: Some(crate::scheduler::get_smart_rr_cursor()),
        snapshot: crate::observability::get_snapshot(),
    }
}

/// Atomically writes runtime stats to path with mode 0600.
pub fn write_runtime_stats(path: &Path, stats: &RuntimeStats) -> Result<(), DaemonError> {
    crate::config::assert_safe_write_path(path)?;
    crate::storage::atomic_json_write(path, stats).map_err(DaemonError::Storage)
}

/// Reads runtime stats from path and verifies schema integrity.
pub fn read_runtime_stats(path: &Path) -> Result<RuntimeStats, DaemonError> {
    crate::config::assert_safe_read_path(path)?;
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DaemonError::RuntimeStatsNotFound);
        }
        Err(e) => return Err(DaemonError::Io(e)),
    };

    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err(DaemonError::Message("empty runtime stats file".to_string()));
    }

    let stats: RuntimeStats =
        serde_json::from_str(trimmed).map_err(|e| DaemonError::Message(format!("corrupt runtime stats: {e}")))?;

    if stats.pid <= 0 {
        return Err(DaemonError::Message("incomplete runtime stats: pid <= 0".to_string()));
    }
    Ok(stats)
}

/// Formats a duration into human-readable shorthand (e.g. 37s, 12m 41s, 3h 18m, 2d 4h).
pub fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins < 60 {
        if rem_secs > 0 {
            return format!("{mins}m {rem_secs}s");
        }
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        if rem_mins > 0 {
            return format!("{hours}h {rem_mins}m");
        }
        return format!("{hours}h");
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours > 0 {
        return format!("{days}d {rem_hours}h");
    }
    format!("{days}d")
}

/// Formats bytes into human-readable binary units (KiB, MiB, GiB, TiB).
pub fn format_memory_bytes(b: u64) -> String {
    const UNIT: u64 = 1024;
    if b < UNIT {
        return format!("{b} B");
    }
    let mut div = UNIT;
    let mut exp = 0;
    let mut n = b / UNIT;
    while n >= UNIT {
        div *= UNIT;
        exp += 1;
        n /= UNIT;
    }
    let val = b as f64 / div as f64;
    let suffix = match exp {
        0 => "KiB",
        1 => "MiB",
        2 => "GiB",
        _ => "TiB",
    };
    format!("{:.1} {suffix}", val)
}

/// Formats a runtime version string cleanly (e.g. "go1.26.7" -> "Go 1.26.7", "rustc 1.82.0" -> "Rust 1.82.0").
pub fn format_go_version(v: &str) -> String {
    let v_trim = v.trim();
    if v_trim.is_empty() {
        return "Go (unknown)".to_string();
    }
    let lower = v_trim.to_lowercase();
    if lower.starts_with("go") {
        format!("Go {}", v_trim[2..].trim())
    } else if lower.starts_with("rustc") {
        format!("Rust {}", v_trim[5..].trim())
    } else if lower.starts_with("rust") {
        format!("Rust {}", v_trim[4..].trim())
    } else {
        v_trim.to_string()
    }
}

pub fn format_rust_version(v: &str) -> String {
    let v_trim = v.trim();
    if v_trim.is_empty() {
        return "Rust (unknown)".to_string();
    }
    let lower = v_trim.to_lowercase();
    if lower.starts_with("rustc") {
        format!("Rust {}", v_trim[5..].trim())
    } else if lower.starts_with("rust") {
        format!("Rust {}", v_trim[4..].trim())
    } else if lower.starts_with("go") {
        format!("Go {}", v_trim[2..].trim())
    } else {
        v_trim.to_string()
    }
}

/// NextCandidateID returns the account the running daemon would pick for the next new conversation.
pub fn next_candidate_id(pool: Option<&crate::storage::Pool>, now: f64) -> String {
    let pool = match pool {
        Some(p) if p.accounts.len() > 1 => p,
        _ => return String::new(),
    };
    let strat = if pool.strategy.is_empty() {
        crate::config::DEFAULT_STRATEGY
    } else {
        &pool.strategy
    };
    let ordered_indices = crate::scheduler::order_candidates(&pool.accounts, strat, pool, now);
    let mut ordered: Vec<crate::storage::Account> = ordered_indices
        .into_iter()
        .map(|idx| pool.accounts[idx].clone())
        .collect();

    let stats_path = crate::config::runtime_file();
    let stats = read_runtime_stats(&stats_path).ok().filter(|s| {
        let elapsed = (jiff::Timestamp::now().as_second() - s.updated_at.as_second()).abs();
        elapsed <= STALE_RUNTIME_THRESHOLD.as_secs() as i64
    });

    if let Some(s) = &stats
        && strat == crate::config::STRATEGY_SMART
        && let Some(cursor) = s.smart_rr_cursor
    {
        ordered = crate::scheduler::rotate_top_tier(&ordered, cursor, now);
    }

    let now_ts = jiff::Timestamp::from_second(now as i64).unwrap_or_else(|_| jiff::Timestamp::now());
    for acc in &ordered {
        if !crate::scheduler::is_restricted(acc) {
            let paused = stats.as_ref().is_some_and(|s| s.paused_for_last_model(&acc.id, now_ts));
            if !paused {
                return acc.id.clone();
            }
        }
    }
    String::new()
}

pub type CustomWriteFn = Arc<dyn Fn(&Path, &RuntimeStats) -> Result<(), DaemonError> + Send + Sync>;
pub type CustomCollectFn = Arc<dyn Fn(i32, Option<jiff::Timestamp>) -> RuntimeStats + Send + Sync>;

/// Manages periodic and coalesced writes of daemon runtime statistics.
pub struct RuntimeStatsWriter {
    pub pid: i32,
    pub start_time: jiff::Timestamp,
    pub runtime_file: PathBuf,
    pub throttle: Duration,
    pub periodic: Duration,
    pub trigger_tx: tokio::sync::mpsc::Sender<()>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    done_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    write_fn: Arc<std::sync::RwLock<Option<CustomWriteFn>>>,
    collect_fn: Arc<std::sync::RwLock<Option<CustomCollectFn>>>,
}

impl RuntimeStatsWriter {
    pub fn start(
        pid: i32,
        start_time: jiff::Timestamp,
        runtime_file: PathBuf,
        throttle: Duration,
        periodic: Duration,
    ) -> Self {
        let throttle = if throttle.is_zero() {
            RUNTIME_STATS_FLUSH_THROTTLE
        } else {
            throttle
        };
        let periodic = if periodic.is_zero() {
            Duration::from_secs(1)
        } else {
            periodic
        };

        let (trigger_tx, mut trigger_rx) = tokio::sync::mpsc::channel::<()>(100);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let write_fn: Arc<std::sync::RwLock<Option<CustomWriteFn>>> = Arc::new(std::sync::RwLock::new(None));
        let collect_fn: Arc<std::sync::RwLock<Option<CustomCollectFn>>> = Arc::new(std::sync::RwLock::new(None));

        let write_fn_task = Arc::clone(&write_fn);
        let collect_fn_task = Arc::clone(&collect_fn);
        let rf = runtime_file.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(periodic);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_write = Instant::now() - throttle;
            let mut pending_write = false;
            let mut coalesce_sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

            let do_flush = |pending: &mut bool, last: &mut Instant| {
                *pending = false;
                *last = Instant::now();
                let custom_collect = collect_fn_task.read().unwrap().clone();
                let stats = if let Some(cf) = custom_collect {
                    cf(pid, Some(start_time))
                } else {
                    collect_runtime_stats(pid, Some(start_time))
                };
                let custom_write = write_fn_task.read().unwrap().clone();
                if let Some(wf) = custom_write {
                    let _ = wf(&rf, &stats);
                } else {
                    let _ = write_runtime_stats(&rf, &stats);
                }
            };

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        if pending_write {
                            do_flush(&mut pending_write, &mut last_write);
                        }
                        break;
                    }
                    _ = ticker.tick() => {
                        coalesce_sleep = None;
                        do_flush(&mut pending_write, &mut last_write);
                    }
                    _ = async {
                        if let Some(sleep) = coalesce_sleep.as_mut() {
                            sleep.await
                        } else {
                            std::future::pending::<()>().await
                        }
                    } => {
                        coalesce_sleep = None;
                        do_flush(&mut pending_write, &mut last_write);
                    }
                    Some(()) = trigger_rx.recv() => {
                        let elapsed = last_write.elapsed();
                        if elapsed >= throttle {
                            coalesce_sleep = None;
                            do_flush(&mut pending_write, &mut last_write);
                        } else {
                            pending_write = true;
                            if coalesce_sleep.is_none() {
                                let wait_dur = (throttle - elapsed).max(Duration::from_millis(5));
                                coalesce_sleep = Some(Box::pin(tokio::time::sleep(wait_dur)));
                            }
                        }
                    }
                }
            }

            let _ = done_tx.send(());
        });

        RuntimeStatsWriter {
            pid,
            start_time,
            runtime_file,
            throttle,
            periodic,
            trigger_tx,
            stop_tx: Some(stop_tx),
            done_rx: Some(done_rx),
            write_fn,
            collect_fn,
        }
    }

    pub fn set_write_func(&self, f: CustomWriteFn) {
        *self.write_fn.write().unwrap() = Some(f);
    }

    pub fn set_collect_func(&self, f: CustomCollectFn) {
        *self.collect_fn.write().unwrap() = Some(f);
    }

    pub fn trigger(&self) {
        let _ = self.trigger_tx.try_send(());
    }

    pub async fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.done_rx.take() {
            let _ = rx.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    #[test]
    fn test_format_uptime() {
        let tests = [
            (Duration::from_secs(0), "0s"),
            (Duration::from_secs(37), "37s"),
            (Duration::from_secs(59), "59s"),
            (Duration::from_secs(60), "1m"),
            (Duration::from_secs(12 * 60 + 41), "12m 41s"),
            (Duration::from_secs(12 * 60), "12m"),
            (Duration::from_secs(3600 + 27 * 60), "1h 27m"),
            (Duration::from_secs(3 * 3600 + 18 * 60), "3h 18m"),
            (Duration::from_secs(3 * 3600), "3h"),
            (Duration::from_secs(24 * 3600), "1d"),
            (Duration::from_secs(2 * 24 * 3600 + 4 * 3600), "2d 4h"),
        ];

        for (d, want) in tests {
            let got = format_uptime(d);
            assert_eq!(got, want, "format_uptime({:?}) = {}, want {}", d, got, want);
        }
    }

    #[test]
    fn test_format_memory_bytes() {
        let to_mib = |m: f64| (m * 1024.0 * 1024.0) as u64;
        let to_gib = |g: f64| (g * 1024.0 * 1024.0 * 1024.0) as u64;

        let tests = [
            (512, "512 B"),
            (1024, "1.0 KiB"),
            (2048, "2.0 KiB"),
            (to_mib(12.4), "12.4 MiB"),
            (to_mib(18.7), "18.7 MiB"),
            (to_mib(31.2), "31.2 MiB"),
            (to_gib(1.5), "1.5 GiB"),
        ];

        for (bytes, want) in tests {
            let got = format_memory_bytes(bytes);
            assert_eq!(got, want, "format_memory_bytes({}) = {}, want {}", bytes, got, want);
        }
    }

    #[test]
    fn test_format_go_version() {
        let tests = [
            ("go1.26.7", "Go 1.26.7"),
            ("go1.22.5", "Go 1.22.5"),
            ("Go1.23.0", "Go 1.23.0"),
            ("", "Go (unknown)"),
            ("custom-build", "custom-build"),
        ];

        for (input, want) in tests {
            let got = format_go_version(input);
            assert_eq!(got, want, "format_go_version({:?}) = {}, want {}", input, got, want);
        }
    }

    #[test]
    fn test_collect_runtime_stats() {
        let started_at = jiff::Timestamp::now();
        let stats = collect_runtime_stats(12345, Some(started_at));

        assert_eq!(stats.pid, 12345);
        assert_eq!(stats.started_at, started_at);
        assert!(stats.goroutines >= 1);
        assert!(stats.alloc_bytes > 0);
        assert!(stats.sys_bytes > 0);
        assert!(!stats.go_version.is_empty());
    }

    #[test]
    fn test_runtime_stats_write_and_read_roundtrip() {
        let env = crate::daemon::test_env();
        let target_file = env.temp_dir.path().join("agy-pool-runtime.json");
        let started_at = jiff::Timestamp::from_second(1700000000).unwrap();
        let updated_at = jiff::Timestamp::from_second(1700000100).unwrap();

        let original = RuntimeStats {
            pid: 4567,
            started_at,
            updated_at,
            goroutines: 24,
            alloc_bytes: 12400000,
            heap_bytes: 18700000,
            sys_bytes: 31200000,
            num_gc: 42,
            go_version: "go1.26.7".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot::default(),
        };

        write_runtime_stats(&target_file, &original).unwrap();

        let read_back = read_runtime_stats(&target_file).unwrap();
        assert_eq!(read_back.pid, original.pid);
        assert_eq!(read_back.goroutines, original.goroutines);
        assert_eq!(read_back.alloc_bytes, original.alloc_bytes);
        assert_eq!(read_back.heap_bytes, original.heap_bytes);
        assert_eq!(read_back.sys_bytes, original.sys_bytes);
        assert_eq!(read_back.num_gc, original.num_gc);
        assert_eq!(read_back.go_version, original.go_version);
    }

    #[test]
    fn test_runtime_stats_missing_file() {
        let env = crate::daemon::test_env();
        let missing = env.temp_dir.path().join("missing.json");
        let res = read_runtime_stats(&missing);
        assert!(matches!(res, Err(DaemonError::RuntimeStatsNotFound)));
    }

    #[test]
    fn test_runtime_stats_corrupt_file() {
        let env = crate::daemon::test_env();
        let corrupt = env.temp_dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{invalid json").unwrap();
        let res = read_runtime_stats(&corrupt);
        assert!(res.is_err());
    }

    #[test]
    fn test_runtime_stats_empty_file() {
        let env = crate::daemon::test_env();
        let empty = env.temp_dir.path().join("empty.json");
        std::fs::write(&empty, b"   \n").unwrap();
        let res = read_runtime_stats(&empty);
        assert!(res.is_err());
    }

    #[test]
    fn test_get_runtime_stats_freshness_and_pid() {
        let env = crate::daemon::test_env();
        let ctx =
            crate::daemon::resolve_instance_context(Some(env.temp_dir.path()), None, Some(9299), Some("127.0.0.1"))
                .unwrap();

        // 1. Initially missing
        assert!(ctx.get_runtime_stats(1000).is_err());

        // 2. Fresh valid stats
        let now_sec = jiff::Timestamp::now().as_second();
        let stats = RuntimeStats {
            pid: 1000,
            started_at: jiff::Timestamp::from_second(now_sec - 3600).unwrap(),
            updated_at: jiff::Timestamp::from_second(now_sec).unwrap(),
            goroutines: 10,
            alloc_bytes: 1024,
            heap_bytes: 1024,
            sys_bytes: 2048,
            num_gc: 0,
            go_version: "go1.22.5".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot::default(),
        };
        write_runtime_stats(&ctx.runtime_file(), &stats).unwrap();

        let got = ctx.get_runtime_stats(1000).unwrap();
        assert_eq!(got.pid, 1000);
        assert_eq!(got.goroutines, 10);

        // 3. PID mismatch
        assert!(matches!(
            ctx.get_runtime_stats(2000),
            Err(DaemonError::RuntimeStatsMismatch(_))
        ));

        // 4. Stale stats (> STALE_RUNTIME_THRESHOLD)
        let mut stale = stats.clone();
        stale.updated_at = jiff::Timestamp::from_second(now_sec - 120).unwrap();
        write_runtime_stats(&ctx.runtime_file(), &stale).unwrap();
        assert!(matches!(
            ctx.get_runtime_stats(1000),
            Err(DaemonError::RuntimeStatsStale(_))
        ));

        // 5. Fallback to PID file if RuntimeFile is missing
        let _ = std::fs::remove_file(ctx.runtime_file());
        let pid_info = crate::daemon::DaemonInfo {
            pid: 1000,
            port: Some(9299),
            runtime: Some(RuntimeStats {
                pid: 1000,
                started_at: jiff::Timestamp::from_second(now_sec - 1800).unwrap(),
                updated_at: jiff::Timestamp::from_second(now_sec).unwrap(),
                goroutines: 15,
                alloc_bytes: 1024,
                heap_bytes: 1024,
                sys_bytes: 2048,
                num_gc: 0,
                go_version: "go1.22.5".to_string(),
                in_flight: None,
                model_pauses: None,
                last_model: None,
                smart_rr_cursor: None,
                snapshot: crate::observability::Snapshot::default(),
            }),
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        let fallback = ctx.get_runtime_stats(1000).unwrap();
        assert_eq!(fallback.goroutines, 15);
    }

    #[test]
    fn test_observability_case_m_older_runtime_json_decodes_safely() {
        let env = crate::daemon::test_env();
        let older_json = r#"{
  "pid": 12345,
  "started_at": "2026-09-19T10:00:00Z",
  "updated_at": "2026-09-19T11:00:00Z",
  "goroutines": 12,
  "alloc_bytes": 1048576,
  "heap_bytes": 2097152,
  "sys_bytes": 4194304,
  "num_gc": 5,
  "go_version": "go1.22.5"
}"#;
        let stats_file = env.temp_dir.path().join("older-runtime.json");
        std::fs::write(&stats_file, older_json.as_bytes()).unwrap();

        let stats = read_runtime_stats(&stats_file).unwrap();
        assert_eq!(stats.pid, 12345);
        assert_eq!(stats.goroutines, 12);
        assert_eq!(stats.alloc_bytes, 1048576);
        assert_eq!(stats.heap_bytes, 2097152);
        assert_eq!(stats.sys_bytes, 4194304);
        assert_eq!(stats.num_gc, 5);
        assert_eq!(stats.go_version, "go1.22.5");
        assert_eq!(stats.snapshot, crate::observability::Snapshot::default());
    }

    #[test]
    fn test_observability_case_n_stale_pid_rejects_stats() {
        let env = crate::daemon::test_env();
        let ctx =
            crate::daemon::resolve_instance_context(Some(env.temp_dir.path()), None, Some(9399), Some("127.0.0.1"))
                .unwrap();

        let now_sec = jiff::Timestamp::now().as_second();
        let old_stats = RuntimeStats {
            pid: 9999,
            started_at: jiff::Timestamp::from_second(now_sec - 3600).unwrap(),
            updated_at: jiff::Timestamp::from_second(now_sec).unwrap(),
            goroutines: 10,
            alloc_bytes: 1024,
            heap_bytes: 1024,
            sys_bytes: 2048,
            num_gc: 0,
            go_version: "go1.22.5".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot {
                generation_requests: 42,
                routing_decisions: 10,
                failover_attempts: 3,
                failover_success: 3,
                ..Default::default()
            },
        };
        write_runtime_stats(&ctx.runtime_file(), &old_stats).unwrap();

        let res = ctx.get_runtime_stats(1234);
        assert!(matches!(res, Err(DaemonError::RuntimeStatsMismatch(_))));

        let stale_stats = RuntimeStats {
            pid: 1234,
            started_at: jiff::Timestamp::from_second(now_sec - 7200).unwrap(),
            updated_at: jiff::Timestamp::from_second(now_sec - 120).unwrap(),
            goroutines: 10,
            alloc_bytes: 1024,
            heap_bytes: 1024,
            sys_bytes: 2048,
            num_gc: 0,
            go_version: "go1.22.5".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot {
                generation_requests: 99,
                ..Default::default()
            },
        };
        write_runtime_stats(&ctx.runtime_file(), &stale_stats).unwrap();

        let res = ctx.get_runtime_stats(1234);
        assert!(matches!(res, Err(DaemonError::RuntimeStatsStale(_))));
    }

    #[tokio::test]
    async fn test_runtime_stats_writer_100_event_coalescence_and_final_zero() {
        // In-flight counts and the change hook are process-wide; proxy tests use them under the same lock.
        let _env = crate::daemon::test_env();
        crate::observability::reset_in_flight();

        let write_count = Arc::new(AtomicI64::new(0));
        let last_written_val = Arc::new(AtomicI64::new(-1));

        let wc = Arc::clone(&write_count);
        let lwv = Arc::clone(&last_written_val);
        let custom_write: CustomWriteFn = Arc::new(move |_path, stats| {
            wc.fetch_add(1, Ordering::SeqCst);
            let val = stats
                .in_flight
                .as_ref()
                .and_then(|m| m.get("acc-stress-1").copied())
                .unwrap_or(0);
            lwv.store(val, Ordering::SeqCst);
            Ok(())
        });

        let throttle = Duration::from_millis(150);
        let mut writer = RuntimeStatsWriter::start(
            12345,
            jiff::Timestamp::now(),
            PathBuf::from("/dev/null"),
            throttle,
            Duration::from_secs(5),
        );
        writer.set_write_func(custom_write);

        let writer_trigger = {
            let tx = writer.trigger_tx.clone();
            move || {
                let _ = tx.try_send(());
            }
        };
        crate::observability::set_on_in_flight_change(Some(Box::new(writer_trigger)));

        const NUM_TASKS: usize = 10;
        const CYCLES_PER_TASK: usize = 10;

        let mut handles = Vec::new();
        for _ in 0..NUM_TASKS {
            handles.push(tokio::spawn(async move {
                for _ in 0..CYCLES_PER_TASK {
                    let mut guard = crate::observability::start_in_flight_generation("acc-stress-1");
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    guard.done();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let in_mem = crate::observability::get_in_flight_generation("acc-stress-1");
        assert_eq!(in_mem, 0, "in-memory in-flight must be 0 after all cycles");

        tokio::time::sleep(throttle + Duration::from_millis(100)).await;
        writer.stop().await;
        crate::observability::clear_on_in_flight_change();

        let writes = write_count.load(Ordering::SeqCst);
        assert!(writes <= 8, "expected coalesced writes <= 8, got {}", writes);
        assert!(writes > 0, "expected at least 1 write");

        let last_val = last_written_val.load(Ordering::SeqCst);
        assert_eq!(last_val, 0, "expected final written in-flight to be 0");
    }

    #[tokio::test]
    async fn test_runtime_stats_writer_multiple_accounts_stress_coalescence() {
        // In-flight counts and the change hook are process-wide; proxy tests use them under the same lock.
        let _env = crate::daemon::test_env();
        crate::observability::reset_in_flight();

        let write_count = Arc::new(AtomicI64::new(0));
        let wc = Arc::clone(&write_count);
        let custom_write: CustomWriteFn = Arc::new(move |_path, _stats| {
            wc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let throttle = Duration::from_millis(150);
        let mut writer = RuntimeStatsWriter::start(
            99999,
            jiff::Timestamp::now(),
            PathBuf::from("/dev/null"),
            throttle,
            Duration::from_secs(5),
        );
        writer.set_write_func(custom_write);

        let writer_trigger = {
            let tx = writer.trigger_tx.clone();
            move || {
                let _ = tx.try_send(());
            }
        };
        crate::observability::set_on_in_flight_change(Some(Box::new(writer_trigger)));

        const NUM_ACCOUNTS: usize = 5;
        const PER_ACC: usize = 20;

        let mut handles = Vec::new();
        for a in 0..NUM_ACCOUNTS {
            let acc_id = format!("acc-multi-{a}");
            handles.push(tokio::spawn(async move {
                for _ in 0..PER_ACC {
                    let mut guard = crate::observability::start_in_flight_generation(&acc_id);
                    tokio::time::sleep(Duration::from_micros(500)).await;
                    guard.done();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let all = crate::observability::all_in_flight_generations();
        for (id, cnt) in all {
            assert_eq!(cnt, 0, "expected in-flight for {} to be 0", id);
        }

        tokio::time::sleep(throttle + Duration::from_millis(100)).await;
        writer.stop().await;
        crate::observability::clear_on_in_flight_change();

        let writes = write_count.load(Ordering::SeqCst);
        assert!(writes <= 8, "expected coalesced writes <= 8, got {}", writes);
    }
}
