use crate::cli::Io;
use crate::cli::{CLR_CYAN, CLR_GREEN, CLR_RED, CLR_RESET, CLR_YELLOW};

pub async fn handle_logs(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut lines = 20;
    let mut follow = false;
    let mut clear = false;
    let mut rotate = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-f" || arg == "--follow" {
            follow = true;
        } else if arg == "--clear" || arg == "--clean" {
            clear = true;
        } else if arg == "--rotate" {
            rotate = true;
        } else if (arg == "-n" || arg == "--lines") && i + 1 < args.len() {
            if let Ok(n) = args[i + 1].parse::<usize>() {
                lines = n;
            }
            i += 1;
        }
        i += 1;
    }

    let log_file = crate::config::log_file();
    let max_log_bytes = crate::config::DEFAULT_MAX_LOG_BYTES;
    let backup_count = crate::config::DEFAULT_BACKUP_LOG_COUNT;

    if clear {
        let _ = crate::daemon::clear_log(Some(&log_file), backup_count as i32);
        let _ = writeln!(
            io.stdout,
            "{}✓ Successfully cleared gateway log ({}){}",
            CLR_GREEN,
            log_file.display(),
            CLR_RESET
        );
        return 0;
    }

    if rotate {
        match std::fs::metadata(&log_file) {
            Ok(fi) if fi.len() > 0 => {
                let rotated =
                    crate::daemon::rotate_log_if_needed(Some(&log_file), max_log_bytes, backup_count as i32, true);
                if let Ok(true) = rotated {
                    let _ = writeln!(
                        io.stdout,
                        "{}✓ Rotated active log: {} -> {}.1{}",
                        CLR_GREEN,
                        log_file.display(),
                        log_file.display(),
                        CLR_RESET
                    );
                } else {
                    let _ = writeln!(io.stdout, "{}[Error] Log rotation failed.{}", CLR_RED, CLR_RESET);
                }
            }
            _ => {
                let _ = writeln!(
                    io.stdout,
                    "{}Log file is empty or does not exist, rotation skipped.{}",
                    CLR_YELLOW, CLR_RESET
                );
            }
        }
        return 0;
    }

    let fi = match std::fs::metadata(&log_file) {
        Ok(m) => m,
        Err(_) => {
            let _ = writeln!(
                io.stdout,
                "{}Log file does not exist yet: {}{}",
                CLR_YELLOW,
                log_file.display(),
                CLR_RESET
            );
            return 0;
        }
    };

    let content = match std::fs::read_to_string(&log_file) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                io.stdout,
                "{}[Error] Failed to read log file: {}{}",
                CLR_RED, e, CLR_RESET
            );
            return 0;
        }
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let total_lines = all_lines.len();

    let backup_file = format!("{}.1", log_file.display());
    let backup_info = if let Ok(bfi) = std::fs::metadata(&backup_file) {
        format!(" | Backup: {}", crate::daemon::format_size(bfi.len() as i64))
    } else {
        String::new()
    };

    let _ = writeln!(io.stdout, "{}", "=".repeat(68));
    let _ = writeln!(
        io.stdout,
        "                    Antigravity Gateway Log                         "
    );
    let _ = writeln!(io.stdout, "{}", "=".repeat(68));
    let _ = writeln!(io.stdout, "Path: {}", log_file.display());
    let _ = writeln!(
        io.stdout,
        "Size: {} ({} lines){}",
        crate::daemon::format_size(fi.len() as i64),
        total_lines,
        backup_info
    );
    let _ = writeln!(
        io.stdout,
        "Policy: Auto-rotate at {} (retaining {} backup)",
        crate::daemon::format_size(max_log_bytes),
        backup_count
    );
    let _ = writeln!(io.stdout, "{}", "-".repeat(68));

    let start = if lines > 0 && total_lines > lines {
        total_lines - lines
    } else {
        0
    };
    for l in &all_lines[start..] {
        let _ = writeln!(io.stdout, "{}", l);
    }

    if follow {
        let _ = writeln!(io.stdout, "{}", "-".repeat(68));
        let _ = writeln!(
            io.stdout,
            "{}Streaming live logs (Ctrl+C to stop)...{}",
            CLR_CYAN, CLR_RESET
        );
    }

    0
}

pub async fn start_daemon_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (ctx, _) = match crate::cli::resolve_context_for_cmd(args) {
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
    let opts = crate::daemon::LaunchOptions {
        port: ctx.listen_port,
        state_dir: Some(ctx.data_dir.clone()),
        config_file: ctx.config_path.clone(),
        pid_file: Some(ctx.pid_file()),
        log_file: Some(ctx.log_file()),
        entrypoint_path: None,
        foreground: false,
        custom_args: Vec::new(),
    };
    match crate::daemon::start_instance(&ctx, opts) {
        Ok(res) => {
            if res.already_run {
                let _ = writeln!(
                    io.stdout,
                    "{}Proxy daemon is already running (PID: {}).{}",
                    CLR_YELLOW, res.pid, CLR_RESET
                );
            } else {
                let _ = writeln!(
                    io.stdout,
                    "{}✓ Started agy-pool gateway proxy on http://{}:{} (PID: {}){}",
                    CLR_GREEN, ctx.listen_host, ctx.listen_port, res.pid, CLR_RESET
                );
            }
            0
        }
        Err(_) => {
            let _ = writeln!(
                io.stdout,
                "{}[Error] Failed to start gateway. Check {}{}",
                CLR_RED,
                ctx.log_file().display(),
                CLR_RESET
            );
            1
        }
    }
}

