use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

pub mod audit;
pub mod logger;
pub use audit::*;
pub use logger::*;

/// Low-overhead process-lifetime atomic counters for runtime observability.
struct Metrics {
    generation_requests: AtomicU64,
    generation_success: AtomicU64,
    routing_decisions: AtomicU64,
    failover_attempts: AtomicU64,
    failover_success: AtomicU64,
    restricted_skips: AtomicU64,
    quota_refresh_attempts: AtomicU64,
    quota_refresh_success: AtomicU64,
    quota_refresh_failure: AtomicU64,
    auth_refresh_attempts: AtomicU64,
    auth_refresh_success: AtomicU64,
    auth_refresh_failure: AtomicU64,
    persistence_errors: AtomicU64,
    no_replay_prevented: AtomicU64,
    lease_fallbacks: AtomicU64,
}

static GLOBAL_METRICS: Metrics = Metrics {
    generation_requests: AtomicU64::new(0),
    generation_success: AtomicU64::new(0),
    routing_decisions: AtomicU64::new(0),
    failover_attempts: AtomicU64::new(0),
    failover_success: AtomicU64::new(0),
    restricted_skips: AtomicU64::new(0),
    quota_refresh_attempts: AtomicU64::new(0),
    quota_refresh_success: AtomicU64::new(0),
    quota_refresh_failure: AtomicU64::new(0),
    auth_refresh_attempts: AtomicU64::new(0),
    auth_refresh_success: AtomicU64::new(0),
    auth_refresh_failure: AtomicU64::new(0),
    persistence_errors: AtomicU64::new(0),
    no_replay_prevented: AtomicU64::new(0),
    lease_fallbacks: AtomicU64::new(0),
};

/// Point-in-time snapshot of process-lifetime counters.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "generation_requests_total")]
    pub generation_requests: u64,
    #[serde(rename = "generation_success_total")]
    pub generation_success: u64,
    #[serde(rename = "routing_decisions_total")]
    pub routing_decisions: u64,
    #[serde(rename = "failover_attempts_total")]
    pub failover_attempts: u64,
    #[serde(rename = "failover_success_total")]
    pub failover_success: u64,
    #[serde(rename = "restricted_account_skips_total")]
    pub restricted_skips: u64,
    #[serde(rename = "quota_refresh_attempts_total")]
    pub quota_refresh_attempts: u64,
    #[serde(rename = "quota_refresh_success_total")]
    pub quota_refresh_success: u64,
    #[serde(rename = "quota_refresh_failure_total")]
    pub quota_refresh_failure: u64,
    #[serde(rename = "auth_refresh_attempts_total")]
    pub auth_refresh_attempts: u64,
    #[serde(rename = "auth_refresh_success_total")]
    pub auth_refresh_success: u64,
    #[serde(rename = "auth_refresh_failure_total")]
    pub auth_refresh_failure: u64,
    #[serde(rename = "persistence_errors_total")]
    pub persistence_errors: u64,
    #[serde(rename = "no_replay_prevented_total")]
    pub no_replay_prevented: u64,
    #[serde(rename = "lease_fallbacks_total")]
    pub lease_fallbacks: u64,
}

