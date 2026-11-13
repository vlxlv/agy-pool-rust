use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_CYAN, CLR_DIM, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

fn render_terminal_report(w: &mut dyn std::io::Write, r: &crate::diagnostics::EffectivenessReport) {
    let _ = writeln!(
        w,
        "\n===================================================================="
    );
    let _ = writeln!(w, "           Algorithm Routing & Effectiveness Report");
    let _ = writeln!(
        w,
        "===================================================================="
    );

    let gw_str = if r.gateway.status == "RUNNING" {
        format!(
            "{}RUNNING{} (PID {}, Uptime: {})",
            CLR_GREEN, CLR_RESET, r.gateway.pid, r.gateway.uptime
        )
    } else {
        format!("{}STOPPED{}", CLR_YELLOW, CLR_RESET)
    };

    let _ = writeln!(
        w,
        " Strategy: {}{}{} | Gateway: {}",
        CLR_BOLD, r.strategy, CLR_RESET, gw_str
    );
    let _ = writeln!(
        w,
        " Analysis Window: past {:.1}h (Total: {} generation requests)\n",
        r.window_seconds as f64 / 3600.0,
        r.window_requests
    );

    let _ = writeln!(w, " {}Account Load Distribution & Quotas:{}", CLR_BOLD, CLR_RESET);
    for acc in &r.accounts {
        let next_marker = if acc.is_next {
            format!(" {}[Next ⚡]{}", CLR_GREEN, CLR_RESET)
        } else {
            String::new()
        };

        let pct_frac = acc.window_percent / 100.0;
        let bar = crate::quota::render_block_progress_bar(Some(pct_frac), 15);

        let q5_str = match acc.quota_5h_percent {
            Some(v) => format!("{:.0}%", v),
            None => "--".to_string(),
        };
        let qw_str = match acc.quota_wk_percent {
            Some(v) => format!("{:.0}%", v),
            None => "--".to_string(),
        };

        let status_color = match acc.status.as_str() {
            "Ready" => CLR_GREEN,
            "Exhausted" | "Restricted" => CLR_RED,
            "Cooldown" => CLR_YELLOW,
            "Disabled" => CLR_DIM,
            _ => CLR_RESET,
        };

        let _ = writeln!(
            w,
            "  • {:<8} {} {:>5.1}% ({:>3} reqs)  5H: {:>4}  WK: {:>4}  {}{}{}{}",
            acc.name,
            bar,
            acc.window_percent,
            acc.window_requests,
            q5_str,
            qw_str,
            status_color,
            acc.status,
            CLR_RESET,
            next_marker
        );
    }

    let _ = writeln!(w, "\n {}Rotation & Balance Analysis:{}", CLR_BOLD, CLR_RESET);
    let balance_color = match r.balance_status.as_str() {
        "SKEWED" => CLR_YELLOW,
        "MONOPOLIZED" => CLR_RED,
        _ => CLR_GREEN,
    };

    let _ = writeln!(
        w,
        "  • Workload Balance:   {}{} (Index: {:.2} / 1.0){}",
        balance_color, r.balance_status, r.balance_index, CLR_RESET
    );
    if !r.dominant_account.is_empty()
        && r.active_accounts > 1
        && r.balance_status != "BALANCED"
        && r.balance_status != "OPTIMAL"
    {
        let mut dom_name = r.dominant_account.clone();
        for a in &r.accounts {
            if a.id == r.dominant_account && !a.name.is_empty() && a.name != a.id {
                dom_name = format!("{} ({})", a.name, a.id);
                break;
            }
        }
        let _ = writeln!(w, "  • Dominant Account:   {}{}{}", CLR_YELLOW, dom_name, CLR_RESET);
    }
    if !r.gateway.uptime.is_empty() {
        let _ = writeln!(
            w,
            "  • Gateway Resilience: {} routed | {} failovers (since start, {})",
            r.gateway.routing_decisions, r.gateway.failovers_total, r.gateway.uptime
        );
    } else {
        let _ = writeln!(
            w,
            "  • Gateway Resilience: {} routed | {} failovers",
            r.gateway.routing_decisions, r.gateway.failovers_total
        );
    }

    let verdict_color = match r.ai_verdict.status.as_str() {
        "OPTIMAL" | "HEALTHY" => CLR_GREEN,
        "WARNING" => CLR_YELLOW,
        _ => CLR_RED,
    };

    let _ = writeln!(w, "\n {}AI Diagnostic Verdict:{}", CLR_BOLD, CLR_RESET);
    let _ = writeln!(
        w,
        "  {}[{}]{} {}",
        verdict_color, r.ai_verdict.status, CLR_RESET, r.ai_verdict.summary
    );

    if !r.ai_verdict.issues.is_empty() {
        let _ = writeln!(w, "  {}Detected Issues:{}", CLR_YELLOW, CLR_RESET);
        for issue in &r.ai_verdict.issues {
            let _ = writeln!(w, "    ⚠ {}", issue);
        }
    }

    if !r.ai_verdict.recommendations.is_empty() {
        let _ = writeln!(w, "  {}Recommendations:{}", CLR_CYAN, CLR_RESET);
        for rec in &r.ai_verdict.recommendations {
            let _ = writeln!(w, "    → {}", rec);
        }
    }

    let _ = writeln!(
        w,
        "====================================================================\n"
    );
}

