#![allow(clippy::collapsible_if)]

use std::borrow::Borrow;
use std::time::SystemTime;

use crate::accounts::display_account_name;
use crate::config::{STRATEGY_MAX_QUOTA, STRATEGY_SMART};
use crate::diagnostics::routing_audit::RoutingAuditEvent;
use crate::proxy::RoutingEvent;
use crate::storage::Pool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountTier {
    Healthy,
    Reserve,
    Depleted,
    Unknown,
}

impl AccountTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountTier::Healthy => "healthy",
            AccountTier::Reserve => "reserve",
            AccountTier::Depleted => "depleted",
            AccountTier::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditVerdict {
    Pass,
    Warning,
    Violation,
}

impl AuditVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditVerdict::Pass => "pass",
            AuditVerdict::Warning => "warning",
            AuditVerdict::Violation => "violation",
        }
    }
}

pub fn evaluate_routing_attempt(
    pool: Option<&Pool>,
    account_name: &str,
    status: u16,
    attempt: usize,
    failover_reason: &str,
    timestamp: SystemTime,
    request_id: &str,
) -> RoutingAuditEvent {
    let now = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let strategy = pool
        .map(|p| {
            if !p.strategy.is_empty() {
                p.strategy.clone()
            } else {
                STRATEGY_MAX_QUOTA.to_string()
            }
        })
        .unwrap_or_else(|| STRATEGY_MAX_QUOTA.to_string());

    let mut event = RoutingAuditEvent {
        timestamp,
        request_id: if request_id.is_empty() {
            None
        } else {
            Some(request_id.to_string())
        },
        strategy: strategy.clone(),
        account: account_name.to_string(),
        account_id: None,
        classification: String::new(),
        five_hour_known: false,
        five_hour_remaining: None,
        five_hour_reset_at: None,
        five_hour_reset_ratio: None,
        five_hour_pace: None,
        weekly_known: false,
        weekly_remaining: None,
        weekly_reset_at: None,
        weekly_reset_ratio: None,
        weekly_pace: None,
        known_window_count: 0,
        freshness_rank: None,
        worst_pace: None,
        total_pace: None,
        raw_floor: None,
        healthy_count: 0,
        reserve_count: 0,
        healthy_candidates: Vec::new(),
        reserve_candidates: Vec::new(),
        attempt: if attempt == 0 { 1 } else { attempt },
        status,
        failover_reason: failover_reason.to_string(),
        verdict: String::new(),
        verdict_details: String::new(),
    };

    let pool = match pool {
        Some(p) if !p.accounts.is_empty() => p,
        _ => {
            event.classification = "unknown".to_string();
            event.verdict = "warning".to_string();
            event.verdict_details = "Empty or uninitialized pool state".to_string();
            return event;
        }
    };

    // 1. Identify selected account
    let selected_acc = pool.accounts.iter().find(|acc| {
        let disp = display_account_name(Some(acc), Some(false));
        disp.eq_ignore_ascii_case(account_name)
            || acc.id.eq_ignore_ascii_case(account_name)
            || acc.name.eq_ignore_ascii_case(account_name)
    });

    let selected_acc = match selected_acc {
        Some(acc) => acc,
        None => {
            event.classification = "unknown".to_string();
            event.verdict = "warning".to_string();
            event.verdict_details = format!("Selected account {:?} not found in state.db", account_name);
            return event;
        }
    };

    event.account = display_account_name(Some(selected_acc), Some(false));
    event.account_id = Some(selected_acc.id.clone());

    // 2. Classify selected account using authoritative scheduler and quota primitives
    let cap_state = crate::quota::compute_capacity_state(selected_acc, now);
    event.five_hour_known = cap_state.q5_known;
    event.five_hour_remaining = cap_state.q5;
    event.five_hour_reset_at = cap_state.reset5_time.map(|t| {
        jiff::Timestamp::try_from(t)
            .map(|ts| ts.to_string())
            .unwrap_or_default()
    });
    event.five_hour_pace = cap_state.pace5;
    if cap_state.q5.is_some() {
        event.five_hour_reset_ratio = Some(cap_state.r5);
    }

    event.weekly_known = cap_state.q7_known;
    event.weekly_remaining = cap_state.q7;
    event.weekly_reset_at = cap_state.reset7_time.map(|t| {
        jiff::Timestamp::try_from(t)
            .map(|ts| ts.to_string())
            .unwrap_or_default()
    });
    event.weekly_pace = cap_state.pace7;
    if cap_state.q7.is_some() {
        event.weekly_reset_ratio = Some(cap_state.r7);
    }

    event.known_window_count = cap_state.known_window_count;
    if !cap_state.worst_pace.is_infinite() {
        event.worst_pace = Some(cap_state.worst_pace);
    }
    if !cap_state.total_pace.is_infinite() {
        event.total_pace = Some(cap_state.total_pace);
    }
    event.raw_floor = Some(cap_state.raw_floor);
    let freshness = crate::quota::freshness_rank(selected_acc, now);
    event.freshness_rank = Some(freshness);

    let classification = if cap_state.is_depleted {
        AccountTier::Depleted
    } else if crate::scheduler::is_reserve_candidate(selected_acc, now) {
        AccountTier::Reserve
    } else if cap_state.known_window_count > 0 || cap_state.legacy_fraction_known {
        AccountTier::Healthy
    } else {
        AccountTier::Unknown
    };
    event.classification = classification.as_str().to_string();

    // 3. Find Healthy and Reserve candidates available in pool
    let mut healthy_candidates = Vec::new();
    let mut reserve_candidates = Vec::new();

    for cand in &pool.accounts {
        if cand.status == crate::storage::STATUS_VALIDATION_REQUIRED
            || cand.status == crate::storage::STATUS_VERIFICATION_PENDING
            || cand.status == crate::storage::STATUS_AUTH_ERROR
            || cand.status == crate::storage::STATUS_DISABLED
        {
            continue;
        }
        if cand.rate_limited_until.is_some_and(|rl| rl > now) {
            continue;
        }
        let cand_cap = crate::quota::compute_capacity_state(cand, now);
        if cand_cap.is_depleted {
            continue;
        }
        let cand_disp = display_account_name(Some(cand), Some(false));
        if crate::scheduler::is_reserve_capacity(&cand_cap) {
            reserve_candidates.push(cand_disp);
        } else if cand_cap.known_window_count > 0 || cand_cap.legacy_fraction_known {
            healthy_candidates.push(cand_disp);
        }
    }

    event.healthy_count = healthy_candidates.len();
    event.reserve_count = reserve_candidates.len();
    event.healthy_candidates = healthy_candidates.clone();
    event.reserve_candidates = reserve_candidates.clone();

    // 4. Violation semantics
    if strategy == STRATEGY_MAX_QUOTA || strategy == STRATEGY_SMART {
        match classification {
            AccountTier::Healthy => {
                event.verdict = "pass".to_string();
            }
            AccountTier::Reserve => {
                let has_other_healthy = healthy_candidates
                    .iter()
                    .any(|hc| !hc.eq_ignore_ascii_case(&event.account));
                if has_other_healthy {
                    event.verdict = "violation".to_string();
                    event.verdict_details = format!(
                        "Reserve account {} selected while Healthy candidates were available: {}",
                        event.account,
                        healthy_candidates.join(", ")
                    );
                } else {
                    event.verdict = "pass".to_string();
                    event.verdict_details =
                        "Reserve account selected as fallback (no Healthy candidates available)".to_string();
                }
            }
            AccountTier::Depleted => {
                if !healthy_candidates.is_empty() || !reserve_candidates.is_empty() {
                    event.verdict = "violation".to_string();
                    event.verdict_details = format!(
                        "Depleted account {} selected while Healthy or Reserve candidates were available",
                        event.account
                    );
                } else {
                    event.verdict = "pass".to_string();
                    event.verdict_details = "Depleted account selected (all accounts depleted)".to_string();
                }
            }
            AccountTier::Unknown => {
                event.verdict = "warning".to_string();
                event.verdict_details =
                    "Selected account quota classification cannot be determined from authoritative state".to_string();
            }
        }
    } else {
        event.verdict = "pass".to_string();
        event.verdict_details = format!("Reserve headroom checks not applicable to strategy {}", strategy);
    }

    event
}