pub async fn stop_daemon_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (ctx, _) = match crate::cli::resolve_context_for_cmd(args) {
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
    match crate::daemon::stop_instance(&ctx, std::time::Duration::from_secs(3)) {
        Ok(()) => {
            let _ = writeln!(io.stdout, "{}✓ Stopped proxy daemon{}", CLR_GREEN, CLR_RESET);
            0
        }
        Err(crate::daemon::DaemonError::NotRunning) => {
            let _ = writeln!(io.stdout, "{}Proxy daemon is not running.{}", CLR_YELLOW, CLR_RESET);
            0
        }
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Cannot stop proxy daemon: {}{}",
                CLR_RED, e, CLR_RESET
            );
            1
        }
    }
}

pub async fn restart_daemon_cmd(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (ctx, _) = match crate::cli::resolve_context_for_cmd(args) {
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
    let opts = crate::daemon::LaunchOptions {
        port: ctx.listen_port,
        state_dir: Some(ctx.data_dir.clone()),
        config_file: ctx.config_path.clone(),
        pid_file: Some(ctx.pid_file()),
        log_file: Some(ctx.log_file()),
        entrypoint_path: None,
        foreground: false,
        custom_args: Vec::new(),
    };
    match crate::daemon::restart_instance(&ctx, opts) {
        Ok(res) => {
            let _ = writeln!(
                io.stdout,
                "{}✓ Started agy-pool gateway proxy on http://{}:{} (PID: {}){}",
                CLR_GREEN, ctx.listen_host, ctx.listen_port, res.pid, CLR_RESET
            );
            0
        }
        Err(e) => {
            let _ = writeln!(
                io.stderr,
                "{}[Error] Failed to restart gateway: {}{}",
                CLR_RED, e, CLR_RESET
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::{cli_test_env, run_cli};

    #[tokio::test]
    async fn test_logs_command() {
        let _env = cli_test_env();

        let log_file = crate::config::log_file();
        std::fs::write(&log_file, "line1\nline2\nline3\n").unwrap();

        let (code, stdout, _) = run_cli(&["logs", "-n", "2"], "").await;
        assert_eq!(code, 0, "logs expected 0, got {code}");
        assert!(stdout.contains("Antigravity Gateway Log"));
        assert!(stdout.contains("line2") && stdout.contains("line3"));

        // Clear logs
        let (code, stdout, _) = run_cli(&["logs", "--clear"], "").await;
        assert_eq!(code, 0, "logs --clear expected 0, got {code}");
        assert!(stdout.contains("Successfully cleared gateway log"));
    }

    #[tokio::test]
    async fn test_daemon_run_removed_from_cli() {
        let _env = cli_test_env();

        // 1. Calling "daemon-run" directly must fail with invalid choice (code 2)
        let (code, _, stderr) = run_cli(&["daemon-run"], "").await;
        assert_eq!(code, 2, "expected code 2 when invoking removed daemon-run, got {code}");
        assert!(
            stderr.contains("invalid choice: 'daemon-run'"),
            "expected 'invalid choice: 'daemon-run'' in stderr, got: {stderr}"
        );
        assert!(
            !stderr.contains("daemon-run,"),
            "usage list in error output should not contain 'daemon-run': {stderr}"
        );

        // 2. Help output must not mention daemon-run
        let (code, stdout, _) = run_cli(&["--help"], "").await;
        assert_eq!(code, 0);
        assert!(
            !stdout.contains("daemon-run"),
            "help text must not mention 'daemon-run', got: {stdout}"
        );
    }

    #[tokio::test]
    async fn test_cli_stop_cross_instance_rejection() {
        let env = cli_test_env();
        let tmp_dir = env.dir.path();

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);

        let data_dir = tmp_dir.join("cli-stop-data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let cfg_a = tmp_dir.join("cfg-a.json");
        let cfg_b = tmp_dir.join("cfg-b.json");
        std::fs::write(&cfg_a, format!(r#"{{"version": 1, "server":{{"port":{port}}}}}"#)).unwrap();
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port}}}}}"#)).unwrap();

        let ctx_a =
            crate::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_a), Some(port), Some("127.0.0.1"))
                .unwrap();

        // Simulate running daemon with cfgA
        let pid_info_a = crate::daemon::DaemonInfo {
            pid: std::process::id() as i32,
            version: Some(ctx_a.version.clone()),
            script_mtime: None,
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            listen_host: Some("127.0.0.1".to_string()),
            listen: Some("127.0.0.1".to_string()),
            port: Some(port),
            runtime: None,
        };
        crate::daemon::write_pid_file(&ctx_a.pid_file(), pid_info_a).unwrap();

        // Attempt stop with cfgB -> REJECTED
        let (code, _, stderr) = run_cli(
            &[
                "stop",
                "-D",
                &data_dir.to_string_lossy(),
                "-c",
                &cfg_b.to_string_lossy(),
            ],
            "",
        )
        .await;
        assert_ne!(
            code, 0,
            "expected non-zero exit code when stopping with wrong config, got {code}"
        );
        assert!(
            stderr.contains("daemon configuration mismatch") || stderr.contains("cannot stop with config"),
            "expected config mismatch error, got: {stderr}"
        );

        // PID file must still exist
        assert!(
            ctx_a.pid_file().exists(),
            "PID file must not be removed on rejected stop"
        );
    }
}
