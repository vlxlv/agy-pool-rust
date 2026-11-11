use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DashboardAuditEvent {
    pub timestamp: String,
    pub model: String,
    pub account: String,
    pub ttfb_ms: u64,
    pub total_ms: u64,
    pub tokens: i64,
    pub status: u16,
    pub failover: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LatencyStatsDto {
    pub sample_count: usize,
    pub ttfb_p50_ms: u64,
    pub ttfb_p95_ms: u64,
    pub total_p50_ms: u64,
    pub total_p95_ms: u64,
}

struct LatencySample {
    ts: Instant,
    ttfb_ms: u64,
    total_ms: u64,
}

struct TelemetryState {
    latency_samples: VecDeque<LatencySample>,
    recent_events: VecDeque<DashboardAuditEvent>,
}

static TELEMETRY_STATE: LazyLock<Mutex<TelemetryState>> = LazyLock::new(|| {
    Mutex::new(TelemetryState {
        latency_samples: VecDeque::with_capacity(100),
        recent_events: VecDeque::with_capacity(50),
    })
});

static BROADCAST_TX: LazyLock<broadcast::Sender<DashboardAuditEvent>> =
    LazyLock::new(|| broadcast::channel(128).0);

pub fn record_attempt(ev: DashboardAuditEvent) {
    let now = Instant::now();
    {
        let mut state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.latency_samples.push_back(LatencySample {
            ts: now,
            ttfb_ms: ev.ttfb_ms,
            total_ms: ev.total_ms,
        });
        if state.latency_samples.len() > 100 {
            state.latency_samples.pop_front();
        }

        state.recent_events.push_back(ev.clone());
        if state.recent_events.len() > 50 {
            state.recent_events.pop_front();
        }
    }
    let _ = BROADCAST_TX.send(ev);
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = (n as f64 * pct / 100.0).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

pub fn latency_and_qps_snapshot(now: Instant) -> (LatencyStatsDto, f64) {
    let (mut ttfb_list, mut total_list, recent_count) = {
        let state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let mut ttfb = Vec::with_capacity(state.latency_samples.len());
        let mut total = Vec::with_capacity(state.latency_samples.len());
        let mut recent = 0usize;
        for s in &state.latency_samples {
            ttfb.push(s.ttfb_ms);
            total.push(s.total_ms);
            let elapsed = now.checked_duration_since(s.ts).unwrap_or(Duration::ZERO);
            if elapsed <= Duration::from_secs(60) {
                recent += 1;
            }
        }
        (ttfb, total, recent)
    };

    let sample_count = ttfb_list.len();
    if sample_count == 0 {
        return (
            LatencyStatsDto {
                sample_count: 0,
                ttfb_p50_ms: 0,
                ttfb_p95_ms: 0,
                total_p50_ms: 0,
                total_p95_ms: 0,
            },
            0.0,
        );
    }

    ttfb_list.sort_unstable();
    total_list.sort_unstable();

    let ttfb_p50_ms = percentile(&ttfb_list, 50.0);
    let ttfb_p95_ms = percentile(&ttfb_list, 95.0);
    let total_p50_ms = percentile(&total_list, 50.0);
    let total_p95_ms = percentile(&total_list, 95.0);

    let qps = (recent_count as f64 / 60.0 * 100.0).round() / 100.0;

    (
        LatencyStatsDto {
            sample_count,
            ttfb_p50_ms,
            ttfb_p95_ms,
            total_p50_ms,
            total_p95_ms,
        },
        qps,
    )
}

pub fn recent_events(limit: usize) -> Vec<DashboardAuditEvent> {
    if limit == 0 {
        return Vec::new();
    }
    let state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let len = state.recent_events.len();
    let take = limit.min(len);
    let skip = len - take;
    state.recent_events.iter().skip(skip).cloned().collect()
}

pub fn subscribe_events() -> broadcast::Receiver<DashboardAuditEvent> {
    BROADCAST_TX.subscribe()
}

pub fn reset_for_test() {
    let mut state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.latency_samples.clear();
    state.recent_events.clear();
}

#[cfg(test)]
mod tests {
    use std::time::Instant;
    use tokio::sync::broadcast::error::RecvError;
    use super::*;

    #[test]
    fn test_empty_telemetry_returns_zeros() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let (stats, qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 0);
        assert_eq!(stats.ttfb_p50_ms, 0);
        assert_eq!(stats.ttfb_p95_ms, 0);
        assert_eq!(stats.total_p50_ms, 0);
        assert_eq!(stats.total_p95_ms, 0);
        assert_eq!(qps, 0.0);
        assert!(!qps.is_nan());
    }

    #[test]
    fn test_latency_percentiles_and_100_cap() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        for i in 1..=150 {
            record_attempt(DashboardAuditEvent {
                timestamp: format!("2026-11-11T12:00:{:02}Z", (i % 60)),
                model: "gemini-2.5-pro".to_string(),
                account: "Account-1".to_string(),
                ttfb_ms: i as u64,
                total_ms: (i * 10) as u64,
                tokens: 100,
                status: 200,
                failover: false,
            });
        }

        let (stats, _qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 100);
        assert!((100..=101).contains(&stats.ttfb_p50_ms));
        assert!(stats.ttfb_p95_ms >= 145);

        let recent = recent_events(20);
        assert_eq!(recent.len(), 20);
        assert_eq!(recent.last().unwrap().ttfb_ms, 150);
        assert_eq!(recent.first().unwrap().ttfb_ms, 131);
        for pair in recent.windows(2) {
            assert!(pair[0].ttfb_ms < pair[1].ttfb_ms);
        }
    }

    #[tokio::test]
    async fn test_broadcast_lag_and_replay() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let mut rx = subscribe_events();

        for i in 1..=150 {
            record_attempt(DashboardAuditEvent {
                timestamp: format!("2026-11-11T12:00:{:02}Z", (i % 60)),
                model: "gemini-2.5-flash".to_string(),
                account: "Account-2".to_string(),
                ttfb_ms: i as u64,
                total_ms: (i * 5) as u64,
                tokens: 50,
                status: 200,
                failover: false,
            });
        }

        let recent50 = recent_events(50);
        assert_eq!(recent50.len(), 50);
        assert_eq!(recent50.last().unwrap().ttfb_ms, 150);
        assert_eq!(recent50.first().unwrap().ttfb_ms, 101);

        match rx.recv().await {
            Err(RecvError::Lagged(missed)) => {
                assert!(missed > 0);
            }
            other => panic!("expected Err(RecvError::Lagged), got {:?}", other),
        }
    }
}