struct InFlightState {
    counts: HashMap<String, i64>,
    remote: HashMap<String, i64>,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

static IN_FLIGHT: LazyLock<Mutex<InFlightState>> = LazyLock::new(|| {
    Mutex::new(InFlightState {
        counts: HashMap::new(),
        remote: HashMap::new(),
        on_change: None,
    })
});

/// Resets all process-lifetime counters to zero (used during startup or test setup).
pub fn reset() {
    GLOBAL_METRICS.generation_requests.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.generation_success.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.routing_decisions.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.failover_attempts.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.failover_success.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.restricted_skips.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.quota_refresh_attempts.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.quota_refresh_success.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.quota_refresh_failure.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.auth_refresh_attempts.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.auth_refresh_success.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.auth_refresh_failure.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.persistence_errors.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.no_replay_prevented.store(0, Ordering::SeqCst);
    GLOBAL_METRICS.lease_fallbacks.store(0, Ordering::SeqCst);
    reset_in_flight();
}

/// Returns a point-in-time snapshot of all observability counters.
pub fn snapshot() -> Snapshot {
    Snapshot {
        generation_requests: GLOBAL_METRICS.generation_requests.load(Ordering::SeqCst),
        generation_success: GLOBAL_METRICS.generation_success.load(Ordering::SeqCst),
        routing_decisions: GLOBAL_METRICS.routing_decisions.load(Ordering::SeqCst),
        failover_attempts: GLOBAL_METRICS.failover_attempts.load(Ordering::SeqCst),
        failover_success: GLOBAL_METRICS.failover_success.load(Ordering::SeqCst),
        restricted_skips: GLOBAL_METRICS.restricted_skips.load(Ordering::SeqCst),
        quota_refresh_attempts: GLOBAL_METRICS.quota_refresh_attempts.load(Ordering::SeqCst),
        quota_refresh_success: GLOBAL_METRICS.quota_refresh_success.load(Ordering::SeqCst),
        quota_refresh_failure: GLOBAL_METRICS.quota_refresh_failure.load(Ordering::SeqCst),
        auth_refresh_attempts: GLOBAL_METRICS.auth_refresh_attempts.load(Ordering::SeqCst),
        auth_refresh_success: GLOBAL_METRICS.auth_refresh_success.load(Ordering::SeqCst),
        auth_refresh_failure: GLOBAL_METRICS.auth_refresh_failure.load(Ordering::SeqCst),
        persistence_errors: GLOBAL_METRICS.persistence_errors.load(Ordering::SeqCst),
        no_replay_prevented: GLOBAL_METRICS.no_replay_prevented.load(Ordering::SeqCst),
        lease_fallbacks: GLOBAL_METRICS.lease_fallbacks.load(Ordering::SeqCst),
    }
}

/// Alias for `snapshot()`, matching Go's `GetSnapshot()`.
pub fn get_snapshot() -> Snapshot {
    snapshot()
}

/// Formats the current metrics snapshot in Prometheus text exposition format (version 0.0.4).
pub fn format_prometheus_metrics() -> String {
    let s = snapshot();
    let mut b = String::new();

    let mut write_metric = |name: &str, help: &str, typ: &str, val: u64| {
        let _ = writeln!(b, "# HELP {name} {help}\n# TYPE {name} {typ}\n{name} {val}");
    };

    write_metric(
        "agy_generation_requests_total",
        "Total generation requests handled",
        "counter",
        s.generation_requests,
    );
    write_metric(
        "agy_generation_success_total",
        "Total generation requests successfully finished",
        "counter",
        s.generation_success,
    );
    write_metric(
        "agy_routing_decisions_total",
        "Total routing candidate decisions made",
        "counter",
        s.routing_decisions,
    );
    write_metric(
        "agy_failover_attempts_total",
        "Total failover attempts to secondary candidates",
        "counter",
        s.failover_attempts,
    );
    write_metric(
        "agy_failover_success_total",
        "Total successful failover dispatches",
        "counter",
        s.failover_success,
    );
    write_metric(
        "agy_restricted_account_skips_total",
        "Total candidates skipped due to account restrictions",
        "counter",
        s.restricted_skips,
    );
    write_metric(
        "agy_quota_refresh_attempts_total",
        "Total background quota refresh attempts",
        "counter",
        s.quota_refresh_attempts,
    );
    write_metric(
        "agy_quota_refresh_success_total",
        "Total background quota refreshes succeeded",
        "counter",
        s.quota_refresh_success,
    );
    write_metric(
        "agy_quota_refresh_failure_total",
        "Total background quota refreshes failed",
        "counter",
        s.quota_refresh_failure,
    );
    write_metric(
        "agy_auth_refresh_attempts_total",
        "Total OAuth token refresh attempts",
        "counter",
        s.auth_refresh_attempts,
    );
    write_metric(
        "agy_auth_refresh_success_total",
        "Total OAuth token refreshes succeeded",
        "counter",
        s.auth_refresh_success,
    );
    write_metric(
        "agy_auth_refresh_failure_total",
        "Total OAuth token refreshes failed",
        "counter",
        s.auth_refresh_failure,
    );
    write_metric(
        "agy_persistence_errors_total",
        "Total state persistence errors",
        "counter",
        s.persistence_errors,
    );
    write_metric(
        "agy_no_replay_prevented_total",
        "Total requests prevented from replaying once committed",
        "counter",
        s.no_replay_prevented,
    );
    write_metric(
        "agy_lease_fallbacks_total",
        "Total fallback reservations when DB lease failed",
        "counter",
        s.lease_fallbacks,
    );

    let total_in_flight: i64 = {
        let state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        state.counts.values().sum()
    };

    let _ = writeln!(
        b,
        "# HELP agy_inflight_generations Current concurrent generation requests\n# TYPE agy_inflight_generations gauge\nagy_inflight_generations {total_in_flight}"
    );

    b
}

/// Increments generation_requests_total.
pub fn record_generation_request() {
    GLOBAL_METRICS.generation_requests.fetch_add(1, Ordering::SeqCst);
}

/// Increments generation_success_total.
pub fn record_generation_success() {
    GLOBAL_METRICS.generation_success.fetch_add(1, Ordering::SeqCst);
}

/// Increments routing_decisions_total.
pub fn record_routing_decision() {
    GLOBAL_METRICS.routing_decisions.fetch_add(1, Ordering::SeqCst);
}

/// Increments failover_attempts_total.
pub fn record_failover_attempt() {
    GLOBAL_METRICS.failover_attempts.fetch_add(1, Ordering::SeqCst);
}

/// Increments failover_success_total.
pub fn record_failover_success() {
    GLOBAL_METRICS.failover_success.fetch_add(1, Ordering::SeqCst);
}

/// Increments restricted_account_skips_total.
pub fn record_restricted_skip() {
    GLOBAL_METRICS.restricted_skips.fetch_add(1, Ordering::SeqCst);
}

/// Increments quota_refresh_attempts_total.
pub fn record_quota_refresh_attempt() {
    GLOBAL_METRICS.quota_refresh_attempts.fetch_add(1, Ordering::SeqCst);
}

/// Increments quota_refresh_success_total.
pub fn record_quota_refresh_success() {
    GLOBAL_METRICS.quota_refresh_success.fetch_add(1, Ordering::SeqCst);
}

/// Increments quota_refresh_failure_total.
pub fn record_quota_refresh_failure() {
    GLOBAL_METRICS.quota_refresh_failure.fetch_add(1, Ordering::SeqCst);
}

/// Increments auth_refresh_attempts_total.
pub fn record_auth_refresh_attempt() {
    GLOBAL_METRICS.auth_refresh_attempts.fetch_add(1, Ordering::SeqCst);
}

/// Increments auth_refresh_success_total.
pub fn record_auth_refresh_success() {
    GLOBAL_METRICS.auth_refresh_success.fetch_add(1, Ordering::SeqCst);
}

/// Increments auth_refresh_failure_total.
pub fn record_auth_refresh_failure() {
    GLOBAL_METRICS.auth_refresh_failure.fetch_add(1, Ordering::SeqCst);
}

/// Increments persistence_errors_total.
pub fn record_persistence_error() {
    GLOBAL_METRICS.persistence_errors.fetch_add(1, Ordering::SeqCst);
}

/// Increments lease_fallbacks_total.
pub fn record_lease_fallback() {
    GLOBAL_METRICS.lease_fallbacks.fetch_add(1, Ordering::SeqCst);
}

/// Increments no_replay_prevented_total.
pub fn record_no_replay_prevented() {
    GLOBAL_METRICS.no_replay_prevented.fetch_add(1, Ordering::SeqCst);
}

/// Formats a u64 integer with thousands separators (e.g. 12,430).
pub fn format_number(n: u64) -> String {
    let s = n.to_string();
    if s.len() <= 3 {
        return s;
    }
    let mut out = String::with_capacity(s.len() + (s.len() - 1) / 3);
    let rem = s.len() % 3;
    if rem > 0 {
        out.push_str(&s[..rem]);
    }
    let mut i = rem;
    while i < s.len() {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&s[i..i + 3]);
        i += 3;
    }
    out
}

