use serde::{Deserialize, Serialize};
use std::sync::{LazyLock, RwLock};

use crate::dashboard::telemetry::LatencyStatsDto;
use crate::storage::Pool;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub status: String,
    pub listen: String,
    pub strategy: String,
    pub uptime_secs: u64,
    pub memory_rss_bytes: u64,
    pub in_flight_requests: i64,
    pub total_requests: u64,
    pub qps: f64,
    pub latency: LatencyStatsDto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowDto {
    pub fraction: f64,
    pub remaining_percent: f64,
    pub reset_at_unix: i64,
    pub reset_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountDto {
    pub id: String,
    pub name: String,
    pub email_masked: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub storage_status: String,
    pub scheduler_state: String,
    pub five_hour: Option<QuotaWindowDto>,
    pub weekly: Option<QuotaWindowDto>,
    pub hits: i64,
    pub in_flight: i64,
    /// The account the native `agy` CLI currently uses (`pool.active_account_id`).
    pub is_cli_base: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountsResponse {
    pub accounts: Vec<AccountDto>,
    pub total_accounts: usize,
    pub active_accounts: usize,
    pub show_email_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastResponse {
    pub runway_hours: Option<f64>,
    pub runway_text: String,
    pub is_sustainable: bool,
    pub burn_rate_requests_per_hour: f64,
    pub window_used: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SseStatsEvent {
    pub in_flight: i64,
    pub qps: f64,
    pub rss_mb: f64,
}

static RUNTIME_META: LazyLock<RwLock<Option<(String, jiff::Timestamp)>>> = LazyLock::new(|| RwLock::new(None));

pub fn set_runtime_meta(listen: String, started_at: jiff::Timestamp) {
    let mut lock = RUNTIME_META.write().unwrap_or_else(|e| e.into_inner());
    *lock = Some((listen, started_at));
}

pub fn get_runtime_meta() -> (String, jiff::Timestamp) {
    let lock = RUNTIME_META.read().unwrap_or_else(|e| e.into_inner());
    if let Some((listen, started_at)) = lock.as_ref() {
        (listen.clone(), *started_at)
    } else {
        (String::from("127.0.0.1:8899"), jiff::Timestamp::now())
    }
}

pub fn mask_email(email: &str) -> String {
    let email = email.trim();
    if email.is_empty() {
        return String::new();
    }
    if let Some((local, domain)) = email.split_once('@') {
        let chars: Vec<char> = local.chars().collect();
        if chars.is_empty() {
            format!("***@{domain}")
        } else if chars.len() <= 2 {
            format!("{}***@{domain}", chars[0])
        } else {
            format!("{}***{}@{domain}", chars[0], chars[chars.len() - 1])
        }
    } else {
        let mut chars = email.chars();
        if let Some(first) = chars.next() {
            format!("{first}***")
        } else {
            String::new()
        }
    }
}

pub fn build_status_response() -> StatusResponse {
    let (listen, started_at) = get_runtime_meta();
    let now_jiff = jiff::Timestamp::now();
    let uptime_secs = (now_jiff.as_second() - started_at.as_second()).max(0) as u64;

    let strategy = match crate::storage::load_pool() {
        Ok(pool) => {
            if pool.strategy.is_empty() {
                "smart".to_string()
            } else {
                pool.strategy
            }
        }
        Err(_) => "smart".to_string(),
    };

    let memory_rss_bytes = crate::daemon::runtime::process_rss_bytes();

    let in_flight_map = crate::observability::all_in_flight_generations();
    let in_flight_requests: i64 = in_flight_map.values().sum();

    let snap = crate::observability::snapshot();
    let total_requests = snap.generation_requests;

    let (latency, qps) = crate::dashboard::telemetry::latency_and_qps_snapshot(std::time::Instant::now());

    StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        status: "running".to_string(),
        listen,
        strategy,
        uptime_secs,
        memory_rss_bytes,
        in_flight_requests,
        total_requests,
        qps,
        latency,
    }
}

fn build_quota_window_dto(
    fraction_opt: Option<f64>,
    reset_time_opt: Option<std::time::SystemTime>,
    now_ts: f64,
) -> Option<QuotaWindowDto> {
    fraction_opt.map(|fraction| {
        let remaining_percent = (fraction * 1000.0).round() / 10.0;
        let reset_at_unix = reset_time_opt
            .and_then(|st| {
                st.duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs() as i64)
            })
            .unwrap_or(0);
        let reset_in_secs = if (reset_at_unix as f64) > now_ts {
            (reset_at_unix as f64 - now_ts).ceil() as u64
        } else {
            0
        };
        QuotaWindowDto {
            fraction,
            remaining_percent,
            reset_at_unix,
            reset_in_secs,
        }
    })
}

pub fn build_accounts_response(pool: &Pool, request_show_email: bool, now_ts: f64) -> AccountsResponse {
    let show_email_allowed = crate::accounts::is_show_email_enabled(Some(pool));
    let show_email = show_email_allowed && request_show_email;
    let in_flight_map = crate::observability::all_in_flight_generations();

    let mut accounts = Vec::with_capacity(pool.accounts.len());

    for acc in &pool.accounts {
        let cap = crate::quota::compute_capacity_state(acc, now_ts);
        let (q5_frac, qw_frac) = crate::quota::display_quota_fractions(acc.last_quota.as_ref());

        let five_hour = build_quota_window_dto(q5_frac, cap.reset5_time, now_ts);
        let weekly = build_quota_window_dto(qw_frac, cap.reset7_time, now_ts);

        let scheduler_state = if acc.status == crate::storage::STATUS_DISABLED {
            "disabled".to_string()
        } else if acc.rate_limited_until.is_some_and(|r| r > now_ts) {
            "cooling".to_string()
        } else if crate::scheduler::is_restricted(acc) {
            "restricted".to_string()
        } else if cap.is_depleted {
            "depleted".to_string()
        } else if crate::scheduler::is_reserve_capacity(&cap) || crate::scheduler::over_weekly_pace_capacity(&cap) {
            "reserve".to_string()
        } else {
            "active".to_string()
        };

        let storage_status = if acc.status.is_empty() {
            "active".to_string()
        } else {
            acc.status.clone()
        };

        let email_masked = mask_email(&acc.email);
        let email = if show_email { Some(acc.email.clone()) } else { None };

        let name = crate::accounts::display_account_name(Some(acc), Some(false));
        let hits = acc.hits();
        let in_flight = in_flight_map.get(&acc.id).copied().unwrap_or(0);

        accounts.push(AccountDto {
            id: acc.id.clone(),
            name,
            email_masked,
            email,
            storage_status,
            scheduler_state,
            five_hour,
            weekly,
            hits,
            in_flight,
            is_cli_base: pool.active_account_id.as_deref() == Some(acc.id.as_str()),
        });
    }

    let total_accounts = accounts.len();
    let active_accounts = accounts.iter().filter(|a| a.scheduler_state == "active").count();

    AccountsResponse {
        accounts,
        total_accounts,
        active_accounts,
        show_email_allowed,
    }
}

pub fn build_forecast_response(pool: &Pool, now_ts: f64) -> ForecastResponse {
    let forecast = crate::forecast::simulate_quota_runway(pool, None, Some(now_ts));
    let burn_telemetry = forecast
        .as_ref()
        .and_then(|f| f.telemetry.clone())
        .unwrap_or_else(|| crate::forecast::get_burn_rate(pool, Some(now_ts)));

    let mut burn_rate_requests_per_hour = burn_telemetry.gens_per_hour;
    if burn_rate_requests_per_hour.is_nan() {
        burn_rate_requests_per_hour = 0.0;
    }
    let window_used = burn_telemetry.window_used;

    let (is_sustainable, runway_hours, runway_text) = match forecast.as_ref().map(|f| f.summary_verdict()) {
        Some(verdict) => (verdict.is_sustainable, verdict.runway_hours, verdict.runway_text),
        None => (false, None, "Unavailable".to_string()),
    };

    ForecastResponse {
        runway_hours,
        runway_text,
        is_sustainable,
        burn_rate_requests_per_hour,
        window_used,
    }
}

pub fn build_sse_stats_event() -> SseStatsEvent {
    let in_flight_map = crate::observability::all_in_flight_generations();
    let in_flight: i64 = in_flight_map.values().sum();

    let (_, qps) = crate::dashboard::telemetry::latency_and_qps_snapshot(std::time::Instant::now());

    let rss_bytes = crate::daemon::runtime::process_rss_bytes();
    let rss_mb = ((rss_bytes as f64 / (1024.0 * 1024.0)) * 10.0).round() / 10.0;

    SseStatsEvent { in_flight, qps, rss_mb }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenTotalsDto {
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    /// Part of prompt_tokens; not added to total_tokens.
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub thinking_tokens: i64,
    pub request_count: i64,
    pub aborted_count: i64,
}

impl From<&crate::storage::tokens::TokenCounts> for TokenTotalsDto {
    fn from(c: &crate::storage::tokens::TokenCounts) -> Self {
        TokenTotalsDto {
            total_tokens: c.total(),
            prompt_tokens: c.prompt,
            cached_tokens: c.cached,
            output_tokens: c.output,
            thinking_tokens: c.thinking,
            request_count: c.requests,
            aborted_count: c.aborted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAccountDto {
    pub account_id: String,
    pub name: String,
    #[serde(flatten)]
    pub totals: TokenTotalsDto,
    /// This account's share of the pool's total_tokens; null when the pool used nothing.
    pub pool_share: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokensResponse {
    pub range: String,
    /// Unix start of the first hour bucket in the range; null for "total".
    pub window_start: Option<i64>,
    pub summary: TokenTotalsDto,
    pub accounts: Vec<TokenAccountDto>,
}

pub fn build_tokens_response(
    range: crate::storage::tokens::TokenRange,
    now_unix: i64,
) -> Result<TokensResponse, crate::storage::StorageError> {
    let usage = crate::storage::tokens::usage_by_account(range, now_unix)?;
    let pool = crate::storage::load_pool().unwrap_or_default();
    let summary = crate::storage::tokens::sum(&usage);
    let pool_total = summary.total();
    let mut accounts: Vec<TokenAccountDto> = usage
        .iter()
        .map(|(id, c)| TokenAccountDto {
            account_id: id.clone(),
            name: crate::storage::tokens::account_label(&pool, id),
            totals: c.into(),
            pool_share: (pool_total > 0).then(|| c.total() as f64 / pool_total as f64),
        })
        .collect();
    accounts.sort_by(|a, b| {
        b.totals
            .total_tokens
            .cmp(&a.totals.total_tokens)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    Ok(TokensResponse {
        range: range.as_str().to_string(),
        window_start: range.start_bucket(now_unix),
        summary: (&summary).into(),
        accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Account, Pool, QuotaState, QuotaWindow, STATUS_DISABLED};

    #[test]
    fn test_mask_email_edge_cases() {
        assert_eq!(mask_email(""), "");
        assert_eq!(mask_email("user@gmail.com"), "u***r@gmail.com");
        assert_eq!(mask_email("a@x.com"), "a***@x.com");
        assert_eq!(mask_email("ab@x.com"), "a***@x.com");
        assert_eq!(mask_email("invalid-email"), "i***");
        assert_eq!(mask_email("äöü@example.com"), "ä***ü@example.com");
    }

    #[test]
    fn test_accounts_dto_null_quotas_and_show_email_toggle() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1700000000.0;
        let mut pool = Pool::default();

        let acc_uninit = Account {
            id: "acc_1".to_string(),
            name: "Alice".to_string(),
            email: "alice@example.com".to_string(),
            status: "active".to_string(),
            last_quota: None,
            ..Default::default()
        };

        let acc_cooling = Account {
            id: "acc_2".to_string(),
            name: "Bob".to_string(),
            email: "bob@example.com".to_string(),
            status: "active".to_string(),
            rate_limited_until: Some(now + 60.0),
            ..Default::default()
        };

        let acc_reserve = Account {
            id: "acc_3".to_string(),
            name: "Charlie".to_string(),
            email: "charlie@example.com".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.08),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let acc_disabled = Account {
            id: "acc_4".to_string(),
            name: "David".to_string(),
            email: "david@example.com".to_string(),
            status: STATUS_DISABLED.to_string(),
            ..Default::default()
        };

        let acc_expired_reset = Account {
            id: "acc_5".to_string(),
            name: "Eve".to_string(),
            email: "eve@example.com".to_string(),
            status: "active".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.85),
                    reset_time: Some(serde_json::json!(now - 10.0)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        pool.accounts = vec![acc_uninit, acc_cooling, acc_reserve, acc_disabled, acc_expired_reset];

        // Default config (pool.show_email == None): server forbids revealing full emails even if query requests it.
        let res_gated = build_accounts_response(&pool, true, now);
        assert!(!res_gated.show_email_allowed);
        let val_gated = serde_json::to_value(&res_gated).expect("serialize res_gated");
        let accs_gated = val_gated
            .get("accounts")
            .and_then(|v| v.as_array())
            .expect("accounts array");
        assert!(accs_gated[0].get("email").is_none());
        assert_eq!(
            accs_gated[0].get("email_masked").and_then(|v| v.as_str()),
            Some("a***e@example.com")
        );

        // When pool enables show_email = Some(true), client toggle controls visibility.
        pool.show_email = Some(true);
        let res_no_email = build_accounts_response(&pool, false, now);
        assert!(res_no_email.show_email_allowed);
        let val_no_email = serde_json::to_value(&res_no_email).expect("serialize res_no_email");
        let accs_no_email = val_no_email
            .get("accounts")
            .and_then(|v| v.as_array())
            .expect("accounts array");
        assert!(accs_no_email[0].get("email").is_none());

        let res_with_email = build_accounts_response(&pool, true, now);
        assert!(res_with_email.show_email_allowed);
        let val_with_email = serde_json::to_value(&res_with_email).expect("serialize res_with_email");
        let accs_with_email = val_with_email
            .get("accounts")
            .and_then(|v| v.as_array())
            .expect("accounts array");
        assert_eq!(
            accs_with_email[0].get("email").and_then(|v| v.as_str()),
            Some("alice@example.com")
        );

        assert_eq!(accs_no_email[0].get("five_hour"), Some(&serde_json::Value::Null));
        assert_eq!(accs_no_email[0].get("weekly"), Some(&serde_json::Value::Null));

        let acc5_json = &accs_no_email[4];
        let reset_in_secs = acc5_json
            .get("five_hour")
            .and_then(|h| h.get("reset_in_secs"))
            .and_then(|v| v.as_u64())
            .expect("reset_in_secs must exist");
        assert_eq!(reset_in_secs, 0);

        let states: Vec<&str> = res_no_email
            .accounts
            .iter()
            .map(|a| a.scheduler_state.as_str())
            .collect();
        assert!(states.contains(&"active"));
        assert!(states.contains(&"reserve"));
        assert!(states.contains(&"cooling"));
        assert!(states.contains(&"disabled"));
    }

    #[test]
    fn test_forecast_dto_verdicts_and_nan_safety() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let now = 1700000000.0;

        // 1. Empty pool must NOT claim Sustainable
        let empty_pool = Pool::default();
        let res_empty = build_forecast_response(&empty_pool, now);
        assert!(!res_empty.is_sustainable);
        assert_eq!(res_empty.runway_hours, None);
        assert_eq!(res_empty.runway_text, "No Accounts");
        assert_eq!(res_empty.burn_rate_requests_per_hour, 0.0);
        let json_str = serde_json::to_string(&res_empty).expect("valid JSON");
        assert!(!json_str.contains("NaN"));

        // 2. All restricted pool must NOT claim Sustainable
        let restricted_pool = Pool {
            accounts: vec![Account {
                id: "acc_1".to_string(),
                status: STATUS_DISABLED.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let res_restricted = build_forecast_response(&restricted_pool, now);
        assert!(!res_restricted.is_sustainable);
        assert_eq!(res_restricted.runway_hours, None);
        assert_eq!(res_restricted.runway_text, "All Restricted");

        // 3. Idle pool with scarce 5h quota stalls at benchmark pace (< 24h)
        let scarce_pool = Pool {
            accounts: vec![Account {
                id: "acc_1".to_string(),
                status: "active".to_string(),
                last_quota: Some(QuotaState {
                    gemini_5h: Some(QuotaWindow {
                        fraction: Some(0.15),
                        reset_time: Some(serde_json::json!(now + 4.5 * 3600.0)),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(QuotaWindow {
                        fraction: Some(0.95),
                        reset_time: Some(serde_json::json!(now + 6.0 * 86400.0)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let res_scarce = build_forecast_response(&scarce_pool, now);
        assert!(!res_scarce.is_sustainable);
        assert!(res_scarce.runway_hours.is_some());

        // 4. Multi-account healthy pool with ample quota is Sustainable
        let healthy_pool = Pool {
            accounts: (1..=3)
                .map(|i| Account {
                    id: format!("acc_{i}"),
                    status: "active".to_string(),
                    last_quota: Some(QuotaState {
                        gemini_5h: Some(QuotaWindow {
                            fraction: Some(1.0),
                            reset_time: Some(serde_json::json!(now + (i as f64) * 3600.0)),
                            ..Default::default()
                        }),
                        gemini_weekly: Some(QuotaWindow {
                            fraction: Some(1.0),
                            reset_time: Some(serde_json::json!(now + 6.0 * 86400.0)),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let res_healthy = build_forecast_response(&healthy_pool, now);
        assert!(res_healthy.is_sustainable);
        assert_eq!(res_healthy.runway_hours, None);
        assert_eq!(res_healthy.runway_text, "Sustainable");

        // 5. Pool where 5h is fine (>= 24h) but weekly quota exhausts (< 7d)
        let weekly_pinch_pool = Pool {
            accounts: (1..=3)
                .map(|i| Account {
                    id: format!("acc_{i}"),
                    status: "active".to_string(),
                    last_quota: Some(QuotaState {
                        gemini_5h: Some(QuotaWindow {
                            fraction: Some(1.0),
                            reset_time: Some(serde_json::json!(now + (i as f64) * 1800.0)),
                            ..Default::default()
                        }),
                        gemini_weekly: Some(QuotaWindow {
                            fraction: Some(0.05),
                            reset_time: Some(serde_json::json!(now + 6.5 * 86400.0)),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let res_weekly = build_forecast_response(&weekly_pinch_pool, now);
        assert!(!res_weekly.is_sustainable);
        assert!(res_weekly.runway_hours.is_some());
        assert!(
            res_weekly.runway_text.contains("(weekly)"),
            "expected weekly text, got {}",
            res_weekly.runway_text
        );
    }

    #[test]
    fn test_runtime_meta_and_status_response() {
        let _env = crate::cli::test_support::cli_test_env();

        let ts = jiff::Timestamp::from_second(1700000000).unwrap();
        set_runtime_meta("0.0.0.0:9999".to_string(), ts);
        let (listen, started) = get_runtime_meta();
        assert_eq!(listen, "0.0.0.0:9999");
        assert_eq!(started, ts);

        let status = build_status_response();
        assert_eq!(status.listen, "0.0.0.0:9999");
        assert_eq!(status.status, "running");
        assert!(!status.version.is_empty());

        let sse = build_sse_stats_event();
        assert!(sse.rss_mb >= 0.0);
    }

    #[test]
    fn accounts_response_marks_cli_base() {
        let pool = crate::storage::Pool {
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    ..Default::default()
                },
            ],
            active_account_id: Some("acc_2".to_string()),
            ..Default::default()
        };
        let resp = build_accounts_response(&pool, false, 0.0);
        let flags: Vec<(String, bool)> = resp.accounts.iter().map(|a| (a.id.clone(), a.is_cli_base)).collect();
        assert_eq!(flags, vec![("acc_1".to_string(), false), ("acc_2".to_string(), true)]);
    }
}
