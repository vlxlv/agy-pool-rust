use crate::cli::Io;
use crate::cli::{CLR_BOLD, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

pub async fn manage_strategy(io: &mut Io<'_>, target: &str) -> bool {
    let valid_strategies = [crate::config::STRATEGY_SMART, crate::config::STRATEGY_MAX_QUOTA];

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(_) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Could not load the account pool.{}",
                CLR_RED, CLR_RESET
            );
            return false;
        }
    };

    let mut current = crate::config::DEFAULT_STRATEGY;
    if !pool.strategy.is_empty() {
        current = &pool.strategy;
    }

    if target.is_empty() {
        let _ = writeln!(
            io.stdout,
            "\nCurrent load balancing strategy: {}{}{}{}\n",
            CLR_BOLD, CLR_GREEN, current, CLR_RESET
        );
        let _ = writeln!(io.stdout, "Available strategies:");
        let _ = writeln!(
            io.stdout,
            "  • {}smart{}       - Multi-account concurrent rotation with imminent reset harvesting and low-quota protection (default)",
            CLR_BOLD, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "  • {}max_quota{}   - Balances across accounts by highest remaining 5-hour and weekly quota headroom\n",
            CLR_BOLD, CLR_RESET
        );
        let _ = writeln!(
            io.stdout,
            "To update strategy: {}agy-pool strategy <name>{}\n",
            CLR_BOLD, CLR_RESET
        );
        return true;
    }

    let target_strategy = target.trim().to_ascii_lowercase();
    let is_valid = valid_strategies.iter().any(|&s| s == target_strategy);

    if !is_valid {
        let _ = writeln!(
            io.stdout,
            "{}[Error] Invalid strategy '{}'. Choose from: {}{}",
            CLR_RED,
            target_strategy,
            valid_strategies.join(", "),
            CLR_RESET
        );
        return false;
    }

    let strat_to_save = target_strategy.clone();
    let res = crate::storage::pool_transaction(move |p| {
        p.strategy = strat_to_save.clone();
        Ok(())
    });
    if let Err(err) = res {
        let _ = writeln!(
            io.stderr,
            "{}[Error] Failed to update strategy: {}{}",
            CLR_RED, err, CLR_RESET
        );
        return false;
    }

    let _ = writeln!(
        io.stdout,
        "{}✓ Load balancing strategy set to: {}{}{}",
        CLR_GREEN, CLR_BOLD, target_strategy, CLR_RESET
    );
    true
}