/// Replaces the snapshot of in-flight counts held by other processes sharing the pool.
pub fn set_remote_in_flight(m: HashMap<String, i64>) {
    let mut state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    state.remote = m;
}

/// Returns the last snapshot of in-flight counts held by other processes.
pub fn remote_in_flight() -> HashMap<String, i64> {
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).remote.clone()
}

/// Registers a callback invoked whenever an in-flight generation starts or finishes.
pub fn set_on_in_flight_change(cb: Option<Box<dyn Fn() + Send + Sync>>) {
    let mut state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    state.on_change = cb.map(Arc::from);
}

/// Helper to clear the in-flight change callback.
pub fn clear_on_in_flight_change() {
    set_on_in_flight_change(None);
}

/// RAII guard that decrements the in-flight count on Drop.
#[derive(Debug)]
pub struct InFlightGuard {
    account_id: Option<String>,
}

impl InFlightGuard {
    /// Explicitly decrements the in-flight count (idempotent).
    pub fn done(&mut self) {
        if let Some(id) = self.account_id.take() {
            decrement_in_flight(&id);
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.done();
    }
}

fn decrement_in_flight(account_id: &str) {
    let callback = {
        let mut state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(val) = state.counts.get_mut(account_id) {
            if *val <= 1 {
                state.counts.remove(account_id);
            } else {
                *val -= 1;
            }
        }
        state.on_change.clone()
    };

    if let Some(cb) = callback {
        cb();
    }
}

/// Increments the in-flight count for account_id and returns an InFlightGuard to decrement it.
pub fn start_in_flight_generation(account_id: &str) -> InFlightGuard {
    if account_id.is_empty() {
        return InFlightGuard { account_id: None };
    }

    let callback = {
        let mut state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        *state.counts.entry(account_id.to_string()).or_insert(0) += 1;
        state.on_change.clone()
    };

    if let Some(cb) = callback {
        cb();
    }

    InFlightGuard {
        account_id: Some(account_id.to_string()),
    }
}

/// Returns the current in-flight generation request count for an account,
/// including the last snapshot of other processes' requests.
pub fn in_flight_generation(account_id: &str) -> i64 {
    let state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    let local = state.counts.get(account_id).copied().unwrap_or(0);
    let remote = state.remote.get(account_id).copied().unwrap_or(0);
    local + remote
}

/// Alias for `in_flight_generation()`, matching Go's `GetInFlightGeneration()`.
pub fn get_in_flight_generation(account_id: &str) -> i64 {
    in_flight_generation(account_id)
}

/// Returns a map snapshot of all accounts with positive in-flight requests.
pub fn all_in_flight_generations() -> HashMap<String, i64> {
    let state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    state
        .counts
        .iter()
        .filter(|(_, v)| **v > 0)
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

/// Alias for `all_in_flight_generations()`, matching Go's `GetAllInFlightGenerations()`.
pub fn get_all_in_flight_generations() -> HashMap<String, i64> {
    all_in_flight_generations()
}

/// Resets all in-flight counts to zero.
pub fn reset_in_flight() {
    let mut state = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    state.counts.clear();
    state.remote.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_atomic_operations_and_reset() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        std::thread::scope(|s| {
            for _ in 0..100 {
                s.spawn(|| {
                    record_generation_request();
                    record_generation_success();
                    record_routing_decision();
                    record_failover_attempt();
                    record_failover_success();
                    record_restricted_skip();
                    record_quota_refresh_attempt();
                    record_quota_refresh_success();
                    record_quota_refresh_failure();
                    record_auth_refresh_attempt();
                    record_auth_refresh_success();
                    record_auth_refresh_failure();
                    record_persistence_error();
                    record_no_replay_prevented();
                });
            }
        });

        let snap = snapshot();
        assert_eq!(
            snap.generation_requests, 100,
            "expected 100 generation requests, got {}",
            snap.generation_requests
        );
        assert_eq!(
            snap.generation_success, 100,
            "expected 100 generation success, got {}",
            snap.generation_success
        );
        assert_eq!(
            snap.routing_decisions, 100,
            "expected 100 routing decisions, got {}",
            snap.routing_decisions
        );
        assert_eq!(
            snap.failover_attempts, 100,
            "expected 100 failover attempts, got {}",
            snap.failover_attempts
        );
        assert_eq!(
            snap.failover_success, 100,
            "expected 100 failover success, got {}",
            snap.failover_success
        );
        assert_eq!(
            snap.restricted_skips, 100,
            "expected 100 restricted skips, got {}",
            snap.restricted_skips
        );
        assert_eq!(
            snap.quota_refresh_attempts, 100,
            "expected 100 quota refresh attempts, got {}",
            snap.quota_refresh_attempts
        );
        assert_eq!(
            snap.quota_refresh_success, 100,
            "expected 100 quota refresh success, got {}",
            snap.quota_refresh_success
        );
        assert_eq!(
            snap.quota_refresh_failure, 100,
            "expected 100 quota refresh failure, got {}",
            snap.quota_refresh_failure
        );
        assert_eq!(
            snap.auth_refresh_attempts, 100,
            "expected 100 auth refresh attempts, got {}",
            snap.auth_refresh_attempts
        );
        assert_eq!(
            snap.auth_refresh_success, 100,
            "expected 100 auth refresh success, got {}",
            snap.auth_refresh_success
        );
        assert_eq!(
            snap.auth_refresh_failure, 100,
            "expected 100 auth refresh failure, got {}",
            snap.auth_refresh_failure
        );
        assert_eq!(
            snap.persistence_errors, 100,
            "expected 100 persistence errors, got {}",
            snap.persistence_errors
        );
        assert_eq!(
            snap.no_replay_prevented, 100,
            "expected 100 no replay prevented, got {}",
            snap.no_replay_prevented
        );

        reset();
        let snap = snapshot();
        assert!(
            snap.generation_requests == 0 && snap.generation_success == 0 && snap.routing_decisions == 0,
            "expected all counters reset to zero, got {:?}",
            snap
        );
    }

    #[test]
    fn test_format_number() {
        let cases = [
            (0, "0"),
            (9, "9"),
            (99, "99"),
            (999, "999"),
            (1000, "1,000"),
            (12430, "12,430"),
            (12401, "12,401"),
            (12988, "12,988"),
            (1000000, "1,000,000"),
            (18446744073709551615, "18,446,744,073,709,551,615"),
        ];

        for (in_val, want) in cases {
            let got = format_number(in_val);
            assert_eq!(got, want, "format_number({}) = {:?}, want {:?}", in_val, got, want);
        }
    }

    #[test]
    fn test_in_flight_tracking_concurrent_lifecycle() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        let in_flight = in_flight_generation("acc1");
        assert_eq!(in_flight, 0, "expected initial in-flight to be 0, got {}", in_flight);
        let all = all_in_flight_generations();
        assert!(
            all.is_empty(),
            "expected initial all_in_flight_generations to be empty, got {:?}",
            all
        );

        let mut done1 = start_in_flight_generation("acc1");
        let in_flight = in_flight_generation("acc1");
        assert_eq!(in_flight, 1, "expected in-flight 1, got {}", in_flight);

        let mut done2 = start_in_flight_generation("acc1");
        let in_flight = in_flight_generation("acc1");
        assert_eq!(in_flight, 2, "expected in-flight 2, got {}", in_flight);

        let all = all_in_flight_generations();
        assert_eq!(
            all.get("acc1").copied(),
            Some(2),
            "expected all_in_flight_generations to have acc1=2, got {:?}",
            all
        );

        // Decrement once
        done1.done();
        let in_flight = in_flight_generation("acc1");
        assert_eq!(in_flight, 1, "expected in-flight 1 after done1, got {}", in_flight);

        // Idempotent: done1 called again should not decrement further
        done1.done();
        let in_flight = in_flight_generation("acc1");
        assert_eq!(
            in_flight, 1,
            "expected in-flight still 1 after duplicate done1, got {}",
            in_flight
        );

        // Decrement second
        done2.done();
        let in_flight = in_flight_generation("acc1");
        assert_eq!(in_flight, 0, "expected in-flight 0 after done2, got {}", in_flight);
        let all = all_in_flight_generations();
        assert!(
            all.is_empty(),
            "expected all_in_flight_generations to be empty after all done, got {:?}",
            all
        );

        // Concurrency test: 50 threads
        let change_calls = Arc::new(Mutex::new(0usize));
        let change_calls_cb = change_calls.clone();
        set_on_in_flight_change(Some(Box::new(move || {
            let mut count = change_calls_cb.lock().unwrap();
            *count += 1;
        })));

        const COUNT: usize = 50;
        let dones_mutex = Mutex::new(Vec::with_capacity(COUNT));
        let dones_ref = &dones_mutex;

        std::thread::scope(|s| {
            for _ in 0..COUNT {
                s.spawn(move || {
                    let guard = start_in_flight_generation("acc_concurrent");
                    dones_ref.lock().unwrap().push(guard);
                });
            }
        });

        let in_flight = in_flight_generation("acc_concurrent");
        assert_eq!(
            in_flight, COUNT as i64,
            "expected in-flight {}, got {}",
            COUNT, in_flight
        );

        let mut guards = dones_mutex.into_inner().unwrap();
        std::thread::scope(|s| {
            for mut guard in guards.drain(..) {
                s.spawn(move || {
                    guard.done();
                });
            }
        });

        let in_flight = in_flight_generation("acc_concurrent");
        assert_eq!(in_flight, 0, "expected in-flight to return to 0, got {}", in_flight);

        let calls = *change_calls.lock().unwrap();
        assert!(
            calls >= COUNT * 2,
            "expected at least {} change callback calls, got {}",
            COUNT * 2,
            calls
        );

        set_on_in_flight_change(None);
    }

