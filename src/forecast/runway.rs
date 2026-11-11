use crate::forecast::burn_rate::{BurnRateTelemetry, get_burn_rate};
use crate::storage::Pool;
use serde::{Deserialize, Serialize};

pub const DEFAULT_FRACTION_PER_HIT_5H: f64 = 0.0050; // ~0.50% of 5h quota (~200 generations per 5h window)
pub const DEFAULT_FRACTION_PER_HIT_WEEKLY: f64 = 0.00035; // ~0.035% of weekly quota (~2,850 generations per week)
pub const BENCHMARK_PACE_GENS_PER_HOUR: f64 = 20.0; // Standard active pair-programming baseline (1 gen every 3 mins)
pub const TELEMETRY_CAP: usize = 1000; // Maximum rolling generation events stored
pub const WINDOW_5H_SECS: f64 = 18000.0; // 5 * 3600
pub const WINDOW_7D_SECS: f64 = 604800.0; // 7 * 86400

/// SingleRunwayResult models time to first quota stall and weekly exhaustion under simulated load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SingleRunwayResult {
    pub first_stall_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_stall_cause: String,
    pub stall_duration_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_empty_acc_id: String,
    pub first_empty_sec: Option<f64>,
    pub weekly_exhaust_sec: Option<f64>,
    pub is_sustainable_5h: bool,
    pub is_sustainable_weekly: bool,
}

/// AccountForecast holds calculated per-account quota projection details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AccountForecast {
    pub id: String,
    pub display_name: String,
    pub q5_pct: f64,
    pub qw_pct: f64,
    pub reset5_in_sec: i64,
    pub resetw_in_sec: i64,
    pub is_cooling: bool,
    pub is_restricted: bool,
}

/// RunwayForecast represents the overall quota endurance prediction for the pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunwayForecast {
    pub status: String,
    pub healthy_count: usize,
    pub cooling_count: usize,
    pub restricted_count: usize,
    pub total_accounts: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: String,
    pub target_pace: f64,
    pub target_weekly_pace: f64,
    pub is_custom_pace: bool,
    pub is_idle: bool,
    pub telemetry: Option<BurnRateTelemetry>,
    pub replenish_rate_pct_hr: f64,
    pub burn_rate_pct_hr: f64,
    pub net_rate_pct_hr: f64,
    pub total_q5_pct: f64,
    pub total_qw_pct: f64,
    pub current_runway: SingleRunwayResult,
    pub benchmark_runway: SingleRunwayResult,
    pub accounts_forecast: Vec<AccountForecast>,
}

/// Shared horizon verdict derived consistently across CLI (`top`, `forecast`) and Web Dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct RunwayVerdict {
    pub is_sustainable: bool,
    pub runway_hours: Option<f64>,
    pub runway_text: String,
    pub five_hour_stall_sec: Option<f64>,
    pub weekly_exhaust_sec: Option<f64>,
}

impl RunwayForecast {
    /// Returns the active simulation horizon (benchmark pace when idle, current pace otherwise).
    pub fn active_runway(&self) -> &SingleRunwayResult {
        if self.is_idle {
            &self.benchmark_runway
        } else {
            &self.current_runway
        }
    }

    /// Returns the 5-hour stall time in seconds only when non-sustainable (< 24h), otherwise `None`.
    pub fn five_hour_stall_sec(&self) -> Option<f64> {
        let active = self.active_runway();
        if !active.is_sustainable_5h {
            active.first_stall_sec
        } else {
            None
        }
    }

    /// Returns the weekly exhaustion time in seconds only when non-sustainable (< 7d), otherwise `None`.
    pub fn weekly_exhaust_sec(&self) -> Option<f64> {
        let active = self.active_runway();
        if !active.is_sustainable_weekly {
            active.weekly_exhaust_sec
        } else {
            None
        }
    }

