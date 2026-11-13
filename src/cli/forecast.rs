use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_DIM, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

fn center_string(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let left = (width - len) / 2;
    let right = width - len - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

pub async fn forecast_cmd(io: &mut Io<'_>, args: &[String]) -> Result<(), String> {
    let mut pace_str: Option<String> = None;
    let mut json_output = false;
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        if arg == "-h" || arg == "--help" {
            let _ = writeln!(
                io.stdout,
                "Usage of forecast:\n  -json\n    \tOutput results in JSON format\n  -p string\n    \tSimulated generation rate in generations/hour\n  -pace string\n    \tSimulated generation rate in generations/hour"
            );
            return Ok(());
        } else if arg == "--json" || arg == "-json" {
            json_output = true;
        } else if arg == "--pace" || arg == "-pace" || arg == "-p" {
            i += 1;
            if i >= args.len() {
                return Err(format!("flag needs an argument: {}", arg));
            }
            pace_str = Some(args[i].clone());
        } else if let Some(val) = arg.strip_prefix("--pace=") {
            pace_str = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("-pace=") {
            pace_str = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("-p=") {
            pace_str = Some(val.to_string());
        } else if arg.starts_with('-') {
            return Err(format!("flag provided but not defined: {}", arg));
        }
        i += 1;
    }

    let custom_pace = if let Some(ref p_str) = pace_str {
        match p_str.parse::<f64>() {
            Ok(p) => Some(p),
            Err(e) => return Err(format!("invalid pace value {:?}: {}", p_str, e)),
        }
    } else {
        None
    };

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(e) => return Err(format!("failed to load accounts pool: {}", e)),
    };

    if pool.accounts.is_empty() {
        if json_output {
            let val = serde_json::json!({
                "status": "no_accounts",
                "error": "No accounts in pool yet"
            });
            let out =
                serde_json::to_string_pretty(&val).map_err(|e| format!("failed to encode JSON forecast: {}", e))?;
            let _ = writeln!(io.stdout, "{}", out);
        } else {
            let _ = writeln!(io.stdout, "\n{}No accounts in pool yet.{}", CLR_YELLOW, CLR_RESET);
            let _ = writeln!(
                io.stdout,
                "Run {}agy-pool login{} or {}agy-pool import-current{} to add accounts.\n",
                CLR_BOLD, CLR_RESET, CLR_BOLD, CLR_RESET
            );
        }
        return Ok(());
    }

    let res = match crate::forecast::simulate_quota_runway(&pool, custom_pace, None) {
        Some(r) => r,
        None => return Err("forecast simulation returned no result".to_string()),
    };

    if json_output {
        let out = serde_json::to_string_pretty(&res).map_err(|e| format!("failed to encode JSON forecast: {}", e))?;
        let _ = writeln!(io.stdout, "{}", out);
        return Ok(());
    }

    if res.status == "all_restricted" {
        let _ = writeln!(
            io.stdout,
            "\n{}[Error] All accounts are restricted (security verification required or auth failure).{}",
            CLR_RED, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "Run {}agy-pool verify{} to unlock restricted accounts in browser.\n",
            CLR_BOLD, CLR_RESET
        );
        return Ok(());
    }

    let sep = "=".repeat(68);
    let sub_sep = "-".repeat(68);

    let _ = writeln!(io.stdout, "\n{}", sep);
    let title = format!("Quota Runway & Endurance Forecast ({})", crate::VERSION);
    let _ = writeln!(io.stdout, "{}{}{}", CLR_BOLD, center_string(&title, 68), CLR_RESET);
    let _ = writeln!(io.stdout, "{}", sep);

    let pace_mode = if res.is_custom_pace {
        "User Specified".to_string()
    } else if res.is_idle {
        "Idle (Reference Pace)".to_string()
    } else {
        let win_used = res.telemetry.as_ref().map(|t| t.window_used.as_str()).unwrap_or("1h");
        format!("Measured ({})", win_used)
    };

    let eff_pace = if res.is_idle {
        crate::forecast::BENCHMARK_PACE_GENS_PER_HOUR
    } else {
        res.target_pace
    };

    let eff_weekly_pace = if res.is_idle {
        crate::forecast::BENCHMARK_PACE_GENS_PER_HOUR * (8.0 / 24.0)
    } else {
        res.target_weekly_pace
    };

    let active_sim = if res.is_idle {
        &res.benchmark_runway
    } else {
        &res.current_runway
    };

    let _ = writeln!(
        io.stdout,
        "  Pool Health:     {}{}/{}{} Accounts Active ({} Cooling, {} Restricted)",
        CLR_BOLD, res.healthy_count, res.total_accounts, CLR_RESET, res.cooling_count, res.restricted_count
    );
    let _ = writeln!(
        io.stdout,
        "  Active Strategy: {}{}{}",
        CLR_BOLD, res.strategy, CLR_RESET
    );
    let _ = writeln!(
        io.stdout,
        "  Simulated Pace:  {}{:.1} generations/hour{} ({})",
        CLR_BOLD, eff_pace, CLR_RESET, pace_mode
    );
    if !res.is_custom_pace && !res.is_idle {
        let _ = writeln!(
            io.stdout,
            "  Daily Work Pace: {}{:.1} generations/hour{} (modeled across active work sessions)",
            CLR_BOLD, eff_weekly_pace, CLR_RESET
        );
    }
    let _ = writeln!(
        io.stdout,
        "  Stock on Hand:   {}{:.1}%{} total 5h quota | {}{:.1}%{} total weekly quota",
        CLR_BOLD, res.total_q5_pct, CLR_RESET, CLR_BOLD, res.total_qw_pct, CLR_RESET
    );

    let cost_5h_pct = crate::forecast::DEFAULT_FRACTION_PER_HIT_5H * 100.0;
    let cost_qw_pct = crate::forecast::DEFAULT_FRACTION_PER_HIT_WEEKLY * 100.0;

    let _ = writeln!(io.stdout, "\n{}[5-Hour Rolling Burst Runway]{}", CLR_BOLD, CLR_RESET);
    let burn = eff_pace * crate::forecast::DEFAULT_FRACTION_PER_HIT_5H * 100.0;
    let replenish = res.replenish_rate_pct_hr;
    let net = replenish - burn;
    let net_sign = if net >= 0.0 { "+" } else { "" };
    let net_color = if net >= 0.0 { CLR_GREEN } else { CLR_YELLOW };
    let net_label = if net >= 0.0 { "Surplus" } else { "Deficit" };

    let _ = writeln!(
        io.stdout,
        "  • Natural Replenish:  +{:.1}%/hr ({} accounts × 20.0%/hr)",
        replenish, res.healthy_count
    );
    let _ = writeln!(
        io.stdout,
        "  • Estimated Burn:     ~{:.1}%/hr (~{:.2}% per generation)",
        burn, cost_5h_pct
    );
    let _ = writeln!(
        io.stdout,
        "  • Net Balance:        {}{}{:.1}%/hr{} ({})",
        net_color, net_sign, net, CLR_RESET, net_label
    );

    if active_sim.is_sustainable_5h {
        let _ = writeln!(
            io.stdout,
            "  • 5-Hour Status:      {}⚡ SUSTAINABLE{} (Pool regenerates faster than consumption)",
            CLR_GREEN, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "  • Continuous Burst:   {}> 48 Hours{} (No 5h quota starvation expected)",
            CLR_GREEN, CLR_RESET
        );
    } else {
        let stall_str = crate::forecast::format_duration_short(active_sim.first_stall_sec);
        let _ = writeln!(
            io.stdout,
            "  • 5-Hour Status:      {}🔻 DRAINING{}",
            CLR_YELLOW, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "  • Time to 1st Stall:  {}~{}{}",
            CLR_YELLOW, stall_str, CLR_RESET
        );
        if let Some(dur) = active_sim.stall_duration_sec
            && dur > 0.0
        {
            let pause_str = crate::forecast::format_duration_short(Some(dur));
            let _ = writeln!(
                io.stdout,
                "  • Dry Period Pause:   ~{} until next account reset",
                pause_str
            );
        }
        if !active_sim.first_empty_acc_id.is_empty() {
            let _ = writeln!(
                io.stdout,
                "  • 1st Account Empty:  Account {}",
                active_sim.first_empty_acc_id
            );
        }
    }

    let _ = writeln!(io.stdout, "\n{}[Weekly Budget Runway]{}", CLR_BOLD, CLR_RESET);
    let weekly_burn = eff_weekly_pace * crate::forecast::DEFAULT_FRACTION_PER_HIT_WEEKLY * 100.0;
    let daily_burn = weekly_burn * 24.0;
    let _ = writeln!(
        io.stdout,
        "  • Modeled Daily Burn: ~{:.1}%/day (~{:.3}% per gen, {:.1} gens/hr avg)",
        daily_burn, cost_qw_pct, eff_weekly_pace
    );

    if active_sim.is_sustainable_weekly {
        let _ = writeln!(
            io.stdout,
            "  • Weekly Status:      {}⚡ SUSTAINABLE{} (> 7 Days capacity on hand)",
            CLR_GREEN, CLR_RESET
        );
    } else if let Some(w_sec) = active_sim.weekly_exhaust_sec {
        let w_days = w_sec / 86400.0;
        let w_color = if w_days <= 2.0 { CLR_RED } else { CLR_YELLOW };
        let _ = writeln!(
            io.stdout,
            "  • Weekly Status:      {}⚠ WEEKLY LIMIT PINCH{}",
            w_color, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "  • Weekly Runway:      {}~{:.1} Days remaining{}",
            w_color, w_days, CLR_RESET
        );
    }

    let _ = writeln!(io.stdout, "\n{}[Staggered Reset Timeline]{}", CLR_BOLD, CLR_RESET);
    for a in &res.accounts_forecast {
        let r5_str = crate::forecast::format_duration_short(Some(a.reset5_in_sec as f64));
        let rw_str = crate::forecast::format_duration_short(Some(a.resetw_in_sec as f64));
        let status_tag = if a.is_restricted {
            format!(" {}[Restricted]{}", CLR_RED, CLR_RESET)
        } else if a.is_cooling {
            format!(" {}[Cooldown]{}", CLR_YELLOW, CLR_RESET)
        } else {
            String::new()
        };
        let _ = writeln!(
            io.stdout,
            "  • {}: 5h in {} (Now: {:.1}%) | Weekly in {} (Now: {:.1}%){}",
            a.display_name, r5_str, a.q5_pct, rw_str, a.qw_pct, status_tag
        );
    }

    let _ = writeln!(io.stdout, "{}", sub_sep);
    let _ = writeln!(
        io.stdout,
        "{}Run 'agy-pool forecast --pace <N>' to simulate custom generation rates.{}",
        CLR_DIM, CLR_RESET
    );
    let _ = writeln!(io.stdout, "{}\n", sep);

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn test_forecast_cmd_empty_pool() {
        let _env = crate::cli::test_support::cli_test_env();

        // Plain text
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("No accounts in pool yet"), "out={}", out_str);

        // JSON output
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[String::from("--json")]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let val: serde_json::Value = serde_json::from_slice(&out).expect("invalid json");
        assert_eq!(val["status"], "no_accounts", "expected status no_accounts");
    }

    #[tokio::test]
    async fn test_forecast_cmd_all_restricted() {
        let _env = crate::cli::test_support::cli_test_env();

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![
            crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Account 1".to_string(),
                status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_2".to_string(),
                name: "Account 2".to_string(),
                status: crate::storage::STATUS_AUTH_ERROR.to_string(),
                ..Default::default()
            },
        ];
        crate::storage::save_pool(&pool).expect("SavePool failed");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("All accounts are restricted"), "out={}", out_str);
        assert!(out_str.contains("agy-pool verify"), "out={}", out_str);
    }

    #[tokio::test]
    async fn test_forecast_cmd_normal_accounts_and_pace() {
        let _env = crate::cli::test_support::cli_test_env();

        let now = crate::clock::now_unix();
        let frac = 0.75;
        let reset5 = now + 7200.0;
        let resetw = now + 86400.0 * 3.0;

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![
            crate::storage::Account {
                id: "acc_1".to_string(),
                name: "Main".to_string(),
                last_quota: Some(crate::storage::QuotaState {
                    gemini_5h: Some(crate::storage::QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::json!(reset5)),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(crate::storage::QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::json!(resetw)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            crate::storage::Account {
                id: "acc_2".to_string(),
                name: "Backup".to_string(),
                last_quota: Some(crate::storage::QuotaState {
                    gemini_5h: Some(crate::storage::QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::json!(reset5)),
                        ..Default::default()
                    }),
                    gemini_weekly: Some(crate::storage::QuotaWindow {
                        fraction: Some(frac),
                        reset_time: Some(serde_json::json!(resetw)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        crate::storage::save_pool(&pool).expect("SavePool failed");

        // 1. Default idle / reference pace output
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let plain = crate::cli::width::strip_ansi(&String::from_utf8_lossy(&out));
        assert!(plain.contains("Quota Runway & Endurance Forecast"), "plain={}", plain);
        assert!(plain.contains("2/2 Accounts Active"), "plain={}", plain);
        assert!(plain.contains("5-Hour Rolling Burst Runway"), "plain={}", plain);
        assert!(plain.contains("Weekly Budget Runway"), "plain={}", plain);
        assert!(plain.contains("Staggered Reset Timeline"), "plain={}", plain);

        // 2. Custom pace argument
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[String::from("--pace"), String::from("25.5")]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let out_pace = crate::cli::width::strip_ansi(&String::from_utf8_lossy(&out));
        assert!(out_pace.contains("25.5 generations/hour"), "out_pace={}", out_pace);
        assert!(out_pace.contains("User Specified"), "out_pace={}", out_pace);

        // 3. JSON output
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = std::io::Cursor::new(Vec::new());
        let mut io = crate::cli::Io {
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            stdout_fd: None,
            stdin_fd: None,
        };
        let res = forecast_cmd(&mut io, &[String::from("--json")]).await;
        assert!(res.is_ok(), "unexpected error: {:?}", res);
        let val: serde_json::Value = serde_json::from_slice(&out).expect("invalid json");
        assert_eq!(val["status"], "ok");
        assert_eq!(val["healthy_count"], 2);
    }

    #[tokio::test]
    async fn test_run_cli_forecast_aliases() {
        let _env = crate::cli::test_support::cli_test_env();

        let mut pool = crate::storage::Pool::new_empty();
        let frac = 0.9;
        pool.accounts = vec![crate::storage::Account {
            id: "acc_1".to_string(),
            name: "TestAcc".to_string(),
            last_quota: Some(crate::storage::QuotaState {
                gemini_5h: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                gemini_weekly: Some(crate::storage::QuotaWindow {
                    fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        crate::storage::save_pool(&pool).expect("SavePool failed");

        for cmd_name in &["forecast", "runway", "predict"] {
            let (code, stdout, stderr) = crate::cli::test_support::run_cli(&[cmd_name, "--json"], "").await;
            assert_eq!(code, 0, "{} failed: err={}", cmd_name, stderr);
            let val: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
                panic!("{} returned invalid json: {}: raw={}", cmd_name, e, stdout);
            });
            assert_eq!(val["status"], "ok", "{} expected status ok", cmd_name);
        }
    }
}