    #[test]
    fn test_format_prometheus_metrics() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        record_generation_request();
        record_generation_success();
        record_failover_attempt();

        let out = format_prometheus_metrics();
        assert!(
            out.contains("agy_generation_requests_total 1"),
            "expected agy_generation_requests_total 1 in prometheus output, got:\n{}",
            out
        );
        assert!(
            out.contains("agy_generation_success_total 1"),
            "expected agy_generation_success_total 1 in prometheus output, got:\n{}",
            out
        );
        assert!(
            out.contains("agy_failover_attempts_total 1"),
            "expected agy_failover_attempts_total 1 in prometheus output, got:\n{}",
            out
        );
        assert!(
            out.contains("agy_inflight_generations 0"),
            "expected agy_inflight_generations 0 in prometheus output, got:\n{}",
            out
        );
    }

    #[test]
    fn prometheus_output_matches_go() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        for _ in 0..1 {
            record_generation_request();
        }
        for _ in 0..2 {
            record_generation_success();
        }
        for _ in 0..3 {
            record_routing_decision();
        }
        for _ in 0..4 {
            record_failover_attempt();
        }
        for _ in 0..5 {
            record_failover_success();
        }
        for _ in 0..6 {
            record_restricted_skip();
        }
        for _ in 0..7 {
            record_quota_refresh_attempt();
        }
        for _ in 0..8 {
            record_quota_refresh_success();
        }
        for _ in 0..9 {
            record_quota_refresh_failure();
        }
        for _ in 0..10 {
            record_auth_refresh_attempt();
        }
        for _ in 0..11 {
            record_auth_refresh_success();
        }
        for _ in 0..12 {
            record_auth_refresh_failure();
        }
        for _ in 0..13 {
            record_persistence_error();
        }
        for _ in 0..14 {
            record_no_replay_prevented();
        }
        for _ in 0..15 {
            record_lease_fallback();
        }

        let got = format_prometheus_metrics();
        let want = r#"# HELP agy_generation_requests_total Total generation requests handled