    pub fn summary_verdict(&self) -> RunwayVerdict {
        match self.status.as_str() {
            "no_accounts" => RunwayVerdict {
                is_sustainable: false,
                runway_hours: None,
                runway_text: "No Accounts".to_string(),
                five_hour_stall_sec: None,
                weekly_exhaust_sec: None,
            },
            "all_restricted" => RunwayVerdict {
                is_sustainable: false,
                runway_hours: None,
                runway_text: "All Restricted".to_string(),
                five_hour_stall_sec: None,
                weekly_exhaust_sec: None,
            },
            _ => {
                let s5 = self.five_hour_stall_sec();
                let sw = self.weekly_exhaust_sec();
                if let Some(secs) = s5 {
                    let hours = ((secs / 3600.0) * 10.0).round() / 10.0;
                    let text = format!("~{}", format_duration_short(Some(secs)));
                    RunwayVerdict {
                        is_sustainable: false,
                        runway_hours: Some(hours),
                        runway_text: text,
                        five_hour_stall_sec: s5,
                        weekly_exhaust_sec: sw,
                    }
                } else if let Some(secs) = sw {
                    let hours = ((secs / 3600.0) * 10.0).round() / 10.0;
                    let days = secs / 86400.0;
                    let text = format!("~{days:.1}d (weekly)");
                    RunwayVerdict {
                        is_sustainable: false,
                        runway_hours: Some(hours),
                        runway_text: text,
                        five_hour_stall_sec: s5,
                        weekly_exhaust_sec: sw,
                    }
                } else {
                    RunwayVerdict {
                        is_sustainable: true,
                        runway_hours: None,
                        runway_text: "Sustainable".to_string(),
                        five_hour_stall_sec: None,
                        weekly_exhaust_sec: None,
                    }
                }
            }
        }
    }
}

/// SimAccount tracks internal mutable state for chronological minute-by-minute simulation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SimAccount {
    pub id: String,
    pub display_name: String,
    pub is_restricted: bool,
    pub cooldown_until: f64,
    pub q5: f64,
    pub qw: f64,
    pub reset5_ts: f64,
    pub resetw_ts: f64,
    pub sim_hits: f64,
}