pub async fn status_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (ctx, rem) = match crate::cli::resolve_context_for_cmd(args) {
        Ok(res) => res,
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to resolve instance: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 1;
        }
    };

    let verbose = rem.iter().any(|a| a == "-v" || a == "--verbose");

    let _ = writeln!(io.stdout, "agy-pool {}\n", crate::VERSION);

    let (status, info, msg) = crate::daemon::check_instance_status(&ctx);
    match status {
        crate::daemon::InstanceStatus::RunningSameInstance => {
            let pid = info.as_ref().map(|i| i.pid).unwrap_or(0);
            let listen_disp = ctx.display_listen(info.as_ref());
            let _ = writeln!(
                io.stdout,
                "Gateway : {}RUNNING{}  PID {}  {}",
                CLR_GREEN, CLR_RESET, pid, listen_disp
            );
        }
        crate::daemon::InstanceStatus::OutdatedBinary => {
            let pid = info.as_ref().map(|i| i.pid).unwrap_or(0);
            let listen_disp = ctx.display_listen(info.as_ref());
            let _ = writeln!(
                io.stdout,
                "Gateway : {}RUNNING (Outdated Code){}  PID {}  {}",
                CLR_YELLOW, CLR_RESET, pid, listen_disp
            );
        }
        crate::daemon::InstanceStatus::ConfigMismatch => {
            let _ = writeln!(
                io.stdout,
                "Gateway : {}CONFIG MISMATCH{} ({})",
                CLR_YELLOW, CLR_RESET, msg
            );
        }
        crate::daemon::InstanceStatus::PortMismatch => {
            let _ = writeln!(
                io.stdout,
                "Gateway : {}PORT MISMATCH{} ({})",
                CLR_YELLOW, CLR_RESET, msg
            );
        }
        crate::daemon::InstanceStatus::PortOccupiedForeign => {
            let _ = writeln!(
                io.stdout,
                "Gateway : {}PORT OCCUPIED{} ({})",
                CLR_YELLOW, CLR_RESET, msg
            );
        }
        crate::daemon::InstanceStatus::ForeignInstance => {
            let _ = writeln!(
                io.stdout,
                "Gateway : {}FOREIGN INSTANCE{} ({})",
                CLR_YELLOW, CLR_RESET, msg
            );
        }
        crate::daemon::InstanceStatus::StalePID => {
            let _ = std::fs::remove_file(ctx.pid_file());
            let _ = std::fs::remove_file(ctx.runtime_file());
            let _ = writeln!(
                io.stdout,
                "Gateway : {}STOPPED{} (stale PID file cleaned)",
                CLR_YELLOW, CLR_RESET
            );
        }
        crate::daemon::InstanceStatus::LegacyPID => {
            let pid = info.as_ref().map(|i| i.pid).unwrap_or(0);
            let _ = writeln!(
                io.stdout,
                "Gateway : {}LEGACY PID{} (PID: {})",
                CLR_YELLOW, CLR_RESET, pid
            );
        }
        crate::daemon::InstanceStatus::Stopped => {
            let _ = writeln!(io.stdout, "Gateway : {}STOPPED{}", CLR_YELLOW, CLR_RESET);
        }
    }

    if status == crate::daemon::InstanceStatus::RunningSameInstance
        || status == crate::daemon::InstanceStatus::OutdatedBinary
    {
        if let Some(ref inf) = info
            && inf.pid > 0
        {
            match ctx.get_runtime_stats(inf.pid) {
                Ok(stats) => {
                    let elapsed_secs =
                        (jiff::Timestamp::now().as_second() - stats.started_at.as_second()).max(0) as u64;
                    let d = std::time::Duration::from_secs(elapsed_secs);
                    let uptime = crate::daemon::format_uptime(d);
                    let go_ver = crate::daemon::format_go_version(&stats.go_version);
                    let alloc_str = crate::daemon::format_memory_bytes(stats.alloc_bytes);
                    let heap_str = crate::daemon::format_memory_bytes(stats.heap_bytes);
                    let sys_str = crate::daemon::format_memory_bytes(stats.sys_bytes);

                    let _ = writeln!(io.stdout, "Uptime  : {}", uptime);
                    let _ = writeln!(io.stdout, "Runtime : {} | Goroutines: {}", go_ver, stats.goroutines);
                    let _ = writeln!(
                        io.stdout,
                        "Memory  : Alloc {} | Heap {} | Sys {}",
                        alloc_str, heap_str, sys_str
                    );
                    let _ = writeln!(io.stdout, "GC      : {} cycles", stats.num_gc);

                    if verbose {
                        let _ = writeln!(io.stdout, "\n  {}Runtime Counters{}", CLR_BOLD, CLR_RESET);
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6} requests / {} success",
                            "Generation",
                            crate::observability::format_number(stats.snapshot.generation_requests),
                            crate::observability::format_number(stats.snapshot.generation_success)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6} decisions",
                            "Routing",
                            crate::observability::format_number(stats.snapshot.routing_decisions)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6} attempts / {} recovered",
                            "Failover",
                            crate::observability::format_number(stats.snapshot.failover_attempts),
                            crate::observability::format_number(stats.snapshot.failover_success)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6}",
                            "Restricted skips",
                            crate::observability::format_number(stats.snapshot.restricted_skips)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6} / {} ok / {} failed",
                            "Quota refresh",
                            crate::observability::format_number(stats.snapshot.quota_refresh_attempts),
                            crate::observability::format_number(stats.snapshot.quota_refresh_success),
                            crate::observability::format_number(stats.snapshot.quota_refresh_failure)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6} / {} ok / {} failed",
                            "Auth refresh",
                            crate::observability::format_number(stats.snapshot.auth_refresh_attempts),
                            crate::observability::format_number(stats.snapshot.auth_refresh_success),
                            crate::observability::format_number(stats.snapshot.auth_refresh_failure)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6}",
                            "Persistence errors",
                            crate::observability::format_number(stats.snapshot.persistence_errors)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6}",
                            "Replay prevented",
                            crate::observability::format_number(stats.snapshot.no_replay_prevented)
                        );
                        let _ = writeln!(
                            io.stdout,
                            "    {:<18} {:>6}",
                            "Lease fallbacks",
                            crate::observability::format_number(stats.snapshot.lease_fallbacks)
                        );
                    }
                }
                Err(_) => {
                    let _ = writeln!(io.stdout, "Runtime : (metrics unavailable)");
                    if verbose {
                        let _ = writeln!(io.stdout, "\n  {}Runtime Counters{}", CLR_BOLD, CLR_RESET);
                        let _ = writeln!(io.stdout, "    (metrics unavailable)");
                    }
                }
            }
        }
    } else if verbose {
        let _ = writeln!(io.stdout, "\n  {}Runtime Counters{}", CLR_BOLD, CLR_RESET);
        let _ = writeln!(io.stdout, "    (gateway not running)");
    }

    let _ = writeln!(io.stdout);
    if let Some(cp) = &ctx.config_path {
        let _ = writeln!(io.stdout, "Config  : {}", cp.display());
    }
    let db_path = ctx.data_dir.join("state.db");
    let _ = writeln!(
        io.stdout,
        "State   : {}  (schema v{})",
        db_path.display(),
        crate::storage::STATE_DB_SCHEMA_VERSION
    );

    let pool = match crate::storage::load_pool() {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(io.stdout, "Pool    : (unavailable: {})", e);
            return 0;
        }
    };

    let total_accounts = pool.accounts.len();
    let strat = if pool.strategy.is_empty() {
        crate::config::STRATEGY_MAX_QUOTA
    } else {
        &pool.strategy
    };

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut ready_cnt = 0;
    let mut cooldown_cnt = 0;
    let mut restricted_cnt = 0;
    let mut active_name = "none".to_string();

    for a in &pool.accounts {
        if let Some(ref aid) = pool.active_account_id
            && a.id == *aid
        {
            active_name = crate::accounts::display_account_name(Some(a), None);
        }

        let (q5_frac, gw_frac) = crate::quota::display_quota_fractions(a.last_quota.as_ref());
        let mut rem_fraction = 1.0;
        if let Some(q5) = q5_frac {
            rem_fraction = q5;
        }
        if let Some(gw) = gw_frac
            && gw < rem_fraction
        {
            rem_fraction = gw;
        }

        if a.status == crate::storage::STATUS_VALIDATION_REQUIRED
            || a.status == crate::storage::STATUS_VERIFICATION_PENDING
            || a.status == crate::storage::STATUS_AUTH_ERROR
            || a.status == crate::storage::STATUS_DISABLED
        {
            restricted_cnt += 1;
        } else if let Some(rl) = a.rate_limited_until
            && rl > now_ts as f64
        {
            cooldown_cnt += 1;
        } else if rem_fraction <= crate::config::DEPLETED_THRESHOLD {
            // exhausted count
        } else {
            ready_cnt += 1;
        }
    }

    if total_accounts == 0 {
        let _ = writeln!(io.stdout, "Pool    : 0 accounts (empty)");
    } else {
        let _ = writeln!(
            io.stdout,
            "Pool    : {} accounts | CLI Base: {} | {} Ready | {} Cooldown | {} Restricted",
            total_accounts, active_name, ready_cnt, cooldown_cnt, restricted_cnt
        );
    }
    let _ = writeln!(io.stdout, "Strategy: {}", strat);

    let _ = writeln!(io.stdout);
    if status == crate::daemon::InstanceStatus::RunningSameInstance {
        if restricted_cnt > 0 || cooldown_cnt > 0 {
            let _ = writeln!(
                io.stdout,
                "{}⚠ Degraded ({} restricted, {} cooling){}",
                CLR_YELLOW, restricted_cnt, cooldown_cnt, CLR_RESET
            );
        } else {
            let _ = writeln!(io.stdout, "{}✓ Healthy{}", CLR_GREEN, CLR_RESET);
        }
    } else if status == crate::daemon::InstanceStatus::Stopped {
        let _ = writeln!(io.stdout, "{}○ Stopped{}", CLR_YELLOW, CLR_RESET);
    } else {
        let _ = writeln!(io.stdout, "{}⚠ {}{}", CLR_YELLOW, msg, CLR_RESET);
    }

    0
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::{cli_test_env, run_cli};

    #[tokio::test]
    async fn test_strategy_command() {
        let _env = cli_test_env();

        // 1. View strategy
        let (code, stdout, _) = run_cli(&["strategy"], "").await;
        assert_eq!(code, 0, "strategy view expected 0, got {code}");
        assert!(
            stdout.contains("Current load balancing strategy:"),
            "expected strategy view output, got: {stdout}"
        );

        // 2. Set invalid strategy -> exit 1
        let (code, stdout, _) = run_cli(&["strategy", "invalid_choice_xyz"], "").await;
        assert_eq!(code, 1, "expected exit code 1 on invalid strategy, got {code}");
        assert!(
            stdout.contains("[Error] Invalid strategy 'invalid_choice_xyz'"),
            "expected invalid strategy error on stdout, got: {stdout}"
        );

        // 3. Set valid strategy -> exit 0
        let (code, stdout, _) = run_cli(&["strategy", "max_quota"], "").await;
        assert_eq!(code, 0, "expected exit code 0 on setting valid strategy, got {code}");
        assert!(
            stdout.contains("✓ Load balancing strategy set to:") && stdout.contains("max_quota"),
            "expected success message, got: {stdout}"
        );

        // Verify persistence in storage
        let pool = crate::storage::load_pool().unwrap();
        assert_eq!(pool.strategy, "max_quota");

        // 4. Removed strategies return invalid strategy error
        let (code, stdout, _) = run_cli(&["strategy", "least_used"], "").await;
        assert_eq!(
            code, 1,
            "expected exit code 1 for removed strategy least_used, got {code}"
        );
        assert!(
            stdout.contains("Invalid strategy"),
            "expected invalid strategy message on stdout, got: {stdout}"
        );
    }

    #[tokio::test]
    async fn test_status_cmd_no_quota_bars_and_dashboard() {
        let _env = cli_test_env();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let frac = 0.85;
        let acc1 = crate::storage::Account {
            id: "acc_1".to_string(),
            email: "secret1@example.com".to_string(),
            name: "MainAccount".to_string(),
            status: "ready".to_string(),
            request_count: 15,
            created_at: Some(now),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
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
        };
        let active_id = "acc_1".to_string();
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some(active_id),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc1],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let (code, stdout, _) = run_cli(&["status", "-p", "19999"], "").await;
        assert_eq!(code, 0, "expected exit 0 on status, got {code}");

        // Must contain status summary fields
        for expected in [
            "Gateway :",
            "STOPPED",
            "State   :",
            "Pool    : 1 accounts | CLI Base: MainAccount | 1 Ready | 0 Cooldown | 0 Restricted",
            "Strategy: max_quota",
            "Stopped",
        ] {
            assert!(
                stdout.contains(expected),
                "status output missing expected string {expected:?}\nGot: {stdout}"
            );
        }

        // Must NOT contain quota progress bars or account cards
        for prohibited in [
            "Gemini 5-Hour:",
            "Gemini Weekly:",
            "Quota age:",
            "Hits:",
            "Antigravity Multi-Account Pool",
            "Base account for CLI/auth metadata",
            "Available in rotation pool",
            "[━━━━",
            "[░░░░",
        ] {
            assert!(
                !stdout.contains(prohibited),
                "status output must not contain full dashboard element {prohibited:?}\nGot: {stdout}"
            );
        }
    }

    #[tokio::test]
    async fn test_status_cmd_running_daemon_with_runtime_stats() {
        let env = cli_test_env();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();

        let tmp_dir = env.dir.path();
        let ctx = crate::daemon::resolve_instance_context(Some(tmp_dir), None, Some(port), Some("127.0.0.1")).unwrap();

        let current_pid = std::process::id() as i32;
        let pid_info = crate::daemon::DaemonInfo {
            pid: current_pid,
            version: Some(ctx.version.clone()),
            script_mtime: None,
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx.config_hash.clone(),
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            listen_host: Some(ctx.primary_connect_addr(None).ip().to_string()),
            listen: Some(ctx.primary_connect_addr(None).ip().to_string()),
            port: Some(port),
            runtime: None,
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        let started_at = jiff::Timestamp::now()
            .checked_sub(jiff::Span::new().hours(1).minutes(27))
            .unwrap();
        let stats = crate::daemon::RuntimeStats {
            pid: current_pid,
            started_at,
            updated_at: jiff::Timestamp::now(),
            goroutines: 18,
            alloc_bytes: (124 * 1024 * 1024) / 10,
            heap_bytes: (187 * 1024 * 1024) / 10,
            sys_bytes: (312 * 1024 * 1024) / 10,
            num_gc: 42,
            go_version: "go1.26.7".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot::default(),
        };
        crate::daemon::write_runtime_stats(&ctx.runtime_file(), &stats).unwrap();

        let (code, stdout, stderr) = run_cli(
            &["status", "-p", &port.to_string(), "-D", &tmp_dir.to_string_lossy()],
            "",
        )
        .await;
        assert_eq!(code, 0, "expected exit 0 on status, got {code}. stderr: {stderr}");

        for expected in [
            "Gateway :",
            "RUNNING",
            &format!("PID {current_pid}"),
            &format!("127.0.0.1:{port}"),
            "Uptime  : 1h 27m",
            "Runtime : Go 1.26.7 | Goroutines: 18",
            "Memory  : Alloc 12.4 MiB | Heap 18.7 MiB | Sys 31.2 MiB",
            "GC      : 42 cycles",
            "State   :",
            "Pool    : 0 accounts (empty)",
            "Strategy: smart",
            "Healthy",
        ] {
            assert!(
                stdout.contains(expected),
                "status running output missing {expected:?}\nGot:\n{stdout}"
            );
        }
    }

    #[tokio::test]
    async fn test_status_cmd_running_daemon_stats_from_daemon_not_cli() {
        let env = cli_test_env();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();

        let tmp_dir = env.dir.path();
        let ctx = crate::daemon::resolve_instance_context(Some(tmp_dir), None, Some(port), Some("127.0.0.1")).unwrap();

        let current_pid = std::process::id() as i32;
        let pid_info = crate::daemon::DaemonInfo {
            pid: current_pid,
            version: Some(ctx.version.clone()),
            script_mtime: None,
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx.config_hash.clone(),
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            listen_host: Some(ctx.primary_connect_addr(None).ip().to_string()),
            listen: Some(ctx.primary_connect_addr(None).ip().to_string()),
            port: Some(port),
            runtime: None,
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        let started_at = jiff::Timestamp::now()
            .checked_sub(jiff::Span::new().seconds(37))
            .unwrap();
        let stats = crate::daemon::RuntimeStats {
            pid: current_pid,
            started_at,
            updated_at: jiff::Timestamp::now(),
            goroutines: 999,
            alloc_bytes: 50 * 1024 * 1024,
            heap_bytes: 60 * 1024 * 1024,
            sys_bytes: 80 * 1024 * 1024,
            num_gc: 777,
            go_version: "go1.99.9".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot::default(),
        };
        crate::daemon::write_runtime_stats(&ctx.runtime_file(), &stats).unwrap();

        let (code, stdout, _) = run_cli(
            &["status", "-p", &port.to_string(), "-D", &tmp_dir.to_string_lossy()],
            "",
        )
        .await;
        assert_eq!(code, 0);

        assert!(
            stdout.contains("Goroutines: 999"),
            "status must use daemon goroutine count (999)\nGot:\n{stdout}"
        );
        assert!(
            stdout.contains("GC      : 777 cycles"),
            "status must use daemon GC count (777)\nGot:\n{stdout}"
        );
        assert!(
            stdout.contains("Go 1.99.9"),
            "status must use daemon Go version\nGot:\n{stdout}"
        );
        assert!(
            stdout.contains("Uptime  : 37s") || stdout.contains("Uptime  : 38s"),
            "status must use daemon uptime (37s)\nGot:\n{stdout}"
        );
    }

    #[tokio::test]
    async fn test_status_cmd_running_daemon_stats_unavailable() {
        let env = cli_test_env();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();

        let tmp_dir = env.dir.path();
        let ctx = crate::daemon::resolve_instance_context(Some(tmp_dir), None, Some(port), Some("127.0.0.1")).unwrap();

        let current_pid = std::process::id() as i32;
        let pid_info = crate::daemon::DaemonInfo {
            pid: current_pid,
            version: Some(ctx.version.clone()),
            script_mtime: None,
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx.config_hash.clone(),
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            listen_host: Some(ctx.primary_connect_addr(None).ip().to_string()),
            listen: Some(ctx.primary_connect_addr(None).ip().to_string()),
            port: Some(port),
            runtime: None,
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        let (code, stdout, _) = run_cli(
            &["status", "-p", &port.to_string(), "-D", &tmp_dir.to_string_lossy()],
            "",
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            stdout.contains("Runtime : (metrics unavailable)"),
            "expected metrics unavailable message\nGot:\n{stdout}"
        );
    }

    #[tokio::test]
    async fn test_audit_routing_removed_from_cli() {
        let _env = cli_test_env();

        for cmd in ["audit-routing", "monitor-routing"] {
            let (code, _, stderr) = run_cli(&[cmd], "").await;
            assert_eq!(code, 2, "expected code 2 when invoking removed command {cmd}");
            assert!(
                stderr.contains(&format!("invalid choice: '{cmd}'")),
                "expected invalid choice in stderr, got: {stderr}"
            );
            assert!(
                !stderr.contains(&format!("{cmd},")),
                "usage list in error output should not contain {cmd}: {stderr}"
            );
        }

        let (code, stdout, _) = run_cli(&["--help"], "").await;
        assert_eq!(code, 0);
        for term in ["audit-routing", "monitor-routing"] {
            assert!(
                !stdout.contains(term),
                "help text must not mention {term}, got: {stdout}"
            );
        }
    }

    #[tokio::test]
    async fn test_cli_status_multi_instance() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let l_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = l_a.local_addr().unwrap().port();
        drop(l_a);

        let l_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = l_b.local_addr().unwrap().port();
        drop(l_b);

        let data_dir_a = tmp_dir.join("cli-data-a");
        let data_dir_b = tmp_dir.join("cli-data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let cfg_a = tmp_dir.join("cfg-a.json");
        let cfg_b = tmp_dir.join("cfg-b.json");
        std::fs::write(&cfg_a, format!(r#"{{"version": 1, "server":{{"port":{port_a}}}}}"#)).unwrap();
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port_b}}}}}"#)).unwrap();

        let ctx_a =
            crate::daemon::resolve_instance_context(Some(&data_dir_a), Some(&cfg_a), Some(port_a), Some("127.0.0.1"))
                .unwrap();

        // Both stopped initially
        let (code_a, out_a, _) = run_cli(
            &[
                "status",
                "-D",
                &data_dir_a.to_string_lossy(),
                "-c",
                &cfg_a.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code_a, 0);
        assert!(out_a.contains("Gateway :") && out_a.contains("STOPPED"));

        let (code_b, out_b, _) = run_cli(
            &[
                "status",
                "-D",
                &data_dir_b.to_string_lossy(),
                "-c",
                &cfg_b.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code_b, 0);
        assert!(out_b.contains("Gateway :") && out_b.contains("STOPPED"));

        // Write simulated running PID file for A
        let pid_info_a = crate::daemon::DaemonInfo {
            pid: std::process::id() as i32,
            version: Some(ctx_a.version.clone()),
            script_mtime: None,
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            listen_host: Some("127.0.0.1".to_string()),
            listen: Some("127.0.0.1".to_string()),
            port: Some(port_a),
            runtime: None,
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx_a.pid_file(), pid_info_a).unwrap();

        let _listener_a = std::net::TcpListener::bind(format!("127.0.0.1:{port_a}")).unwrap();

        // Query status on A -> RUNNING
        let (code_a, out_a, _) = run_cli(
            &[
                "status",
                "-D",
                &data_dir_a.to_string_lossy(),
                "-c",
                &cfg_a.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code_a, 0);
        assert!(out_a.contains("Gateway :") && out_a.contains("RUNNING"));

        // Query status on B -> STILL STOPPED!
        let (code_b, out_b, _) = run_cli(
            &[
                "status",
                "-D",
                &data_dir_b.to_string_lossy(),
                "-c",
                &cfg_b.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code_b, 0);
        assert!(out_b.contains("Gateway :") && out_b.contains("STOPPED"));
    }

    #[tokio::test]
    async fn test_observability_case_o_status_verbose_and_privacy() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let frac = 0.95;
        let expiry = (now + 3600) as f64;
        let secret_email = "sensitive-developer-account@internal.corp.example.com";
        let secret_token = "ya29.a0AfH6SMD-SuperSecretOauthAccessToken123456789";

        let acc = crate::storage::Account {
            id: "acc_sensitive_1".to_string(),
            email: secret_email.to_string(),
            name: "ProdDeveloper".to_string(),
            status: "ready".to_string(),
            request_count: 150,
            created_at: Some(now),
            access_token: secret_token.to_string(),
            refresh_token: "1//04mock_refresh_token_abcdef".to_string(),
            token_expiry: Some(expiry),
            last_quota: Some(crate::storage::QuotaState {
                remaining_fraction: Some(frac),
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
        };
        let active_id = acc.id.clone();
        let pool = crate::storage::Pool {
            version: 1,
            active_account_id: Some(active_id),
            strategy: crate::config::STRATEGY_MAX_QUOTA.to_string(),
            accounts: vec![acc],
            ..Default::default()
        };
        crate::storage::save_pool(&pool).unwrap();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();

        let ctx = crate::daemon::resolve_instance_context(Some(tmp_dir), None, Some(port), Some("127.0.0.1")).unwrap();

        let current_pid = std::process::id() as i32;
        let pid_info = crate::daemon::DaemonInfo {
            pid: current_pid,
            version: Some(ctx.version.clone()),
            script_mtime: None,
            config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx.config_hash.clone(),
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            listen_host: Some(ctx.primary_connect_addr(None).ip().to_string()),
            listen: Some(ctx.primary_connect_addr(None).ip().to_string()),
            port: Some(port),
            runtime: None,
            ..Default::default()
        };
        crate::daemon::write_pid_file(&ctx.pid_file(), pid_info).unwrap();

        let started_at = jiff::Timestamp::now().checked_sub(jiff::Span::new().hours(2)).unwrap();
        let stats = crate::daemon::RuntimeStats {
            pid: current_pid,
            started_at,
            updated_at: jiff::Timestamp::now(),
            goroutines: 14,
            alloc_bytes: 15 * 1024 * 1024,
            heap_bytes: 22 * 1024 * 1024,
            sys_bytes: 35 * 1024 * 1024,
            num_gc: 12,
            go_version: "go1.26.7".to_string(),
            in_flight: None,
            model_pauses: None,
            last_model: None,
            smart_rr_cursor: None,
            snapshot: crate::observability::Snapshot {
                generation_requests: 1250,
                generation_success: 1240,
                routing_decisions: 1250,
                failover_attempts: 15,
                failover_success: 14,
                restricted_skips: 5,
                quota_refresh_attempts: 80,
                quota_refresh_success: 78,
                quota_refresh_failure: 2,
                auth_refresh_attempts: 10,
                auth_refresh_success: 10,
                auth_refresh_failure: 0,
                persistence_errors: 1,
                no_replay_prevented: 3,
                lease_fallbacks: 0,
            },
        };
        crate::daemon::write_runtime_stats(&ctx.runtime_file(), &stats).unwrap();

        // 1. Default status (no -v/--verbose) MUST NOT contain "Runtime Counters"
        let (code, default_str, _) = run_cli(
            &["status", "-p", &port.to_string(), "-D", &tmp_dir.to_string_lossy()],
            "",
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            !default_str.contains("Runtime Counters"),
            "default status output should not contain 'Runtime Counters', got:\n{default_str}"
        );

        // 2. Verbose status (-v) MUST contain "Runtime Counters" with formatted values
        let (code, verbose_str, _) = run_cli(
            &[
                "status",
                "-v",
                "-p",
                &port.to_string(),
                "-D",
                &tmp_dir.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code, 0);

        let expected_sections = [
            "Runtime Counters",
            "Generation",
            "1,250 requests / 1,240 success",
            "Routing",
            "1,250 decisions",
            "Failover",
            "15 attempts / 14 recovered",
            "Restricted skips",
            "5",
            "Quota refresh",
            "80 / 78 ok / 2 failed",
            "Auth refresh",
            "10 / 10 ok / 0 failed",
            "Persistence errors",
            "1",
            "Replay prevented",
            "3",
        ];
        for exp in expected_sections {
            assert!(
                verbose_str.contains(exp),
                "verbose status output missing expected section {exp:?}\nGot:\n{verbose_str}"
            );
        }

        // 3. Privacy assertions: Output must NEVER expose account email, tokens, URLs, or secrets
        let prohibited_patterns = [
            secret_email,
            secret_token,
            "ya29.",
            "refresh_token",
            "https://",
            "http://",
            "@internal.corp.example.com",
            "prompt",
            "content",
        ];
        for pattern in prohibited_patterns {
            assert!(
                !verbose_str.contains(pattern),
                "SECURITY/PRIVACY VIOLATION: verbose status leaked {pattern:?}"
            );
            assert!(
                !default_str.contains(pattern),
                "SECURITY/PRIVACY VIOLATION: default status leaked {pattern:?}"
            );
        }

        // 4. Stopped gateway with --verbose renders clean indicator
        drop(l);
        let _ = std::fs::remove_file(ctx.pid_file());
        let (code, stopped_str, _) = run_cli(
            &[
                "status",
                "--verbose",
                "-p",
                &port.to_string(),
                "-D",
                &tmp_dir.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            stopped_str.contains("Runtime Counters") && stopped_str.contains("(gateway not running)"),
            "expected '(gateway not running)' in stopped verbose status, got:\n{stopped_str}"
        );
    }
}