# TYPE agy_generation_requests_total counter
agy_generation_requests_total 1
# HELP agy_generation_success_total Total generation requests successfully finished
# TYPE agy_generation_success_total counter
agy_generation_success_total 2
# HELP agy_routing_decisions_total Total routing candidate decisions made
# TYPE agy_routing_decisions_total counter
agy_routing_decisions_total 3
# HELP agy_failover_attempts_total Total failover attempts to secondary candidates
# TYPE agy_failover_attempts_total counter
agy_failover_attempts_total 4
# HELP agy_failover_success_total Total successful failover dispatches
# TYPE agy_failover_success_total counter
agy_failover_success_total 5
# HELP agy_restricted_account_skips_total Total candidates skipped due to account restrictions
# TYPE agy_restricted_account_skips_total counter
agy_restricted_account_skips_total 6
# HELP agy_quota_refresh_attempts_total Total background quota refresh attempts
# TYPE agy_quota_refresh_attempts_total counter
agy_quota_refresh_attempts_total 7
# HELP agy_quota_refresh_success_total Total background quota refreshes succeeded
# TYPE agy_quota_refresh_success_total counter
agy_quota_refresh_success_total 8
# HELP agy_quota_refresh_failure_total Total background quota refreshes failed
# TYPE agy_quota_refresh_failure_total counter
agy_quota_refresh_failure_total 9
# HELP agy_auth_refresh_attempts_total Total OAuth token refresh attempts
# TYPE agy_auth_refresh_attempts_total counter
agy_auth_refresh_attempts_total 10
# HELP agy_auth_refresh_success_total Total OAuth token refreshes succeeded
# TYPE agy_auth_refresh_success_total counter
agy_auth_refresh_success_total 11
# HELP agy_auth_refresh_failure_total Total OAuth token refreshes failed
# TYPE agy_auth_refresh_failure_total counter
agy_auth_refresh_failure_total 12
# HELP agy_persistence_errors_total Total state persistence errors
# TYPE agy_persistence_errors_total counter
agy_persistence_errors_total 13
# HELP agy_no_replay_prevented_total Total requests prevented from replaying once committed
# TYPE agy_no_replay_prevented_total counter
agy_no_replay_prevented_total 14
# HELP agy_lease_fallbacks_total Total fallback reservations when DB lease failed
# TYPE agy_lease_fallbacks_total counter
agy_lease_fallbacks_total 15
# HELP agy_inflight_generations Current concurrent generation requests
# TYPE agy_inflight_generations gauge
agy_inflight_generations 0
"#;

        assert_eq!(got, want);
    }
}