/// FormatDurationShort formats seconds into compact human-readable duration (e.g. "4h 50m", "6d 22h", "45m", "30s").
pub fn format_duration_short(sec: Option<f64>) -> String {
    let Some(sec) = sec else {
        return "N/A".to_string();
    };
    let mut s = sec as i64;
    if s < 0 {
        s = 0;
    }
    let days = s / 86400;
    let rem = s % 86400;
    let hours = rem / 3600;
    let rem = rem % 3600;
    let mins = rem / 60;
    let secs = rem % 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// RunSingleSimulation simulates chronological quota depletion and staggered reset replenishment.
/// Models minute-by-minute account selection and capacity recovery.
pub fn run_single_simulation(
    sim_accs: &[SimAccount],
    pace: f64,
    strategy: &str,
    now: f64,
    sim_hours: usize,
    pace_weekly: Option<f64>,
) -> SingleRunwayResult {
    if pace <= 0.0 {
        return SingleRunwayResult {
            is_sustainable_5h: true,
            is_sustainable_weekly: true,
            ..Default::default()
        };
    }

    let pw = pace_weekly.unwrap_or(pace);
    let mut accs = sim_accs.to_vec();

    let mut first_empty_acc_id = String::new();
    let mut first_empty_sec = None;
    let mut first_stall_sec = None;
    let mut first_stall_cause = String::new();
    let mut stall_duration_sec = None;
    let mut weekly_exhaust_sec = None;

    let mut t = now;
    let end_t = now + (sim_hours as f64) * 3600.0;

    let cost_5h = DEFAULT_FRACTION_PER_HIT_5H;
    let cost_qw = DEFAULT_FRACTION_PER_HIT_WEEKLY;

    while t < end_t {
        let elapsed = t - now;
        let dt = if elapsed < 12.0 * 3600.0 {
            60.0
        } else if elapsed < 48.0 * 3600.0 {
            300.0
        } else {
            1800.0
        };

        t += dt;

        // Apply quota resets
        for a in &mut accs {
            if !a.is_restricted {
                if t >= a.reset5_ts {
                    a.q5 = 1.0;
                    while a.reset5_ts <= t {
                        a.reset5_ts += WINDOW_5H_SECS;
                    }
                }
                if t >= a.resetw_ts {
                    a.qw = 1.0;
                    while a.resetw_ts <= t {
                        a.resetw_ts += WINDOW_7D_SECS;
                    }
                }
            }
        }

        let demand_gens_5h = pace * (dt / 3600.0);
        let demand_gens_qw = pw * (dt / 3600.0);
        let demand_q5 = demand_gens_5h * cost_5h;
        let demand_qw = demand_gens_qw * cost_qw;

        let mut eligible = Vec::new();
        for (i, a) in accs.iter().enumerate() {
            if !a.is_restricted && t >= a.cooldown_until && a.q5 > 0.005 && a.qw > 0.005 {
                eligible.push(i);
            }
        }

        if eligible.is_empty() {
            if first_stall_sec.is_none() {
                let stall_sec = t - now;
                first_stall_sec = Some(stall_sec);

                let mut weekly_dead = true;
                let mut cooling_dead = true;
                for a in &accs {
                    if !a.is_restricted {
                        if a.qw > 0.005 {
                            weekly_dead = false;
                        }
                        if t >= a.cooldown_until {
                            cooling_dead = false;
                        }
                    }
                }
                if weekly_dead {
                    first_stall_cause = "weekly".to_string();
                } else if cooling_dead {
                    first_stall_cause = "cooldown".to_string();
                } else {
                    first_stall_cause = "5h".to_string();
                }

                let mut relief_candidates = Vec::new();
                for a in &accs {
                    if !a.is_restricted {
                        relief_candidates.push(a.reset5_ts);
                        relief_candidates.push(a.resetw_ts);
                        if a.cooldown_until > t {
                            relief_candidates.push(a.cooldown_until);
                        }
                    }
                }
                if !relief_candidates.is_empty() {
                    let mut min_relief = relief_candidates[0];
                    for &r in &relief_candidates[1..] {
                        if r < min_relief {
                            min_relief = r;
                        }
                    }
                    let dur = (min_relief - t).max(0.0);
                    stall_duration_sec = Some(dur);
                }
            }

            let mut all_weekly_dead = true;
            for a in &accs {
                if !a.is_restricted && a.qw > 0.005 {
                    all_weekly_dead = false;
                    break;
                }
            }
            if all_weekly_dead {
                if weekly_exhaust_sec.is_none() {
                    let exhaust_sec = t - now;
                    weekly_exhaust_sec = Some(exhaust_sec);
                }
                break;
            }

            if elapsed >= 48.0 * 3600.0 {
                break;
            }
            continue;
        }

        let selected_idx = if strategy == "max_quota" {
            let mut sel = eligible[0];
            for &idx in &eligible[1..] {
                let a = &accs[idx];
                let s = &accs[sel];
                if a.q5 > s.q5 || (a.q5 == s.q5 && (a.qw > s.qw || (a.qw == s.qw && a.sim_hits < s.sim_hits))) {
                    sel = idx;
                }
            }
            sel
        } else {
            // smart (default)
            let mut sel = eligible[0];
            for &idx in &eligible[1..] {
                let a = &accs[idx];
                let s = &accs[sel];

                let mut a_renew = a.resetw_ts;
                if a_renew <= 0.0 || (a.reset5_ts > 0.0 && a.reset5_ts < a_renew) {
                    a_renew = a.reset5_ts;
                }
                let mut sel_renew = s.resetw_ts;
                if sel_renew <= 0.0 || (s.reset5_ts > 0.0 && s.reset5_ts < sel_renew) {
                    sel_renew = s.reset5_ts;
                }

                if (a_renew > 0.0 && (sel_renew <= 0.0 || a_renew < sel_renew))
                    || (a_renew == sel_renew && (a.sim_hits < s.sim_hits || (a.sim_hits == s.sim_hits && a.q5 > s.q5)))
                {
                    sel = idx;
                }
            }
            sel
        };

        let selected = &mut accs[selected_idx];
        selected.q5 = (selected.q5 - demand_q5).max(0.0);
        selected.qw = (selected.qw - demand_qw).max(0.0);
        selected.sim_hits += demand_gens_5h;

        if selected.q5 <= 0.005 && first_empty_sec.is_none() {
            let es = t - now;
            first_empty_sec = Some(es);
            first_empty_acc_id = selected.id.clone();
        }
        if selected.qw <= 0.005 && weekly_exhaust_sec.is_none() {
            let mut all_dead = true;
            for a in &accs {
                if !a.is_restricted && a.qw > 0.005 {
                    all_dead = false;
                    break;
                }
            }
            if all_dead {
                let es = t - now;
                weekly_exhaust_sec = Some(es);
            }
        }
    }

    let is_sustainable_5h = first_stall_sec.is_none() || first_stall_sec.unwrap() >= 24.0 * 3600.0;
    let is_sustainable_weekly = weekly_exhaust_sec.is_none();

    SingleRunwayResult {
        first_stall_sec,
        first_stall_cause,
        stall_duration_sec,
        first_empty_acc_id,
        first_empty_sec,
        weekly_exhaust_sec,
        is_sustainable_5h,
        is_sustainable_weekly,
    }
}

/// SimulateQuotaRunway evaluates pool endurance by combining telemetry, staggered resets, and dual windows.
/// Returns complete forecast metrics for current pace and standard benchmark pace.
pub fn simulate_quota_runway(
    pool: &Pool,
    pace_gens_per_hour: Option<f64>,
    now_opt: Option<f64>,
) -> Option<RunwayForecast> {
    let now = match now_opt {
        Some(n) if n > 0.0 => n,
        _ => crate::clock::now_unix(),
    };

    if pool.accounts.is_empty() {
        return Some(RunwayForecast {
            status: "no_accounts".to_string(),
            healthy_count: 0,
            cooling_count: 0,
            restricted_count: 0,
            total_accounts: 0,
            ..Default::default()
        });
    }

    let sim_hours = 168;
    let show_email = crate::accounts::is_show_email_enabled(Some(pool));
    let mut sim_accs = Vec::with_capacity(pool.accounts.len());

    for acc in &pool.accounts {
        let status = acc.status.as_str();
        let is_restricted = status == crate::storage::Status::VALIDATION_REQUIRED
            || status == crate::storage::Status::AUTH_ERROR
            || status == crate::storage::Status::DISABLED;
        let cooldown_until = acc.rate_limited_until.unwrap_or(0.0);

        let (g5_frac, gw_frac) = crate::quota::display_quota_fractions(acc.last_quota.as_ref());
        let mut q5 = g5_frac.unwrap_or(1.0);
        let mut qw = gw_frac.unwrap_or(1.0);

        let mut reset5 = None;
        let mut resetw = None;
        if let Some(ref lq) = acc.last_quota {
            if let Some(ref w5) = lq.gemini_5h
                && w5.reset_time.is_some()
            {
                reset5 = w5.reset_time.as_ref();
            }
            if let Some(ref ww) = lq.gemini_weekly
                && ww.reset_time.is_some()
            {
                resetw = ww.reset_time.as_ref();
            }
            if reset5.is_none() && resetw.is_none() {
                reset5 = lq.reset_time.as_ref();
                resetw = lq.reset_time.as_ref();
            }
        }

        let reset5_ts = reset5.and_then(crate::quota::parse_iso_or_timestamp);
        let resetw_ts = resetw.and_then(crate::quota::parse_iso_or_timestamp);
        let last_updated = acc.last_quota.as_ref().and_then(|lq| lq.updated_at.map(|u| u as f64));

        let r5 = if let Some(r5_ts) = reset5_ts {
            if r5_ts <= now {
                if last_updated.is_none() || last_updated.unwrap() < r5_ts {
                    q5 = 1.0;
                }
                let mut t5 = r5_ts;
                while t5 <= now {
                    t5 += WINDOW_5H_SECS;
                }
                t5
            } else {
                r5_ts
            }
        } else {
            now + WINDOW_5H_SECS
        };

        let rw = if let Some(rw_ts) = resetw_ts {
            if rw_ts <= now {
                if last_updated.is_none() || last_updated.unwrap() < rw_ts {
                    qw = 1.0;
                }
                let mut tw = rw_ts;
                while tw <= now {
                    tw += WINDOW_7D_SECS;
                }
                tw
            } else {
                rw_ts
            }
        } else {
            now + WINDOW_7D_SECS
        };

        let sim_hits = acc.gen_count.map(|g| g as f64).unwrap_or(acc.request_count as f64);

        sim_accs.push(SimAccount {
            id: acc.id.clone(),
            display_name: crate::accounts::display_account_name(Some(acc), Some(show_email)),
            is_restricted,
            cooldown_until,
            q5,
            qw,
            reset5_ts: r5,
            resetw_ts: rw,
            sim_hits,
        });
    }

    let mut healthy_count = 0;
    let mut cooling_count = 0;
    let mut restricted_count = 0;
    for a in &sim_accs {
        if !a.is_restricted {
            healthy_count += 1;
            if a.cooldown_until > now {
                cooling_count += 1;
            }
        } else {
            restricted_count += 1;
        }
    }

    if healthy_count == 0 {
        return Some(RunwayForecast {
            status: "all_restricted".to_string(),
            healthy_count: 0,
            cooling_count: 0,
            restricted_count,
            total_accounts: pool.accounts.len(),
            message: "All accounts are restricted (verification required or auth error)".to_string(),
            ..Default::default()
        });
    }

    let telemetry = get_burn_rate(pool, Some(now));
    let mut strategy = pool.strategy.as_str();
    if strategy.is_empty() {
        strategy = "max_quota";
    }

    let target_pace: f64;
    let target_weekly_pace: f64;
    let is_custom_pace: bool;
    let is_idle: bool;

    if let Some(p) = pace_gens_per_hour {
        target_pace = p.max(0.0);
        target_weekly_pace = target_pace;
        is_custom_pace = true;
        is_idle = false;
    } else {
        target_pace = telemetry.gens_per_hour;
        target_weekly_pace = telemetry.daily_pace;
        is_custom_pace = false;
        is_idle = target_pace == 0.0;
    }

    let current_sim = run_single_simulation(
        &sim_accs,
        target_pace,
        strategy,
        now,
        sim_hours,
        Some(target_weekly_pace),
    );
    let bench_weekly_pace = BENCHMARK_PACE_GENS_PER_HOUR * (8.0 / 24.0);
    let benchmark_sim = run_single_simulation(
        &sim_accs,
        BENCHMARK_PACE_GENS_PER_HOUR,
        strategy,
        now,
        sim_hours,
        Some(bench_weekly_pace),
    );

    let replenish_rate = (healthy_count as f64) * 20.0;
    let burn_rate_pct = target_pace * DEFAULT_FRACTION_PER_HIT_5H * 100.0;
    let net_rate_pct = replenish_rate - burn_rate_pct;

    let mut total_q5 = 0.0;
    let mut total_qw = 0.0;
    for a in &sim_accs {
        if !a.is_restricted {
            total_q5 += a.q5 * 100.0;
            total_qw += a.qw * 100.0;
        }
    }

    let mut acc_forecasts = Vec::with_capacity(sim_accs.len());
    for a in &sim_accs {
        acc_forecasts.push(AccountForecast {
            id: a.id.clone(),
            display_name: a.display_name.clone(),
            q5_pct: (a.q5 * 1000.0).round() / 10.0,
            qw_pct: (a.qw * 1000.0).round() / 10.0,
            reset5_in_sec: (a.reset5_ts - now).max(0.0) as i64,
            resetw_in_sec: (a.resetw_ts - now).max(0.0) as i64,
            is_cooling: a.cooldown_until > now,
            is_restricted: a.is_restricted,
        });
    }

    Some(RunwayForecast {
        status: "ok".to_string(),
        healthy_count,
        cooling_count,
        restricted_count,
        total_accounts: pool.accounts.len(),
        message: String::new(),
        strategy: strategy.to_string(),
        target_pace,
        target_weekly_pace,
        is_custom_pace,
        is_idle,
        telemetry: Some(telemetry),
        replenish_rate_pct_hr: replenish_rate,
        burn_rate_pct_hr: burn_rate_pct,
        net_rate_pct_hr: net_rate_pct,
        total_q5_pct: total_q5,
        total_qw_pct: total_qw,
        current_runway: current_sim,
        benchmark_runway: benchmark_sim,
        accounts_forecast: acc_forecasts,
    })
}
