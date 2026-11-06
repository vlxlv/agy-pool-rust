#![allow(clippy::collapsible_if)]

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::DEFAULT_STRATEGY;
use crate::daemon::InstanceContext;
use crate::storage::{Pool, StorageError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountEffectiveness {
    pub id: String,
    pub name: String,
    pub status: String,
    pub is_next: bool,
    pub window_requests: i64,
    pub window_percent: f64,
    pub lifetime_hits: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_5h_percent: Option<f64>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub quota_5h_reset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_wk_percent: Option<f64>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub quota_wk_reset: String,
    pub in_flight: i64,
    pub is_paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayEffectiveness {
    pub status: String,
    pub pid: i32,
    pub uptime: String,
    pub uptime_seconds: i64,
    pub generation_total: u64,
    pub generation_ok: u64,
    pub routing_decisions: u64,
    pub failovers_total: u64,
    pub failovers_ok: u64,
    pub restricted_skips: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AIVerdict {
    pub status: String,
    pub summary: String,
    pub issues: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectivenessReport {
    pub timestamp: String,
    pub strategy: String,
    pub gateway: GatewayEffectiveness,
    pub window_seconds: i64,
    #[serde(rename = "window_requests_total")]
    pub window_requests: i64,
    pub accounts: Vec<AccountEffectiveness>,
    pub active_accounts: usize,
    pub balance_index: f64,
    pub balance_status: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub dominant_account: String,
    pub ai_verdict: AIVerdict,
}

pub fn compute_effectiveness_report(
    ctx: Option<&InstanceContext>,
    mut window_duration: Duration,
) -> Result<EffectivenessReport, StorageError> {
    let pool = crate::storage::load_pool().unwrap_or_else(|_| Pool::new_empty());

    let now_time = jiff::Timestamp::now();
    let now_ts = now_time.as_second();
    let strat = if !pool.strategy.is_empty() {
        pool.strategy.clone()
    } else {
        DEFAULT_STRATEGY.to_string()
    };

    if window_duration.is_zero() {
        window_duration = Duration::from_secs(4 * 3600);
    }
    let cutoff_ts = now_ts - window_duration.as_secs() as i64;

    // 1. Gather Gateway runtime metrics if running
    let mut gw_eff = GatewayEffectiveness {
        status: "STOPPED".to_string(),
        pid: 0,
        uptime: String::new(),
        uptime_seconds: 0,
        generation_total: 0,
        generation_ok: 0,
        routing_decisions: 0,
        failovers_total: 0,
        failovers_ok: 0,
        restricted_skips: 0,
    };

    let mut runtime_stats = None;
    if let Some(ctx) = ctx {
        let (status, info, _) = crate::daemon::check_instance_status(ctx);
        if (status == crate::daemon::InstanceStatus::RunningSameInstance
            || status == crate::daemon::InstanceStatus::OutdatedBinary)
            && let Some(info) = info
            && info.pid > 0
        {
            gw_eff.status = "RUNNING".to_string();
            gw_eff.pid = info.pid;
            if let Ok(stats) = ctx.get_runtime_stats(info.pid) {
                let uptime_secs = (now_ts - stats.started_at.as_second()).max(0);
                gw_eff.uptime = crate::daemon::format_uptime(Duration::from_secs(uptime_secs as u64));
                gw_eff.uptime_seconds = uptime_secs;
                gw_eff.generation_total = stats.snapshot.generation_requests;
                gw_eff.generation_ok = stats.snapshot.generation_success;
                gw_eff.routing_decisions = stats.snapshot.routing_decisions;
                gw_eff.failovers_total = stats.snapshot.failover_attempts;
                gw_eff.failovers_ok = stats.snapshot.failover_success;
                gw_eff.restricted_skips = stats.snapshot.restricted_skips;
                runtime_stats = Some(stats);
            }
        }
    }

    // 2. Count window requests per account from pool.generation_events
    let mut event_counts = std::collections::HashMap::new();
    let mut total_window_events: i64 = 0;
    for e in &pool.generation_events {
        if e.ts >= cutoff_ts {
            *event_counts.entry(e.id.clone()).or_insert(0i64) += 1;
            total_window_events += 1;
        }
    }

    // 3. Determine who is Next
    let next_id = crate::daemon::next_candidate_id(Some(&pool), now_ts as f64);

    // 4. Build per-account metrics
    let mut acc_metrics = Vec::with_capacity(pool.accounts.len());
    let mut active_ready_count = 0;
    let mut active_ready_requests = Vec::new();
    let mut active_ready_ids = Vec::new();

    for a in &pool.accounts {
        let mut eff = AccountEffectiveness {
            id: a.id.clone(),
            name: crate::accounts::display_account_name(Some(a), Some(false)),
            status: a.status.clone(),
            is_next: a.id == next_id,
            window_requests: 0,
            window_percent: 0.0,
            lifetime_hits: a.get_hits(),
            quota_5h_percent: None,
            quota_5h_reset: String::new(),
            quota_wk_percent: None,
            quota_wk_reset: String::new(),
            in_flight: 0,
            is_paused: false,
        };

        if let Some(s) = &runtime_stats {
            if let Some(if_map) = &s.in_flight {
                eff.in_flight = if_map.get(&a.id).copied().unwrap_or(0);
            }
            eff.is_paused = s.paused_for_last_model(&a.id, now_time);
        }

        let cap_state = crate::quota::compute_capacity_state(a, now_ts as f64);
        let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(a.last_quota.as_ref());
        if let Some(q5) = q5_frac {
            let pct = (q5 * 1000.0).round() / 10.0;
            eff.quota_5h_percent = Some(pct);
            if let Some(r5) = cap_state.reset5_time {
                let r5_str = jiff::Timestamp::try_from(r5)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                eff.quota_5h_reset = crate::quota::format_compact_remaining_time(r5_str.as_str());
            }
        }
        if let Some(gw) = gw_frac {
            let pct = (gw * 1000.0).round() / 10.0;
            eff.quota_wk_percent = Some(pct);
            if let Some(r7) = cap_state.reset7_time {
                let r7_str = jiff::Timestamp::try_from(r7)
                    .map(|ts| ts.to_string())
                    .unwrap_or_default();
                eff.quota_wk_reset = crate::quota::format_compact_remaining_time(r7_str.as_str());
            }
        }

        let reqs = event_counts.get(&a.id).copied().unwrap_or(0);
        eff.window_requests = reqs;
        if total_window_events > 0 {
            eff.window_percent = (reqs as f64 * 1000.0 / total_window_events as f64).round() / 10.0;
        }

        let is_cooling = a.rate_limited_until.is_some_and(|rl| rl > now_ts as f64);
        if a.status == crate::storage::STATUS_DISABLED {
            eff.status = "Disabled".to_string();
        } else if crate::scheduler::is_restricted(a) {
            eff.status = "Restricted".to_string();
        } else if is_cooling {
            eff.status = "Cooldown".to_string();
        } else if gw_frac.is_some_and(|gw| gw <= crate::config::DEPLETED_THRESHOLD) {
            eff.status = "Exhausted".to_string();
        } else {
            eff.status = "Ready".to_string();
            active_ready_count += 1;
            active_ready_requests.push(reqs);
            active_ready_ids.push(a.id.clone());
        }

        acc_metrics.push(eff);
    }

    // 5. Calculate Balance Index across active Ready accounts
    let mut balance_index = 1.0;
    let mut balance_status = "IDLE".to_string();
    let mut dominant_account = String::new();
    let mut max_active_reqs: i64 = -1;

    if active_ready_count <= 1 {
        balance_index = 1.0;
        if active_ready_count == 1 {
            balance_status = "MONOPOLIZED".to_string();
            dominant_account = active_ready_ids[0].clone();
        } else {
            balance_status = "IDLE".to_string();
        }
    } else if total_window_events == 0 {
        balance_index = 1.0;
        balance_status = "IDLE".to_string();
    } else {
        let mut total_active_reqs: i64 = 0;
        for (i, &reqs) in active_ready_requests.iter().enumerate() {
            total_active_reqs += reqs;
            if reqs > max_active_reqs {
                max_active_reqs = reqs;
                dominant_account = active_ready_ids[i].clone();
            }
        }

        if total_active_reqs > 0 {
            let mean = total_active_reqs as f64 / active_ready_count as f64;
            let mut sum_sq_diff = 0.0;
            for &reqs in &active_ready_requests {
                let diff = reqs as f64 - mean;
                sum_sq_diff += diff * diff;
            }
            let std_dev = (sum_sq_diff / active_ready_count as f64).sqrt();
            let rsd = std_dev / mean;
            let max_rsd = ((active_ready_count - 1) as f64).sqrt();
            if max_rsd > 0.0 {
                let normalized_skew = (rsd / max_rsd).min(1.0);
                balance_index = ((1.0 - normalized_skew) * 100.0).round() / 100.0;
            }

            if balance_index >= 0.80 {
                balance_status = "OPTIMAL".to_string();
            } else if balance_index >= 0.60 {
                balance_status = "BALANCED".to_string();
            } else if balance_index >= 0.35 {
                balance_status = "SKEWED".to_string();
            } else {
                balance_status = "MONOPOLIZED".to_string();
            }
        }
    }

    // 6. Generate AI Verdict & Issues
    let mut issues = Vec::new();
    let mut recommendations = Vec::new();
    let mut verdict_status = "HEALTHY".to_string();

    if gw_eff.status != "RUNNING" {
        issues.push("Gateway proxy is currently STOPPED.".to_string());
        recommendations.push("Start gateway with 'agy-pool start'.".to_string());
        verdict_status = "DEGRADED".to_string();
    }

    for acc in &acc_metrics {
        if acc.status == "Exhausted" {
            issues.push(format!(
                "Account {} is Exhausted (Weekly quota depleted, all models blocked until reset).",
                acc.name
            ));
            verdict_status = "DEGRADED".to_string();
        } else if acc.status == "Restricted" {
            issues.push(format!(
                "Account {} is Restricted (Requires re-verification with 'agy-pool verify').",
                acc.name
            ));
            if verdict_status != "DEGRADED" {
                verdict_status = "WARNING".to_string();
            }
        } else if acc.status == "Cooldown" {
            issues.push(format!(
                "Account {} is currently in Cooldown (429 rate limit backoff).",
                acc.name
            ));
            if verdict_status != "DEGRADED" {
                verdict_status = "WARNING".to_string();
            }
        }

        if acc.status == "Ready" && acc.quota_wk_percent.is_some_and(|p| p < 15.0) {
            issues.push(format!(
                "Account {} has low weekly quota ({} until reset).",
                acc.name, acc.quota_wk_reset
            ));
            if verdict_status != "DEGRADED" {
                verdict_status = "WARNING".to_string();
            }
        }
    }

    if balance_status == "SKEWED" || balance_status == "MONOPOLIZED" {
        if total_window_events >= (active_ready_count * 3) as i64 {
            issues.push(format!(
                "Traffic distribution is skewed (Dominant account: {}).",
                dominant_account
            ));
            if verdict_status == "HEALTHY" {
                verdict_status = "WARNING".to_string();
            }
            recommendations.push(format!(
                "Strategy is currently '{}'. Some skew is expected: a conversation stays on its account (session affinity) and smart harvests expiring 5H quota first; persistent skew across many conversations means new sessions are not being spread.",
                strat
            ));
        }
    }

    if gw_eff.failovers_total > 0 && gw_eff.generation_total > 0 {
        let fail_rate = gw_eff.failovers_total as f64 / gw_eff.generation_total as f64;
        if fail_rate > 0.05 {
            issues.push("High failover rate detected (failovers occur on >5% of generations).".to_string());
            if verdict_status != "CRITICAL" {
                verdict_status = "WARNING".to_string();
            }
        }
    }

    if issues.is_empty() {
        recommendations.push(format!(
            "Strategy '{}' is operating smoothly with balanced account rotation and healthy quota margins.",
            strat
        ));
    } else if recommendations.is_empty() {
        recommendations
            .push("Review account statuses and run 'agy-pool quota' to monitor replenishment timing.".to_string());
    }

    let summary = match verdict_status.as_str() {
        "HEALTHY" => "All active accounts are healthy, traffic is well-distributed, and quotas are sustainable.",
        "WARNING" => "System is operational, but some accounts have low quota or traffic is non-uniform.",
        "DEGRADED" => "One or more accounts are exhausted or restricted, reducing total pool concurrency.",
        _ => "Critical condition detected in account pool or gateway proxy.",
    };

    Ok(EffectivenessReport {
        timestamp: now_time.to_string(),
        strategy: strat,
        gateway: gw_eff,
        window_seconds: window_duration.as_secs() as i64,
        window_requests: total_window_events,
        accounts: acc_metrics,
        active_accounts: active_ready_count,
        balance_index,
        balance_status,
        dominant_account,
        ai_verdict: AIVerdict {
            status: verdict_status,
            summary: summary.to_string(),
            issues,
            recommendations,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Account, GenerationEvent, Pool, QuotaState, QuotaWindow};

    #[test]
    fn test_compute_effectiveness_report_empty_pool() {
        let env = crate::daemon::test_env();
        let report = compute_effectiveness_report(None, Duration::from_secs(4 * 3600)).unwrap();
        assert_eq!(report.active_accounts, 0);
        assert_eq!(report.balance_status, "IDLE");
        drop(env);
    }

    #[test]
    fn test_compute_effectiveness_report_balanced_distribution() {
        let env = crate::daemon::test_env();
        let now = jiff::Timestamp::now().as_second();
        let frac = 0.90;

        let acc1 = Account {
            id: "acc1".to_string(),
            name: "Account 1".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc2".to_string(),
            name: "Account 2".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let pool = Pool {
            version: 1,
            strategy: "smart".to_string(),
            accounts: vec![acc1, acc2],
            generation_events: vec![
                GenerationEvent {
                    id: "acc1".to_string(),
                    ts: now - 100,
                },
                GenerationEvent {
                    id: "acc1".to_string(),
                    ts: now - 80,
                },
                GenerationEvent {
                    id: "acc2".to_string(),
                    ts: now - 60,
                },
                GenerationEvent {
                    id: "acc2".to_string(),
                    ts: now - 40,
                },
            ],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let report = compute_effectiveness_report(None, Duration::from_secs(4 * 3600)).unwrap();
        assert_eq!(report.active_accounts, 2);
        assert_eq!(report.window_requests, 4);
        assert!(
            report.balance_index >= 0.99,
            "expected balance_index >= 0.99, got {}",
            report.balance_index
        );
        assert_eq!(report.balance_status, "OPTIMAL");
        assert!(
            report.ai_verdict.status == "DEGRADED"
                || report.ai_verdict.status == "WARNING"
                || report.ai_verdict.status == "HEALTHY",
            "invalid AI verdict status: {}",
            report.ai_verdict.status
        );
        drop(env);
    }

    #[test]
    fn test_compute_effectiveness_report_skewed_distribution() {
        let env = crate::daemon::test_env();
        let now = jiff::Timestamp::now().as_second();
        let frac = 0.90;

        let acc1 = Account {
            id: "acc1".to_string(),
            name: "Account 1".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc2".to_string(),
            name: "Account 2".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut events = Vec::new();
        for i in 0..20 {
            events.push(GenerationEvent {
                id: "acc1".to_string(),
                ts: now - (i * 10),
            });
        }
        events.push(GenerationEvent {
            id: "acc2".to_string(),
            ts: now - 5,
        });

        let pool = Pool {
            version: 1,
            strategy: "smart".to_string(),
            accounts: vec![acc1, acc2],
            generation_events: events,
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let report = compute_effectiveness_report(None, Duration::from_secs(4 * 3600)).unwrap();
        assert_eq!(report.dominant_account, "acc1");
        assert!(
            report.balance_status == "SKEWED" || report.balance_status == "MONOPOLIZED",
            "expected SKEWED or MONOPOLIZED status, got {}",
            report.balance_status
        );
        assert!(
            !report.ai_verdict.issues.is_empty(),
            "expected detected issues for skewed traffic"
        );
        drop(env);
    }
}