pub async fn stats_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (ctx, rem) = match crate::cli::resolve_context_for_cmd(args) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let mut json_output = false;
    let mut window_hours = 4.0f64;
    let mut i = 0;

    while i < rem.len() {
        let arg = &rem[i];
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(
                io.stderr,
                "Usage of stats:\n  -H float\n    \tRolling analysis window in hours (default 4)\n  -hours float\n    \tRolling analysis window in hours (default 4)\n  -json\n    \tOutput full diagnostic metrics in structured JSON format for AI analysis"
            );
            return 2;
        } else if arg == "--json" || arg == "-json" {
            json_output = true;
        } else if arg == "--hours" || arg == "-hours" || arg == "-H" || arg == "--H" {
            i += 1;
            if i >= rem.len() {
                let _ = writeln!(io.stderr, "flag needs an argument: {}", arg);
                return 2;
            }
            match rem[i].parse::<f64>() {
                Ok(v) => window_hours = v,
                Err(_) => {
                    let _ = writeln!(io.stderr, "invalid value {:?} for flag -hours", rem[i]);
                    return 2;
                }
            }
        } else if let Some(val) = arg
            .strip_prefix("--hours=")
            .or_else(|| arg.strip_prefix("-hours="))
            .or_else(|| arg.strip_prefix("-H="))
            .or_else(|| arg.strip_prefix("--H="))
        {
            match val.parse::<f64>() {
                Ok(v) => window_hours = v,
                Err(_) => {
                    let _ = writeln!(io.stderr, "invalid value {:?} for flag -hours", val);
                    return 2;
                }
            }
        } else if arg.starts_with('-') {
            let _ = writeln!(io.stderr, "flag provided but not defined: {}", arg);
            return 2;
        }
        i += 1;
    }

    let window_duration = std::time::Duration::from_secs_f64(window_hours * 3600.0);
    match crate::diagnostics::compute_effectiveness_report(Some(&ctx), window_duration) {
        Ok(report) => {
            if json_output {
                match serde_json::to_string_pretty(&report) {
                    Ok(out) => {
                        let _ = writeln!(io.stdout, "{}", out);
                        0
                    }
                    Err(e) => {
                        let _ = writeln!(io.stderr, "{}[Error] JSON encoding error: {}{}", CLR_RED, e, CLR_RESET);
                        1
                    }
                }
            } else {
                render_terminal_report(io.stdout, &report);
                if let Some(line) = crate::cli::tokens::summary_line(
                    crate::storage::tokens::TokenRange::Day,
                    crate::clock::now_unix() as i64,
                ) {
                    let _ = writeln!(io.stdout, "\n{line}");
                }
                0
            }
        }
        Err(e) => {
            if json_output {
                let val = serde_json::json!({
                    "status": "error",
                    "error": e.to_string(),
                });
                if let Ok(out) = serde_json::to_string_pretty(&val) {
                    let _ = writeln!(io.stdout, "{}", out);
                }
            } else {
                let _ = writeln!(
                    io.stderr,
                    "{}[Error] Failed to compute effectiveness report: {}{}",
                    CLR_RED, e, CLR_RESET
                );
            }
            1
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {

    #[tokio::test]
    async fn test_stats_cmd_terminal_output() {
        let _env = crate::cli::test_support::cli_test_env();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["stats"], "").await;
        assert_eq!(code, 0, "expected code 0, err: {}", stderr);
        assert!(
            stdout.contains("Algorithm Routing & Effectiveness Report"),
            "out:\n{}",
            stdout
        );
        assert!(stdout.contains("AI Diagnostic Verdict"), "out:\n{}", stdout);
    }

    #[tokio::test]
    async fn test_stats_cmd_json_output() {
        let _env = crate::cli::test_support::cli_test_env();

        let (code, stdout, stderr) = crate::cli::test_support::run_cli(&["stats", "--json"], "").await;
        assert_eq!(code, 0, "expected code 0, err: {}", stderr);

        let report: crate::diagnostics::EffectivenessReport = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("failed to parse JSON report: {}: raw:\n{}", e, stdout));

        assert!(
            !report.ai_verdict.status.is_empty(),
            "expected non-empty AI verdict status"
        );
    }
}
