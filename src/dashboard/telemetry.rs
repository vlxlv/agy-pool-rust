use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;

mod timestamp_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(ts: &jiff::Timestamp, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(ts)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<jiff::Timestamp, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<jiff::Timestamp>().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DashboardAuditEvent {
    #[serde(default)]
    pub id: u64,
    #[serde(with = "timestamp_serde")]
    pub timestamp: jiff::Timestamp,
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
    ttfb_ms: u64,
    total_ms: u64,
}

struct TelemetryState {
    next_id: u64,
    latency_samples: VecDeque<LatencySample>,
    recent_events: VecDeque<DashboardAuditEvent>,
    qps_buckets: VecDeque<(u64, u32)>,
}

static TELEMETRY_STATE: LazyLock<Mutex<TelemetryState>> = LazyLock::new(|| {
    Mutex::new(TelemetryState {
        next_id: 0,
        latency_samples: VecDeque::with_capacity(100),
        recent_events: VecDeque::with_capacity(50),
        qps_buckets: VecDeque::with_capacity(64),
    })
});

static BROADCAST_TX: LazyLock<broadcast::Sender<DashboardAuditEvent>> = LazyLock::new(|| broadcast::channel(128).0);

/// Records one incoming client generation request into the 60-second QPS sliding window.
pub fn record_client_request() {
    let current_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(last) = state.qps_buckets.back_mut() {
        if last.0 == current_sec {
            last.1 += 1;
        } else {
            state.qps_buckets.push_back((current_sec, 1));
        }
    } else {
        state.qps_buckets.push_back((current_sec, 1));
    }

    while let Some(front) = state.qps_buckets.front() {
        if current_sec.saturating_sub(front.0) >= 60 {
            state.qps_buckets.pop_front();
        } else {
            break;
        }
    }
}

pub fn record_attempt(mut ev: DashboardAuditEvent, record_latency: bool) {
    let broadcast_ev = {
        let mut state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.next_id = state.next_id.wrapping_add(1);
        if state.next_id == 0 {
            state.next_id = 1;
        }
        ev.id = state.next_id;

        if record_latency {
            if state.latency_samples.len() >= 100 {
                state.latency_samples.pop_front();
            }
            state.latency_samples.push_back(LatencySample {
                ttfb_ms: ev.ttfb_ms,
                total_ms: ev.total_ms,
            });
        }

        if state.recent_events.len() >= 50 {
            state.recent_events.pop_front();
        }

        // Check receiver count while holding TELEMETRY_STATE so subscribe_with_replay never misses an in-flight event.
        if BROADCAST_TX.receiver_count() > 0 {
            state.recent_events.push_back(ev.clone());
            Some(ev)
        } else {
            state.recent_events.push_back(ev);
            None
        }
    };

    if let Some(ev) = broadcast_ev {
        let _ = BROADCAST_TX.send(ev);
    }
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

pub fn latency_and_qps_snapshot(_now: Instant) -> (LatencyStatsDto, f64) {
    let (mut ttfb_list, mut total_list, qps) = {
        let state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let mut ttfb = Vec::with_capacity(state.latency_samples.len());
        let mut total = Vec::with_capacity(state.latency_samples.len());
        for s in &state.latency_samples {
            ttfb.push(s.ttfb_ms);
            total.push(s.total_ms);
        }

        let current_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let total_reqs: u32 = state
            .qps_buckets
            .iter()
            .filter(|(sec, _)| current_sec.saturating_sub(*sec) < 60)
            .map(|(_, count)| *count)
            .sum();

        let qps = (total_reqs as f64 / 60.0 * 100.0).round() / 100.0;
        (ttfb, total, qps)
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
            qps,
        );
    }

    ttfb_list.sort_unstable();
    total_list.sort_unstable();

    let ttfb_p50_ms = percentile(&ttfb_list, 50.0);
    let ttfb_p95_ms = percentile(&ttfb_list, 95.0);
    let total_p50_ms = percentile(&total_list, 50.0);
    let total_p95_ms = percentile(&total_list, 95.0);

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

/// Atomically snapshots up to `limit` recent events and subscribes to future events
/// under the `TELEMETRY_STATE` lock so replay + live stream have no gap or duplicate boundary.
pub fn subscribe_with_replay(limit: usize) -> (Vec<DashboardAuditEvent>, broadcast::Receiver<DashboardAuditEvent>) {
    let state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let rx = BROADCAST_TX.subscribe();
    if limit == 0 {
        return (Vec::new(), rx);
    }
    let len = state.recent_events.len();
    let take = limit.min(len);
    let skip = len - take;
    let replay = state.recent_events.iter().skip(skip).cloned().collect();
    (replay, rx)
}

pub fn reset_for_test() {
    let mut state = TELEMETRY_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.next_id = 0;
    state.latency_samples.clear();
    state.recent_events.clear();
    state.qps_buckets.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::sync::broadcast::error::RecvError;

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
            record_attempt(
                DashboardAuditEvent {
                    id: 0,
                    timestamp: jiff::Timestamp::from_second(1700000000 + i).unwrap(),
                    model: "gemini-2.5-pro".to_string(),
                    account: "Account-1".to_string(),
                    ttfb_ms: i as u64,
                    total_ms: (i * 10) as u64,
                    tokens: 100,
                    status: 200,
                    failover: false,
                },
                true,
            );
        }

        let (stats, _qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 100);
        assert!((100..=101).contains(&stats.ttfb_p50_ms));
        assert!(stats.ttfb_p95_ms >= 145);

        let recent = recent_events(20);
        assert_eq!(recent.len(), 20);
        assert_eq!(recent.last().unwrap().ttfb_ms, 150);
        assert_eq!(recent.last().unwrap().id, 150);
        assert_eq!(recent.first().unwrap().ttfb_ms, 131);
        assert_eq!(recent.first().unwrap().id, 131);
        for pair in recent.windows(2) {
            assert!(pair[0].ttfb_ms < pair[1].ttfb_ms);
            assert!(pair[0].id < pair[1].id);
        }
    }

    #[test]
    fn test_client_request_qps_separation_and_latency_gating() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();

        // Recording attempts with record_latency=false (e.g. failed failovers or metadata calls)
        // must NOT pollute latency samples or QPS.
        for _ in 0..5 {
            record_attempt(
                DashboardAuditEvent {
                    id: 0,
                    timestamp: jiff::Timestamp::now(),
                    model: "gemini-2.5-flash".to_string(),
                    account: "Account-1".to_string(),
                    ttfb_ms: 0,
                    total_ms: 0,
                    tokens: 0,
                    status: 429,
                    failover: true,
                },
                false,
            );
        }
        let (stats, qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 0);
        assert_eq!(qps, 0.0);

        // Recording incoming client generation requests increments QPS even when sample_count == 0.
        for _ in 0..6 {
            record_client_request();
        }
        let (stats, qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 0);
        assert_eq!(qps, 0.1);

        // Completed generation attempt records latency without double-counting QPS.
        record_attempt(
            DashboardAuditEvent {
                id: 0,
                timestamp: jiff::Timestamp::now(),
                model: "gemini-2.5-flash".to_string(),
                account: "Account-1".to_string(),
                ttfb_ms: 120,
                total_ms: 480,
                tokens: 55,
                status: 200,
                failover: false,
            },
            true,
        );
        let (stats, qps) = latency_and_qps_snapshot(Instant::now());
        assert_eq!(stats.sample_count, 1);
        assert_eq!(stats.ttfb_p50_ms, 120);
        assert_eq!(stats.total_p50_ms, 480);
        assert_eq!(qps, 0.1);
    }

    #[tokio::test]
    async fn test_broadcast_lag_and_replay() {
        let mut rx = {
            let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            reset_for_test();
            let (initial, rx) = subscribe_with_replay(20);
            assert!(initial.is_empty());

            for i in 1..=150 {
                record_attempt(
                    DashboardAuditEvent {
                        id: 0,
                        timestamp: jiff::Timestamp::from_second(1700000000 + i).unwrap(),
                        model: "gemini-2.5-flash".to_string(),
                        account: "Account-2".to_string(),
                        ttfb_ms: i as u64,
                        total_ms: (i * 5) as u64,
                        tokens: 50,
                        status: 200,
                        failover: false,
                    },
                    true,
                );
            }

            let (recent20, mut rx2) = subscribe_with_replay(20);
            assert_eq!(recent20.len(), 20);
            assert_eq!(recent20.last().unwrap().id, 150);
            let last_replayed_id = recent20.last().unwrap().id;

            record_attempt(
                DashboardAuditEvent {
                    id: 0,
                    timestamp: jiff::Timestamp::from_second(1700000200).unwrap(),
                    model: "gemini-2.5-flash".to_string(),
                    account: "Account-2".to_string(),
                    ttfb_ms: 151,
                    total_ms: 755,
                    tokens: 50,
                    status: 200,
                    failover: false,
                },
                true,
            );
            let next_ev = rx2.try_recv().expect("live event received after atomic replay");
            assert_eq!(next_ev.id, last_replayed_id + 1);

            let recent50 = recent_events(50);
            assert_eq!(recent50.len(), 50);
            assert_eq!(recent50.last().unwrap().ttfb_ms, 151);
            assert_eq!(recent50.first().unwrap().ttfb_ms, 102);
            rx
        };

        match rx.recv().await {
            Err(RecvError::Lagged(missed)) => {
                assert!(missed > 0);
            }
            other => panic!("expected Err(RecvError::Lagged), got {:?}", other),
        }
    }
}
