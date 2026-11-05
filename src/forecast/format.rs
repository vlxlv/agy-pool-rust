use crate::forecast::runway::{format_duration_short, simulate_quota_runway};
use crate::storage::Pool;
use std::io::Write;

const CLR_RESET: &str = "\x1b[0m";
const CLR_BOLD: &str = "\x1b[1m";
const CLR_DIM: &str = "\x1b[2m";
const CLR_GREEN: &str = "\x1b[32m";
const CLR_YELLOW: &str = "\x1b[33m";
const CLR_RED: &str = "\x1b[31m";
const CLR_CYAN: &str = "\x1b[36m";

/// PrintRunwaySummary renders compact runway forecast block inside the main quota dashboard.
pub fn print_runway_summary(w: &mut impl Write, pool: &Pool, now: f64, width_opt: Option<usize>) {
    let res = match simulate_quota_runway(pool, None, Some(now)) {
        Some(r) => r,
        None => return,
    };
    if res.status == "no_accounts" || res.status == "all_restricted" {
        return;
    }

    let width = match width_opt {
        Some(w) if w > 0 => w,
        _ => 68,
    };

    let mut sub_sep_len = 68;
    if width < sub_sep_len {
        sub_sep_len = width;
    }
    let sub_sep = "-".repeat(sub_sep_len);
    let _ = writeln!(w, "{sub_sep}");
    let _ = writeln!(
        w,
        " {CLR_BOLD}{CLR_CYAN}⚡ Quota Runway & Endurance Forecast:{CLR_RESET}"
    );

    let h_count = res.healthy_count;
    let tot = res.total_accounts;
    let cool = res.cooling_count;
    let is_idle = res.is_idle;
    let pace = res.target_pace;
    let sim = &res.current_runway;
    let bench = &res.benchmark_runway;

    let mut health_info = format!("{h_count}/{tot} Active");
    if cool > 0 {
        health_info.push_str(&format!(" ({cool} Cooling)"));
    }

    let pace_info = if is_idle {
        format!("{CLR_DIM}Idle (0 gens/hr){CLR_RESET}")
    } else {
        let win_label = if let Some(ref tel) = res.telemetry {
            tel.window_used.as_str()
        } else {
            "1h"
        };
        let win_label = if win_label == "smoothed" {
            "smoothed active".to_string()
        } else {
            format!("{win_label} window")
        };
        format!("{CLR_BOLD}{pace:.1} gens/hr{CLR_RESET} ({CLR_CYAN}{win_label}{CLR_RESET})")
    };

    let _ = writeln!(w, "  • Health & Pace:   {health_info} | {pace_info}");

    let replenish = res.replenish_rate_pct_hr;
    if !is_idle {
        let burn = res.burn_rate_pct_hr;
        let net = res.net_rate_pct_hr;
        let sign = if net >= 0.0 { "+" } else { "" };
        let net_color = if net >= 0.0 { CLR_GREEN } else { CLR_YELLOW };
        if width >= 76 {
            let _ = writeln!(
                w,
                "  • Burn/Replenish:  ~{burn:.1}%/hr burn | +{replenish:.1}%/hr reload ({net_color}{sign}{net:.1}%/hr net{CLR_RESET})"
            );
        } else {
            let _ = writeln!(
                w,
                "  • Burn/Reload:     ~{burn:.1}%/hr burn | +{replenish:.1}%/hr reload ({net_color}{sign}{net:.1}%{CLR_RESET})"
            );
        }
    } else if width >= 64 {
        let _ = writeln!(
            w,
            "  • Capacity Reload: +{replenish:.1}%/hr natural recovery across pool"
        );
    } else {
        let _ = writeln!(w, "  • Capacity Reload: +{replenish:.1}%/hr natural recovery");
    }

    // 5-hour rolling runway
    if is_idle {
        let b5h = if bench.is_sustainable_5h {
            if width >= 76 {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (> 24h at standard ~20 gens/hr pace)")
            } else {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (> 24h at standard pace)")
            }
        } else if let Some(stall_sec) = bench.first_stall_sec {
            let stall_str = format_duration_short(Some(stall_sec));
            if width >= 76 {
                format!("{CLR_YELLOW}~{stall_str}{CLR_RESET} at standard ~20 gens/hr pace")
            } else {
                format!("{CLR_YELLOW}~{stall_str}{CLR_RESET} at standard pace")
            }
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        let _ = writeln!(w, "  • Active Coding:   {b5h}");
    } else {
        let run5h = if sim.is_sustainable_5h {
            if width >= 76 {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (Pool replenishes faster than burn rate)")
            } else if width >= 70 {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (Pool replenishes faster than burn)")
            } else {
                format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (Replenish > burn)")
            }
        } else if let Some(stall_sec) = sim.first_stall_sec {
            let stall_str = format_duration_short(Some(stall_sec));
            let cause_note = if let Some(pause_sec) = sim.stall_duration_sec
                && pause_sec > 0.0
            {
                let pause_str = format_duration_short(Some(pause_sec));
                format!(" (Dry pause: ~{pause_str} until next reset)")
            } else {
                String::new()
            };
            format!("{CLR_YELLOW}~{stall_str} until first stall{CLR_RESET}{cause_note}")
        } else {
            format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
        };
        let _ = writeln!(w, "  • 5-Hour Runway:   {run5h}");
    }

    // Weekly budget runway
    let active_sim = if is_idle { bench } else { sim };
    let w_run = if active_sim.is_sustainable_weekly {
        format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET} (> 7 Days remaining)")
    } else if let Some(ex_sec) = active_sim.weekly_exhaust_sec {
        let w_days = ex_sec / 86400.0;
        let w_color = if w_days <= 2.0 { CLR_RED } else { CLR_YELLOW };
        format!("{w_color}~{w_days:.1} Days remaining{CLR_RESET}")
    } else {
        format!("{CLR_GREEN}⚡ Sustainable{CLR_RESET}")
    };

    let w_label = if is_idle {
        "Weekly Budget:  "
    } else {
        "Weekly Runway:  "
    };
    let _ = writeln!(w, "  • {w_label} {w_run}");
}