pub fn evaluate_frozen_routing_event(ev: impl Borrow<RoutingEvent>) -> RoutingAuditEvent {
    let ev = ev.borrow();
    let strategy = if !ev.strategy.is_empty() {
        ev.strategy.clone()
    } else {
        STRATEGY_MAX_QUOTA.to_string()
    };

    let acc_name = if !ev.account.is_empty() {
        &ev.account
    } else {
        &ev.account_name
    };

    let healthy_count = if ev.healthy_candidate_count == 0 && !ev.healthy_candidates.is_empty() {
        ev.healthy_candidates.len()
    } else {
        ev.healthy_candidate_count
    };

    let reserve_count = if ev.reserve_candidate_count == 0 && !ev.reserve_candidates.is_empty() {
        ev.reserve_candidates.len()
    } else {
        ev.reserve_candidate_count
    };

    let mut audit_ev = RoutingAuditEvent {
        timestamp: ev.timestamp,
        request_id: None,
        strategy: strategy.clone(),
        account: acc_name.clone(),
        account_id: ev.account_id.clone(),
        classification: ev.classification.to_lowercase(),
        five_hour_known: ev.five_hour_known,
        five_hour_remaining: ev.five_hour_remaining,
        five_hour_reset_at: ev.five_hour_reset_at.clone(),
        five_hour_reset_ratio: ev.five_hour_reset_ratio,
        five_hour_pace: ev.five_hour_pace,
        weekly_known: ev.weekly_known,
        weekly_remaining: ev.weekly_remaining,
        weekly_reset_at: ev.weekly_reset_at.clone(),
        weekly_reset_ratio: ev.weekly_reset_ratio,
        weekly_pace: ev.weekly_pace,
        known_window_count: ev.known_window_count,
        freshness_rank: ev.freshness_rank,
        worst_pace: ev.worst_pace,
        total_pace: ev.total_pace,
        raw_floor: ev.raw_floor,
        healthy_count,
        reserve_count,
        healthy_candidates: ev.healthy_candidates.clone(),
        reserve_candidates: ev.reserve_candidates.clone(),
        attempt: if ev.attempt == 0 { 1 } else { ev.attempt },
        status: ev.status,
        failover_reason: ev.failover_reason.clone(),
        verdict: String::new(),
        verdict_details: String::new(),
    };

    let tier_lower = ev.classification.to_lowercase();
    if strategy == STRATEGY_MAX_QUOTA || strategy == STRATEGY_SMART {
        match tier_lower.as_str() {
            "healthy" => {
                audit_ev.verdict = "pass".to_string();
                audit_ev.verdict_details = "Healthy account selected".to_string();
            }
            "reserve" => {
                let mut has_other_healthy = ev
                    .healthy_candidates
                    .iter()
                    .any(|hc| !hc.eq_ignore_ascii_case(acc_name));
                if !has_other_healthy && healthy_count > 0 {
                    has_other_healthy = true;
                }
                if has_other_healthy {
                    audit_ev.verdict = "violation".to_string();
                    let healthy_list = if !ev.healthy_candidates.is_empty() {
                        ev.healthy_candidates.join(", ")
                    } else {
                        format!("{} healthy candidates", healthy_count)
                    };
                    audit_ev.verdict_details = format!(
                        "Reserve account {} selected while Healthy candidates were available: {}",
                        acc_name, healthy_list
                    );
                } else {
                    audit_ev.verdict = "pass".to_string();
                    audit_ev.verdict_details =
                        "Reserve account selected as fallback (no Healthy candidates available)".to_string();
                }
            }
            "depleted" => {
                if healthy_count > 0
                    || !ev.healthy_candidates.is_empty()
                    || reserve_count > 0
                    || !ev.reserve_candidates.is_empty()
                {
                    audit_ev.verdict = "violation".to_string();
                    audit_ev.verdict_details = format!(
                        "Depleted account {} selected while Healthy or Reserve candidates were available",
                        acc_name
                    );
                } else {
                    audit_ev.verdict = "pass".to_string();
                    audit_ev.verdict_details = "Depleted account selected (all accounts depleted)".to_string();
                }
            }
            _ => {
                audit_ev.verdict = "warning".to_string();
                audit_ev.verdict_details =
                    "Selected account quota classification cannot be determined from selection-time metadata"
                        .to_string();
            }
        }
    } else {
        audit_ev.verdict = "pass".to_string();
        audit_ev.verdict_details = format!("Reserve headroom checks not applicable to strategy {}", strategy);
    }

    audit_ev
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use crate::storage::{Account, Pool, QuotaState, QuotaWindow};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Duration;

    fn make_test_account(id: &str, name: &str, q5: Option<f64>, qw: Option<f64>, now: f64) -> Account {
        let mut lq = QuotaState {
            updated_at: Some(now as i64 - 10),
            ..Default::default()
        };
        if let Some(f5) = q5 {
            lq.gemini_5h = Some(QuotaWindow {
                fraction: Some(f5),
                reset_time: Some(serde_json::Value::String("2026-09-17T20:00:00Z".to_string())),
                ..Default::default()
            });
        }
        if let Some(fw) = qw {
            lq.gemini_weekly = Some(QuotaWindow {
                fraction: Some(fw),
                reset_time: Some(serde_json::Value::String("2026-09-24T00:00:00Z".to_string())),
                ..Default::default()
            });
        }
        Account {
            id: id.to_string(),
            name: name.to_string(),
            status: "active".to_string(),
            last_quota: Some(lq),
            ..Default::default()
        }
    }

    #[test]
    fn test_audit_a_healthy_selection_pass() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_work", "Work", Some(0.80), Some(0.70), now),
                make_test_account("acc_backup", "Backup", Some(0.90), Some(0.70), now),
                make_test_account("acc_main", "Main", Some(0.10), Some(0.50), now), // Reserve
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Work", 200, 1, "", ts, "req-1");
        assert_eq!(ev.classification, "healthy");
        assert_eq!(ev.verdict, "pass");
    }

    #[test]
    fn test_audit_b_reserve_while_healthy_exists_violation() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_backup", "Backup", Some(0.90), Some(0.70), now), // Healthy
                make_test_account("acc_main", "Main", Some(0.10), Some(0.50), now),     // Reserve
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Main", 200, 1, "", ts, "req-2");
        assert_eq!(ev.classification, "reserve");
        assert_eq!(ev.verdict, "violation");
        assert!(ev.verdict_details.contains("Healthy candidates were available"));
    }

    #[test]
    fn test_audit_c_reserve_fallback_pass() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_main", "Main", Some(0.10), Some(0.50), now), // Reserve
                make_test_account("acc_r2", "Backup", Some(0.08), Some(0.50), now), // Reserve
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Main", 200, 1, "", ts, "req-3");
        assert_eq!(ev.classification, "reserve");
        assert_eq!(ev.verdict, "pass");
        assert!(ev.verdict_details.contains("fallback"));
    }

    #[test]
    fn test_audit_d_depleted_while_reserve_exists_violation() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_dep", "DepletedAcc", Some(0.002), Some(0.50), now), // Depleted (<=0.005)
                make_test_account("acc_main", "Main", Some(0.10), Some(0.50), now),        // Reserve
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "DepletedAcc", 200, 1, "", ts, "req-4");
        assert_eq!(ev.classification, "depleted");
        assert_eq!(ev.verdict, "violation");
    }

    #[test]
    fn test_audit_e_unknown_classification_warning() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![make_test_account("acc_work", "Work", Some(0.80), Some(0.70), now)],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "GhostAccount", 200, 1, "", ts, "req-5");
        assert_eq!(ev.classification, "unknown");
        assert_eq!(ev.verdict, "warning");
    }

    #[test]
    fn test_audit_f_legacy_unidentified_windows() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let legacy12 = Account {
            id: "legacy_12".to_string(),
            name: "Legacy12".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.12),
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let explicit_5h12 = make_test_account("explicit_5h", "Explicit5H", Some(0.12), None, now);
        let explicit_weekly12 = make_test_account("explicit_weekly", "ExplicitWeekly", None, Some(0.12), now);
        let unidentified_acc = Account {
            id: "unidentified".to_string(),
            name: "Unidentified".to_string(),
            status: "active".to_string(),
            ..Default::default()
        };

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![legacy12, explicit_5h12, explicit_weekly12, unidentified_acc],
            ..Default::default()
        };

        // 1. Legacy fraction 0.12 retains old behavior: not Reserve -> Healthy, PASS
        let ev1 = evaluate_routing_attempt(Some(&pool), "Legacy12", 200, 1, "", ts, "");
        assert_eq!(ev1.classification, "healthy");
        assert_eq!(ev1.verdict, "pass");

        // 2. Explicit 5H 0.12 has identified 5H window -> Reserve; with ExplicitWeekly12 (Healthy) available -> VIOLATION
        let ev2 = evaluate_routing_attempt(Some(&pool), "Explicit5H", 200, 1, "", ts, "");
        assert_eq!(ev2.classification, "reserve");
        assert_eq!(ev2.verdict, "violation");

        // 3. Explicit Weekly 0.12 has identified Weekly window -> Healthy (>10%), PASS
        let ev3 = evaluate_routing_attempt(Some(&pool), "ExplicitWeekly", 200, 1, "", ts, "");
        assert_eq!(ev3.classification, "healthy");
        assert_eq!(ev3.verdict, "pass");

        // 4. Unidentified account (no quota known) -> WARNING
        let ev4 = evaluate_routing_attempt(Some(&pool), "Unidentified", 200, 1, "", ts, "");
        assert_eq!(ev4.classification, "unknown");
        assert_eq!(ev4.verdict, "warning");
    }

    #[test]
    fn test_audit_g_smart_reserve_violations() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: STRATEGY_SMART.to_string(),
            accounts: vec![
                make_test_account("acc_work", "Work", Some(0.10), Some(0.50), now), // 10% (Reserve)
                make_test_account("acc_backup", "Backup", Some(0.90), Some(0.70), now), // 90% (Healthy)
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Work", 200, 1, "", ts, "");
        assert_eq!(ev.verdict, "violation");
        assert!(ev.verdict_details.contains("Reserve account"));
    }

    #[test]
    fn test_audit_h_other_strategy_no_reserve_violations() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);

        let pool = Pool {
            strategy: "custom_unmanaged".to_string(),
            accounts: vec![
                make_test_account("acc_work", "Work", Some(0.10), Some(0.50), now), // 10%
                make_test_account("acc_backup", "Backup", Some(0.90), Some(0.70), now), // 90%
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Work", 200, 1, "", ts, "");
        assert_eq!(ev.verdict, "pass");
    }

    #[test]
    fn test_audit_i_failover_two_attempts_without_false_violation() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let ts1 = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64);
        let ts2 = SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64 + 1);

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_work", "Work", Some(0.95), Some(0.80), now),
                make_test_account("acc_backup", "Backup", Some(0.90), Some(0.80), now),
            ],
            ..Default::default()
        };

        // Attempt 1: Work hit rate limit (429)
        let ev1 = evaluate_routing_attempt(Some(&pool), "Work", 429, 1, "rate limit/quota error", ts1, "req-1");
        assert_eq!(ev1.verdict, "pass");
        assert_eq!(ev1.status, 429);

        // Attempt 2: Backup succeeded (200)
        let ev2 = evaluate_routing_attempt(Some(&pool), "Backup", 200, 2, "", ts2, "req-1");
        assert_eq!(ev2.verdict, "pass");
        assert_eq!(ev2.status, 200);
        assert_eq!(ev2.attempt, 2);
    }

    #[tokio::test]
    async fn test_audit_j_event_consumer_cannot_block_generation_path() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let call_count = Arc::new(AtomicI64::new(0));
        let cc = Arc::clone(&call_count);
        let (tx_gate, rx_gate) = std::sync::mpsc::channel();
        let rx_gate = Arc::new(std::sync::Mutex::new(rx_gate));

        let slow_obs = move |_acc: &str, _status: i32, _attempt: i32, _reason: &str, _path: &str| {
            cc.fetch_add(1, Ordering::SeqCst);
            if let Ok(rx) = rx_gate.lock() {
                let _ = rx.recv();
            }
        };

        crate::proxy::set_audit_observer(Some(slow_obs));
        crate::proxy::reset_dropped_audit_events();

        struct Guard(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                drop(self.0.take());
                crate::proxy::set_audit_observer(None::<fn(&str, i32, i32, &str, &str)>);
            }
        }
        let _guard = Guard(Some(tx_gate));

        let start = std::time::Instant::now();
        let ok = crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "MockAcc".to_string(),
            account_name: "MockAcc".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["MockAcc".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });
        let elapsed = start.elapsed();
        assert!(ok, "expected first event to be queued successfully");
        assert!(
            elapsed < Duration::from_millis(50),
            "audit observer blocked generation path: took {:?}",
            elapsed
        );

        // Overfill buffer
        for _ in 0..1100 {
            crate::proxy::emit_routing_event(RoutingEvent {
                timestamp: SystemTime::now(),
                strategy: "max_quota".to_string(),
                account: "MockAcc".to_string(),
                account_name: "MockAcc".to_string(),
                account_id: None,
                classification: "Healthy".to_string(),
                healthy_candidate_count: 1,
                healthy_candidates: vec!["MockAcc".to_string()],
                reserve_candidate_count: 0,
                reserve_candidates: Vec::new(),
                five_hour_known: false,
                five_hour_remaining: None,
                five_hour_reset_at: None,
                five_hour_reset_ratio: None,
                five_hour_pace: None,
                weekly_known: false,
                weekly_remaining: None,
                weekly_reset_at: None,
                weekly_reset_ratio: None,
                weekly_pace: None,
                known_window_count: 0,
                freshness_rank: None,
                worst_pace: None,
                total_pace: None,
                raw_floor: None,
                attempt: 1,
                status: 200,
                failover_reason: String::new(),
                path: "/v1internal:streamGenerateContent".to_string(),
            });
        }
        let total_elapsed = start.elapsed();
        assert!(
            total_elapsed < Duration::from_millis(150),
            "overfilling buffer took too long: {:?}",
            total_elapsed
        );
    }

    #[test]
    fn test_audit_k_sensitive_data_never_exposed() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let acc = Account {
            id: "acc_secret".to_string(),
            name: "SafeDisplayName".to_string(),
            email: "user.private.secret@corp.example.com".to_string(),
            access_token: "ya29.secret_token_12345".to_string(),
            refresh_token: "1//refresh_secret_abcdef".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.85),
                updated_at: Some(now as i64 - 5),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "SafeDisplayName", 200, 1, "", SystemTime::now(), "req-sec");
        let json_str = serde_json::to_string(&ev).unwrap();

        let forbidden = [
            "user.private.secret",
            "example.com",
            "ya29.",
            "refresh_secret",
            "secret_token",
            "Authorization",
            "Bearer",
        ];

        for token in forbidden {
            assert!(
                !json_str.contains(token),
                "security violation: emitted event contains sensitive token {:?}",
                token
            );
        }
    }

    #[test]
    fn test_audit_l_json_output_valid() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789560000.0;
        let pool = Pool {
            strategy: STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![
                make_test_account("acc_work", "Work", Some(0.824), Some(0.651), now),
                make_test_account("acc_backup", "Backup", Some(0.962), Some(0.669), now),
            ],
            ..Default::default()
        };

        let ev = evaluate_routing_attempt(Some(&pool), "Work", 200, 1, "", SystemTime::now(), "req-json");
        let json_bytes = serde_json::to_vec(&ev).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();

        let required_fields = [
            "timestamp",
            "strategy",
            "account",
            "classification",
            "five_hour_remaining",
            "weekly_remaining",
            "healthy_candidates",
            "attempt",
            "status",
            "verdict",
        ];
        for f in required_fields {
            assert!(parsed.get(f).is_some(), "missing required field in JSON: {:?}", f);
        }
    }

    #[test]
    fn test_temporal_audit_a_healthy_selection_remains_pass_after_state_degrades() {
        let env = crate::daemon::test_env();
        let ev = RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "Work".to_string(),
            account_name: "Work".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Work".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: Some(0.85),
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: Some(0.90),
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        };

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let degraded_acc = make_test_account("acc_work", "Work", Some(0.05), Some(0.05), now);
        let _ = crate::storage::save_pool(&Pool {
            accounts: vec![degraded_acc],
            strategy: "max_quota".to_string(),
            ..Default::default()
        });

        let audit_ev = evaluate_frozen_routing_event(&ev);
        assert_eq!(audit_ev.verdict, "pass");
        drop(env);
    }

    #[test]
    fn test_temporal_audit_b_reserve_violation_remains_violation_after_state_changes() {
        let env = crate::daemon::test_env();
        let ev = RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "Work".to_string(),
            account_name: "Work".to_string(),
            account_id: None,
            classification: "Reserve".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Backup".to_string()],
            reserve_candidate_count: 1,
            reserve_candidates: vec!["Work".to_string()],
            five_hour_known: false,
            five_hour_remaining: Some(0.10),
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: Some(0.50),
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        };

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let depleted_acc1 = make_test_account("acc_work", "Work", Some(0.0), Some(0.0), now);
        let depleted_acc2 = make_test_account("acc_backup", "Backup", Some(0.0), Some(0.0), now);
        let _ = crate::storage::save_pool(&Pool {
            accounts: vec![depleted_acc1, depleted_acc2],
            strategy: "max_quota".to_string(),
            ..Default::default()
        });

        let audit_ev = evaluate_frozen_routing_event(&ev);
        assert_eq!(audit_ev.verdict, "violation");
        drop(env);
    }

    #[tokio::test]
    async fn test_temporal_audit_c_pre_session_events_not_reported() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::proxy::set_audit_observer(None::<fn(&str, i32, i32, &str, &str)>);
        crate::proxy::set_audit_observer_event(None::<fn(RoutingEvent)>);
        crate::proxy::reset_dropped_audit_events();

        // Emit event prior to session starting
        crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "OldAccount".to_string(),
            account_name: "OldAccount".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["OldAccount".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });

        // Start a new audit session via subscribe_audit
        let (mut sub_events, _sub) = crate::proxy::subscribe_audit(100);

        // Emit new event after session starts
        crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "NewAccount".to_string(),
            account_name: "NewAccount".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["NewAccount".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });

        let received = tokio::time::timeout(Duration::from_millis(200), sub_events.recv())
            .await
            .expect("timed out waiting for session event")
            .expect("stream closed");
        assert_eq!(received.account, "NewAccount");

        // Verify no additional events
        assert!(sub_events.try_recv().is_err());
    }

    #[test]
    fn test_temporal_audit_d_no_active_auditor_does_not_fill_queue() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::proxy::set_audit_observer(None::<fn(&str, i32, i32, &str, &str)>);
        crate::proxy::set_audit_observer_event(None::<fn(RoutingEvent)>);
        crate::proxy::reset_dropped_audit_events();

        assert!(!crate::proxy::has_active_auditor());
        let initial_queue_len = crate::proxy::audit_queue_len();

        for _ in 0..50 {
            let delivered = crate::proxy::emit_routing_event(RoutingEvent {
                timestamp: SystemTime::now(),
                strategy: "max_quota".to_string(),
                account: "MockAcc".to_string(),
                account_name: "MockAcc".to_string(),
                account_id: None,
                classification: "Healthy".to_string(),
                healthy_candidate_count: 1,
                healthy_candidates: vec!["MockAcc".to_string()],
                reserve_candidate_count: 0,
                reserve_candidates: Vec::new(),
                five_hour_known: false,
                five_hour_remaining: None,
                five_hour_reset_at: None,
                five_hour_reset_ratio: None,
                five_hour_pace: None,
                weekly_known: false,
                weekly_remaining: None,
                weekly_reset_at: None,
                weekly_reset_ratio: None,
                weekly_pace: None,
                known_window_count: 0,
                freshness_rank: None,
                worst_pace: None,
                total_pace: None,
                raw_floor: None,
                attempt: 1,
                status: 200,
                failover_reason: String::new(),
                path: "/v1internal:streamGenerateContent".to_string(),
            });
            assert!(
                !delivered,
                "expected emit_routing_event to return false when no auditor active"
            );
        }

        assert_eq!(crate::proxy::audit_queue_len(), initial_queue_len);
        assert_eq!(crate::proxy::dropped_audit_events(), 0);
    }

    #[tokio::test]
    async fn test_temporal_audit_e_blocked_consumer_cannot_delay_generation() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let slow_obs = move |_ev: RoutingEvent| {
            if let Ok(r) = rx.lock() {
                let _ = r.recv();
            }
        };

        crate::proxy::set_audit_observer_event(Some(slow_obs));
        struct Cleanup(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                drop(self.0.take());
                crate::proxy::set_audit_observer_event(None::<fn(RoutingEvent)>);
            }
        }
        let _clean = Cleanup(Some(tx));

        let start = std::time::Instant::now();
        crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "Work".to_string(),
            account_name: "Work".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Work".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "blocked consumer delayed generation: took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_temporal_audit_f_queue_overflow_increments_dropped_count_without_blocking() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Use a subscriber with a small buffer
        let (_rx, _sub) = crate::proxy::subscribe_audit(10);
        crate::proxy::reset_dropped_audit_events();

        let start = std::time::Instant::now();
        for _ in 0..1200 {
            crate::proxy::emit_routing_event(RoutingEvent {
                timestamp: SystemTime::now(),
                strategy: "max_quota".to_string(),
                account: "Work".to_string(),
                account_name: "Work".to_string(),
                account_id: None,
                classification: "Healthy".to_string(),
                healthy_candidate_count: 1,
                healthy_candidates: vec!["Work".to_string()],
                reserve_candidate_count: 0,
                reserve_candidates: Vec::new(),
                five_hour_known: false,
                five_hour_remaining: None,
                five_hour_reset_at: None,
                five_hour_reset_ratio: None,
                five_hour_pace: None,
                weekly_known: false,
                weekly_remaining: None,
                weekly_reset_at: None,
                weekly_reset_ratio: None,
                weekly_pace: None,
                known_window_count: 0,
                freshness_rank: None,
                worst_pace: None,
                total_pace: None,
                raw_floor: None,
                attempt: 1,
                status: 200,
                failover_reason: String::new(),
                path: "/v1internal:streamGenerateContent".to_string(),
            });
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "overfilling queue blocked: took {:?}",
            elapsed
        );
        assert!(crate::proxy::dropped_audit_events() > 0);
    }

    #[test]
    fn test_frozen_reset_timestamps_and_ranking_inputs() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1789700000.0;
        let reset5_str = "2026-09-17T20:00:00Z";
        let reset7_str = "2026-09-24T00:00:00Z";
        let q5 = 0.85;
        let qw = 0.92;

        let acc = Account {
            id: "acc_123".to_string(),
            name: "Work".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(q5),
                    reset_time: Some(serde_json::Value::String(reset5_str.to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(qw),
                    reset_time: Some(serde_json::Value::String(reset7_str.to_string())),
                    ..Default::default()
                }),
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };

        let cap_state = crate::quota::compute_capacity_state(&acc, now);
        assert!(cap_state.reset5_time.is_some());
        assert!(cap_state.reset7_time.is_some());

        let r5_str = jiff::Timestamp::try_from(cap_state.reset5_time.unwrap())
            .unwrap()
            .to_string();
        let r7_str = jiff::Timestamp::try_from(cap_state.reset7_time.unwrap())
            .unwrap()
            .to_string();
        assert_eq!(r5_str, reset5_str);
        assert_eq!(r7_str, reset7_str);
        assert!(cap_state.pace5.is_some() && cap_state.pace7.is_some());

        let freshness = crate::quota::freshness_rank(&acc, now);
        let wp = cap_state.worst_pace;
        let tp = cap_state.total_pace;
        let rf = cap_state.raw_floor;
        let r5 = cap_state.r5;
        let r7 = cap_state.r7;

        let ev = RoutingEvent {
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_secs(now as u64),
            strategy: "max_quota".to_string(),
            account: "Work".to_string(),
            account_name: "Work".to_string(),
            account_id: Some(acc.id.clone()),
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Work".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: cap_state.q5_known,
            five_hour_remaining: cap_state.q5,
            five_hour_reset_at: Some(reset5_str.to_string()),
            five_hour_reset_ratio: Some(r5),
            five_hour_pace: cap_state.pace5,
            weekly_known: cap_state.q7_known,
            weekly_remaining: cap_state.q7,
            weekly_reset_at: Some(reset7_str.to_string()),
            weekly_reset_ratio: Some(r7),
            weekly_pace: cap_state.pace7,
            known_window_count: cap_state.known_window_count,
            freshness_rank: Some(freshness),
            worst_pace: Some(wp),
            total_pace: Some(tp),
            raw_floor: Some(rf),
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        };

        let audit_ev = evaluate_frozen_routing_event(&ev);
        assert_eq!(audit_ev.five_hour_reset_at.as_deref(), Some(reset5_str));
        assert_eq!(audit_ev.weekly_reset_at.as_deref(), Some(reset7_str));
        assert_eq!(audit_ev.five_hour_pace, cap_state.pace5);
        assert_eq!(audit_ev.weekly_pace, cap_state.pace7);
        assert_eq!(audit_ev.worst_pace, Some(wp));
        assert_eq!(audit_ev.total_pace, Some(tp));
        assert_eq!(audit_ev.raw_floor, Some(rf));
        assert_eq!(audit_ev.freshness_rank, Some(freshness));

        let json_str = serde_json::to_string(&audit_ev).unwrap();
        let expected_keys = [
            "five_hour_reset_at",
            "weekly_reset_at",
            "five_hour_pace",
            "weekly_pace",
            "five_hour_reset_ratio",
            "weekly_reset_ratio",
            "worst_pace",
            "total_pace",
            "raw_floor",
            "freshness_rank",
            "healthy_count",
            "reserve_count",
        ];
        for key in expected_keys {
            assert!(
                json_str.contains(key),
                "expected JSON to contain {:?}, got:\n{}",
                key,
                json_str
            );
        }
    }

    #[tokio::test]
    async fn test_temporal_audit_i_session_stop_unregisters_and_no_stale_leak() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::proxy::set_audit_observer(None::<fn(&str, i32, i32, &str, &str)>);
        crate::proxy::set_audit_observer_event(None::<fn(RoutingEvent)>);
        crate::proxy::reset_dropped_audit_events();

        // Session 1
        let (mut sub1, cancel1) = crate::proxy::subscribe_audit(10);
        assert!(crate::proxy::has_active_auditor());

        crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "Session1Acc".to_string(),
            account_name: "Session1Acc".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Session1Acc".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });

        let ev1 = tokio::time::timeout(Duration::from_millis(200), sub1.recv())
            .await
            .expect("session 1 timed out")
            .expect("stream closed");
        assert_eq!(ev1.account, "Session1Acc");

        // Stop session 1 by dropping cancel1
        drop(cancel1);
        assert!(!crate::proxy::has_active_auditor());

        // Emit events while stopped
        for _ in 0..5 {
            crate::proxy::emit_routing_event(RoutingEvent {
                timestamp: SystemTime::now(),
                strategy: "max_quota".to_string(),
                account: "StaleAcc".to_string(),
                account_name: "StaleAcc".to_string(),
                account_id: None,
                classification: "Healthy".to_string(),
                healthy_candidate_count: 1,
                healthy_candidates: vec!["StaleAcc".to_string()],
                reserve_candidate_count: 0,
                reserve_candidates: Vec::new(),
                five_hour_known: false,
                five_hour_remaining: None,
                five_hour_reset_at: None,
                five_hour_reset_ratio: None,
                five_hour_pace: None,
                weekly_known: false,
                weekly_remaining: None,
                weekly_reset_at: None,
                weekly_reset_ratio: None,
                weekly_pace: None,
                known_window_count: 0,
                freshness_rank: None,
                worst_pace: None,
                total_pace: None,
                raw_floor: None,
                attempt: 1,
                status: 200,
                failover_reason: String::new(),
                path: "/v1internal:streamGenerateContent".to_string(),
            });
        }

        // Session 2 starts
        let (mut sub2, _cancel2) = crate::proxy::subscribe_audit(10);
        assert!(crate::proxy::has_active_auditor());

        // Ensure no stale events
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sub2.recv())
                .await
                .is_err()
        );

        // Emit new event for session 2
        crate::proxy::emit_routing_event(RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: "Session2Acc".to_string(),
            account_name: "Session2Acc".to_string(),
            account_id: None,
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec!["Session2Acc".to_string()],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: 0,
            freshness_rank: None,
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        });

        let ev2 = tokio::time::timeout(Duration::from_millis(200), sub2.recv())
            .await
            .expect("session 2 timed out")
            .expect("stream closed");
        assert_eq!(ev2.account, "Session2Acc");
    }

    #[test]
    fn test_temporal_audit_j_privacy_no_sensitive_data() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let secret_email = "developer-private@example.com";
        let secret_token = "ya29.secret_token_12345";

        let acc = Account {
            id: "acc_secret".to_string(),
            name: "ProductionWork".to_string(),
            email: secret_email.to_string(),
            access_token: secret_token.to_string(),
            refresh_token: secret_token.to_string(),
            last_quota: Some(QuotaState {
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };

        let cap_state = crate::quota::compute_capacity_state(&acc, now);
        let disp = display_account_name(Some(&acc), Some(false));
        let freshness = crate::quota::freshness_rank(&acc, now);

        let ev = RoutingEvent {
            timestamp: SystemTime::now(),
            strategy: "max_quota".to_string(),
            account: disp.clone(),
            account_name: disp.clone(),
            account_id: Some(acc.id.clone()),
            classification: "Healthy".to_string(),
            healthy_candidate_count: 1,
            healthy_candidates: vec![disp],
            reserve_candidate_count: 0,
            reserve_candidates: Vec::new(),
            five_hour_known: false,
            five_hour_remaining: None,
            five_hour_reset_at: None,
            five_hour_reset_ratio: None,
            five_hour_pace: None,
            weekly_known: false,
            weekly_remaining: None,
            weekly_reset_at: None,
            weekly_reset_ratio: None,
            weekly_pace: None,
            known_window_count: cap_state.known_window_count,
            freshness_rank: Some(freshness),
            worst_pace: None,
            total_pace: None,
            raw_floor: None,
            attempt: 1,
            status: 200,
            failover_reason: String::new(),
            path: "/v1internal:streamGenerateContent".to_string(),
        };

        let audit_ev = evaluate_frozen_routing_event(&ev);
        let ev_json = serde_json::to_string(&ev).unwrap();
        let audit_ev_json = serde_json::to_string(&audit_ev).unwrap();

        for sensitive in [
            secret_email,
            secret_token,
            "Authorization",
            "bearer",
            "prompt",
            "response",
        ] {
            assert!(
                !ev_json.contains(sensitive),
                "RoutingEvent leaked sensitive data {:?}",
                sensitive
            );
            assert!(
                !audit_ev_json.contains(sensitive),
                "RoutingAuditEvent leaked sensitive data {:?}",
                sensitive
            );
        }
    }

    #[test]
    fn test_temporal_audit_k_legacy_semantics_intact() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let low_5h = 0.15;
        let healthy_weekly = 0.80;

        // Positively identified 5H at 15% -> Reserve
        let acc_5h = Account {
            id: "acc_5h".to_string(),
            name: "Identified5H".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(low_5h),
                    reset_time: Some(serde_json::Value::String("2026-09-17T20:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(healthy_weekly),
                    reset_time: Some(serde_json::Value::String("2026-09-24T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cap_5h = crate::quota::compute_capacity_state(&acc_5h, now);
        assert!(crate::scheduler::is_reserve_capacity(&cap_5h));

        // Positively identified Weekly at 10% -> Reserve
        let low_wk = 0.10;
        let acc_wk = Account {
            id: "acc_wk".to_string(),
            name: "IdentifiedWeekly".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(healthy_weekly),
                    reset_time: Some(serde_json::Value::String("2026-09-17T20:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(low_wk),
                    reset_time: Some(serde_json::Value::String("2026-09-24T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cap_wk = crate::quota::compute_capacity_state(&acc_wk, now);
        assert!(crate::scheduler::is_reserve_capacity(&cap_wk));

        // Legacy single fraction at 12% (no window identity) -> NOT reserve
        let legacy_frac = 0.12;
        let acc_legacy = Account {
            id: "acc_legacy".to_string(),
            name: "LegacyAccount".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(legacy_frac),
                updated_at: Some(now as i64 - 10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let cap_legacy = crate::quota::compute_capacity_state(&acc_legacy, now);
        assert!(!crate::scheduler::is_reserve_capacity(&cap_legacy));
        assert!(cap_legacy.legacy_fraction_known);
    }
}
