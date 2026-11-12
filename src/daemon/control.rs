use std::path::Path;
use std::time::{Duration, Instant};

use super::DaemonError;

/// Stops the daemon instance using the specified PID file path.
pub fn stop_with_file(
    ctx: &super::instance::InstanceContext,
    pid_file: &Path,
    mut timeout: Duration,
) -> Result<(), DaemonError> {
    crate::config::assert_safe_write_path(pid_file)?;

    let info = match super::pid::read_pid_file(pid_file) {
        Ok(info) => info,
        Err(DaemonError::PIDNotFound) => return Err(DaemonError::NotRunning),
        Err(err) => {
            // Malformed PID file: safely clean up if configured listen targets are not listening
            let addrs = super::server::resolve_context_socket_addrs(ctx).unwrap_or_default();
            if !super::server::any_addr_listening(&addrs) {
                let _ = std::fs::remove_file(pid_file);
                return Ok(());
            }
            return Err(DaemonError::Message(format!(
                "cannot safely remove malformed PID file while {} is listening: {err}",
                ctx.display_listen(None)
            )));
        }
    };

    let pid = info.pid;
    if !super::pid::is_process_alive(pid) {
        // Process already dead: safely clean up stale PID file
        let _ = std::fs::remove_file(pid_file);
        return Ok(());
    }

    // 1. Validate process identity: NEVER signal foreign process!
    if !super::pid::is_agy_pool_process(pid) {
        let _ = std::fs::remove_file(pid_file);
        return Err(DaemonError::PIDReused);
    }

    // 2. Validate data directory ownership: NEVER signal another instance!
    if let Some(info_dir) = &info.data_dir {
        if !info_dir.is_empty() {
            let can_info = super::instance::canonicalize_dir(Path::new(info_dir))
                .unwrap_or_else(|_| std::path::PathBuf::from(info_dir));
            if can_info != ctx.data_dir {
                return Err(DaemonError::PIDNotOwned(format!(
                    "PID file contains foreign data dir {:?} (expected {:?})",
                    info_dir, ctx.data_dir
                )));
            }
        }
    } else {
        // Legacy metadata: check if process is owned by this dataDir
        if !super::instance::is_process_owned_by_data_dir(pid, &ctx.data_dir) {
            return Err(DaemonError::PIDNotOwned(format!(
                "cannot verify ownership of legacy PID {} for data dir {:?}",
                pid, ctx.data_dir
            )));
        }
    }

    // 3. Port validation
    if let Some(port) = info.port
        && port > 0
        && ctx.listen_port > 0
        && port != ctx.listen_port
    {
        return Err(DaemonError::PIDNotOwned(format!(
            "daemon running on port {}, requested {}",
            port, ctx.listen_port
        )));
    }

    // 4. Config path validation (refuse stop if caller specified a different config file)
    if let (Some(info_cfg), Some(ctx_cfg)) = (&info.config_path, &ctx.config_path) {
        let can_info_cfg = super::instance::canonicalize_file(Path::new(info_cfg))
            .unwrap_or_else(|_| std::path::PathBuf::from(info_cfg));
        if can_info_cfg != *ctx_cfg {
            return Err(DaemonError::ConfigMismatch(format!(
                "daemon running with config {:?}, cannot stop with config {:?}",
                info_cfg, ctx.config_path
            )));
        }
    } else if ctx.config_path.is_some() && info.config_path.is_none() {
        return Err(DaemonError::ConfigMismatch(format!(
            "daemon running without config, cannot stop with config {:?}",
            ctx.config_path
        )));
    }

    // 5. Stop through systemd when it supervises the daemon, otherwise send SIGTERM
    let scope = super::systemd::systemd_scope(pid);
    if !scope.is_empty() {
        super::systemd::systemctl(scope, "stop")?;
    } else {
        let _ = super::systemd::send_signal(pid, libc::SIGTERM);
    }

    // 6. Poll until process exits (up to timeout)
    if timeout.is_zero() {
        timeout = Duration::from_secs(3);
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !super::pid::is_process_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if super::pid::is_process_alive(pid) {
        return Err(DaemonError::Message(format!(
            "daemon (PID: {pid}) did not terminate within {timeout:?}"
        )));
    }

    // 7. Unlink PID file if owned
    let _ = super::pid::remove_pid_file_if_owned(pid_file, pid);
    Ok(())
}

/// StopInstance gracefully stops the daemon instance uniquely identified by ctx.
pub fn stop_instance(ctx: &super::instance::InstanceContext, timeout: Duration) -> Result<(), DaemonError> {
    stop_with_file(ctx, &ctx.pid_file(), timeout)
}

/// StopDaemon stops the running proxy daemon gracefully with SIGTERM.
pub fn stop_daemon(pid_file: Option<&Path>, port: u16, timeout: Duration) -> Result<(), DaemonError> {
    let resolved = match pid_file {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => crate::config::pid_file(),
    };
    let data_dir = resolved
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(crate::config::data_dir);

    let mut config_path = None;
    if let Some(info) = super::pid::get_daemon_info(Some(&resolved))
        && let Some(cfg) = info.config_path
    {
        config_path = Some(std::path::PathBuf::from(cfg));
    }

    let ctx = super::instance::resolve_instance_context(Some(&data_dir), config_path.as_deref(), Some(port), None)?;

    stop_with_file(&ctx, &resolved, timeout)
}

/// RestartInstance stops the existing daemon for ctx, waits for port release, and starts the new daemon.
pub fn restart_instance(
    ctx: &super::instance::InstanceContext,
    mut opts: super::lifecycle::LaunchOptions,
) -> Result<super::lifecycle::StartResult, DaemonError> {
    let pid_file = opts.pid_file.clone().unwrap_or_else(|| ctx.pid_file());

    let (status, info, msg) = super::instance::check_status_with_file(ctx, &pid_file);
    if status == super::instance::InstanceStatus::ForeignInstance
        || status == super::instance::InstanceStatus::PortOccupiedForeign
    {
        return Err(DaemonError::Message(format!("cannot restart instance: {msg}")));
    }

    // Resolve new target sockets before restarting/stopping the running daemon so invalid/missing interfaces fail closed.
    let new_addrs = super::server::resolve_context_socket_addrs(ctx)?;

    // A systemd-supervised daemon is restarted by its unit; spawning our own would fight it for the PID lock.
    if let Some(info) = super::pid::get_daemon_info(Some(&pid_file))
        && super::pid::is_process_alive(info.pid)
        && super::pid::is_agy_pool_process(info.pid)
    {
        let scope = super::systemd::systemd_scope(info.pid);
        if !scope.is_empty() {
            super::systemd::systemctl(scope, "restart")?;
            return wait_restarted(ctx, &pid_file, info.pid, scope);
        }
    }

    // Determine identity of the running daemon to stop under its recorded port/config.
    let live_info = info.as_ref().filter(|i| super::pid::is_process_alive(i.pid));
    let old_pid = live_info.map(|i| i.pid);
    let mut stop_ctx = ctx.clone();
    let mut old_addrs = Vec::new();
    if let Some(i) = live_info {
        if let Some(port) = i.port {
            stop_ctx.listen_port = port;
        }
        stop_ctx.config_path = i
            .config_path
            .as_ref()
            .map(|c| super::instance::canonicalize_file(Path::new(c)).unwrap_or_else(|_| std::path::PathBuf::from(c)));
        old_addrs = super::server::resolve_info_socket_addrs(i, 0);
    }

    // Verify any new target sockets not already held by the running daemon are free before stopping it.
    let newly_claimed: Vec<std::net::SocketAddr> = new_addrs
        .iter()
        .copied()
        .filter(|a| {
            !old_addrs.contains(a)
                && !old_addrs
                    .iter()
                    .any(|old| old.port() == a.port() && (old.ip().is_unspecified() || a.ip().is_unspecified()))
        })
        .collect();
    if !newly_claimed.is_empty() && super::server::any_addr_listening(&newly_claimed) {
        return Err(DaemonError::ForeignPortOccupied(format!(
            "target endpoint(s) {} occupied by another process; cannot move daemon from port {}",
            super::server::format_socket_addrs(&newly_claimed),
            stop_ctx.listen_port
        )));
    }

    // 1. Stop existing daemon
    let _ = stop_with_file(&stop_ctx, &pid_file, Duration::from_secs(5));
    if let Some(pid) = old_pid
        && super::pid::is_process_alive(pid)
    {
        return Err(DaemonError::Message(format!(
            "cannot restart instance: existing daemon (PID {pid}) did not terminate"
        )));
    }

    // 2. Bounded wait for socket release
    let wait_addrs = if !old_addrs.is_empty() {
        old_addrs
    } else {
        super::server::resolve_context_socket_addrs(&stop_ctx).unwrap_or_else(|_| new_addrs.clone())
    };
    if !wait_addrs.is_empty() {
        for _ in 0..30 {
            if !super::server::any_addr_listening(&wait_addrs) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // 3. Start new daemon with same instance configuration
    opts.state_dir = Some(ctx.data_dir.clone());
    opts.config_file = ctx.config_path.clone();
    opts.port = ctx.listen_port;
    opts.pid_file = Some(pid_file);

    let mut res = super::lifecycle::start_instance(ctx, opts)?;
    res.was_restarted = true;
    Ok(res)
}

fn wait_restarted(
    ctx: &super::instance::InstanceContext,
    pid_file: &Path,
    old_pid: i32,
    scope: &str,
) -> Result<super::lifecycle::StartResult, DaemonError> {
    let expected_addrs = super::server::resolve_context_socket_addrs(ctx)?;
    let max_attempts = if cfg!(test) { 3 } else { 50 };
    for _ in 0..max_attempts {
        if let Some(info) = super::pid::get_daemon_info(Some(pid_file))
            && info.pid != old_pid
            && super::pid::is_process_alive(info.pid)
            && super::server::all_addrs_listening(&expected_addrs)
        {
            return Ok(super::lifecycle::StartResult {
                pid: info.pid,
                already_run: false,
                was_restarted: true,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Err(DaemonError::Message(format!(
        "{} restarted but the gateway is not listening on {}; inspect `systemctl {} status {}` and {:?}",
        super::systemd::SYSTEMD_UNIT_NAME,
        ctx.display_listen(None),
        scope,
        super::systemd::SYSTEMD_UNIT_NAME,
        ctx.log_file()
    )))
}

/// RestartDaemon stops the existing daemon, waits for port release, and starts the new daemon.
pub fn restart_daemon(opts: super::lifecycle::LaunchOptions) -> Result<super::lifecycle::StartResult, DaemonError> {
    let data_dir = opts
        .state_dir
        .clone()
        .or_else(|| opts.pid_file.as_ref().and_then(|p| p.parent().map(|p| p.to_path_buf())));

    let ctx = super::instance::resolve_instance_context(
        data_dir.as_deref(),
        opts.config_file.as_deref(),
        Some(opts.port),
        None,
    )?;

    restart_instance(&ctx, opts)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn test_stop_daemon_scenarios() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("stop.pid");

        // 1. Stop nonexistent PID file
        let err = stop_daemon(Some(&pid_file), 12345, Duration::from_millis(100));
        assert!(matches!(err, Err(DaemonError::NotRunning)));

        // 2. Stop with dead PID: cleans up stale PID file
        let mut dead_pid = 4194300;
        while super::super::pid::is_process_alive(dead_pid) {
            dead_pid -= 1;
        }

        super::super::pid::write_pid_file(
            &pid_file,
            super::super::pid::DaemonInfo {
                pid: dead_pid,
                ..Default::default()
            },
        )
        .unwrap();

        let err = stop_daemon(Some(&pid_file), 12345, Duration::from_millis(100));
        assert!(err.is_ok());
        assert!(!pid_file.exists());

        // 3. Stop with foreign PID (e.g. PID 1): refuses to kill, cleans up stale file
        if Path::new("/proc/1/cmdline").exists() {
            super::super::pid::write_pid_file(
                &pid_file,
                super::super::pid::DaemonInfo {
                    pid: 1,
                    ..Default::default()
                },
            )
            .unwrap();

            let err = stop_daemon(Some(&pid_file), 12345, Duration::from_millis(100));
            assert!(matches!(err, Err(DaemonError::PIDReused)));
            assert!(!pid_file.exists());
        }
    }

    #[test]
    fn test_restart_daemon_sequencing() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("restart.pid");
        let log_file = env.temp_dir.path().join("restart.log");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let opts = super::super::lifecycle::LaunchOptions {
            port,
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };

        let res1 = super::super::lifecycle::start_daemon(opts.clone()).unwrap();
        assert!(res1.pid > 0);
        let pid1 = res1.pid;
        let mut kill_guard1 = crate::daemon::lifecycle::ProcessKillGuard::new(pid1);

        assert!(super::super::server::is_daemon_running(Some(&pid_file), port));

        // Restart
        let res2 = restart_daemon(opts).unwrap();
        assert!(res2.pid > 0);
        let pid2 = res2.pid;
        let mut kill_guard2 = crate::daemon::lifecycle::ProcessKillGuard::new(pid2);

        assert_ne!(pid1, pid2, "expected new PID after restart");
        assert!(!super::super::pid::is_process_alive(pid1));
        assert!(super::super::server::is_daemon_running(Some(&pid_file), port));

        // Cleanup
        let _ = stop_daemon(Some(&pid_file), port, Duration::from_secs(3));
        kill_guard1.disarm();
        kill_guard2.disarm();
    }

    #[test]
    fn test_restart_moves_own_daemon_to_new_port() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("move.pid");
        let log_file = env.temp_dir.path().join("move.log");

        let free_port = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (port_a, port_b) = (free_port(), free_port());
        let write_cfg = |name: &str, port: u16| {
            let p = env.temp_dir.path().join(name);
            std::fs::write(
                &p,
                format!(r#"{{"version": 1, "server": {{"listen": "127.0.0.1", "port": {port}}}}}"#),
            )
            .unwrap();
            p
        };
        let (cfg_a, cfg_b) = (write_cfg("config-a.json", port_a), write_cfg("config-b.json", port_b));

        let opts = super::super::lifecycle::LaunchOptions {
            port: port_a,
            config_file: Some(cfg_a),
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };
        let res1 = super::super::lifecycle::start_daemon(opts.clone()).unwrap();
        let mut kill_guard1 = crate::daemon::lifecycle::ProcessKillGuard::new(res1.pid);

        // Switch both config file and port, as `agy-pool restart -c config-b.json` does.
        let res2 = restart_daemon(super::super::lifecycle::LaunchOptions {
            port: port_b,
            config_file: Some(cfg_b),
            ..opts
        })
        .expect("restart with another config must move this instance's own daemon");
        let mut kill_guard2 = crate::daemon::lifecycle::ProcessKillGuard::new(res2.pid);

        assert_ne!(res1.pid, res2.pid);
        assert!(!super::super::pid::is_process_alive(res1.pid));
        assert!(super::super::server::is_daemon_running(Some(&pid_file), port_b));
        assert!(!super::super::server::is_port_listening(port_a));

        let _ = stop_daemon(Some(&pid_file), port_b, Duration::from_secs(3));
        kill_guard1.disarm();
        kill_guard2.disarm();
    }

    #[test]
    fn test_restart_preserves_running_daemon_if_target_port_occupied() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("preserve.pid");
        let log_file = env.temp_dir.path().join("preserve.log");

        let free_port = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (port_a, port_b) = (free_port(), free_port());
        let write_cfg = |name: &str, port: u16| {
            let p = env.temp_dir.path().join(name);
            std::fs::write(
                &p,
                format!(r#"{{"version": 1, "server": {{"listen": "127.0.0.1", "port": {port}}}}}"#),
            )
            .unwrap();
            p
        };
        let (cfg_a, cfg_b) = (write_cfg("cfg-a.json", port_a), write_cfg("cfg-b.json", port_b));

        let opts = super::super::lifecycle::LaunchOptions {
            port: port_a,
            config_file: Some(cfg_a),
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };
        let res1 = super::super::lifecycle::start_daemon(opts.clone()).unwrap();
        let mut kill_guard1 = crate::daemon::lifecycle::ProcessKillGuard::new(res1.pid);

        // Occupy port_b with a foreign listener
        let foreign_listener = std::net::TcpListener::bind(format!("127.0.0.1:{port_b}")).unwrap();

        // Attempting restart to occupied port_b must fail and NOT kill the existing daemon on port_a
        let err = restart_daemon(super::super::lifecycle::LaunchOptions {
            port: port_b,
            config_file: Some(cfg_b),
            ..opts
        })
        .unwrap_err();
        assert!(matches!(err, DaemonError::ForeignPortOccupied(_)));

        // Daemon on port_a must still be running and alive
        assert!(super::super::pid::is_process_alive(res1.pid));
        assert!(super::super::server::is_daemon_running(Some(&pid_file), port_a));

        drop(foreign_listener);
        let _ = stop_daemon(Some(&pid_file), port_a, Duration::from_secs(3));
        kill_guard1.disarm();
    }

    #[test]
    fn test_concurrent_start_protection() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("concurrent.pid");
        let log_file = env.temp_dir.path().join("concurrent.log");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let opts = super::super::lifecycle::LaunchOptions {
            port,
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };

        let results = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let o = opts.clone();
            let res_clone = Arc::clone(&results);
            handles.push(std::thread::spawn(move || {
                if let Ok(r) = super::super::lifecycle::start_daemon(o) {
                    res_clone.lock().unwrap().push(r);
                }
            }));
        }

        for h in handles {
            let _ = h.join();
        }

        let res_list = results.lock().unwrap().clone();
        assert!(!res_list.is_empty(), "at least one start should succeed");

        let info = super::super::pid::get_daemon_info(Some(&pid_file));
        assert!(info.is_some());
        let pid = info.unwrap().pid;
        let mut kill_guard = crate::daemon::lifecycle::ProcessKillGuard::new(pid);

        let _ = stop_daemon(Some(&pid_file), port, Duration::from_secs(3));
        kill_guard.disarm();
    }

    static SYSTEMCTL_RECORDER_CALLS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    fn test_record_systemctl(scope: &str, verb: &str) {
        let mut calls = SYSTEMCTL_RECORDER_CALLS.lock().unwrap_or_else(|e| e.into_inner());
        calls.push((scope.to_string(), verb.to_string()));
    }

    fn test_scope_override(_pid: i32) -> &'static str {
        "--user"
    }

    #[test]
    fn rust_restart_uses_systemctl_when_unit_active() {
        use std::os::unix::process::CommandExt;

        let env = crate::daemon::test_env();

        struct ResetHooks;
        impl Drop for ResetHooks {
            fn drop(&mut self) {
                crate::daemon::systemd::set_systemd_scope_override(None);
                crate::daemon::systemd::set_systemctl_recorder(None);
            }
        }
        let _reset_guard = ResetHooks;

        SYSTEMCTL_RECORDER_CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        crate::daemon::systemd::set_systemd_scope_override(Some(test_scope_override));
        crate::daemon::systemd::set_systemctl_recorder(Some(test_record_systemctl));

        // Spawns sleep 30 with argv0 as agy-pool so is_agy_pool_process identifies it
        let child = std::process::Command::new("sleep")
            .arg0("agy-pool")
            .arg("30")
            .spawn()
            .expect("spawn sleep 30");
        let child_pid = child.id() as i32;

        struct ChildGuard(Option<std::process::Child>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(mut c) = self.0.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        }
        let _child_guard = ChildGuard(Some(child));

        let pid_file = env.temp_dir.path().join("systemd_test.pid");
        let log_file = env.temp_dir.path().join("systemd_test.log");
        let port = 19876;

        let ctx = super::super::instance::resolve_instance_context(Some(env.temp_dir.path()), None, Some(port), None)
            .unwrap();

        super::super::pid::write_pid_file(
            &pid_file,
            super::super::pid::DaemonInfo {
                pid: child_pid,
                data_dir: Some(env.temp_dir.path().to_str().unwrap().to_string()),
                config_path: ctx.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                port: Some(port),
                ..Default::default()
            },
        )
        .unwrap();

        // 1. Restart path:
        let opts = super::super::lifecycle::LaunchOptions {
            port,
            state_dir: Some(env.temp_dir.path().to_path_buf()),
            config_file: ctx.config_path.clone(),
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };

        // RestartInstance calls systemctl("--user", "restart") and then wait_restarted
        let restart_res = restart_instance(&ctx, opts);
        // wait_restarted will time out since recorder does not spawn a new daemon
        assert!(
            restart_res.is_err(),
            "expected restart to time out waiting for systemd unit"
        );

        {
            let calls = SYSTEMCTL_RECORDER_CALLS.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                *calls,
                vec![("--user".to_string(), "restart".to_string())],
                "expected recorder to see exactly ('--user', 'restart')"
            );
        }

        // Assert child is still alive and no new daemon took over
        assert!(super::super::pid::is_process_alive(child_pid));
        let info = super::super::pid::read_pid_file(&pid_file).unwrap();
        assert_eq!(info.pid, child_pid, "no new daemon process was spawned");

        // 2. Stop path:
        SYSTEMCTL_RECORDER_CALLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let stop_res = stop_with_file(&ctx, &pid_file, Duration::from_millis(50));
        assert!(
            stop_res.is_err(),
            "expected stop_with_file to time out as child was not killed"
        );

        {
            let calls = SYSTEMCTL_RECORDER_CALLS.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                *calls,
                vec![("--user".to_string(), "stop".to_string())],
                "expected recorder to see exactly ('--user', 'stop')"
            );
        }

        assert!(super::super::pid::is_process_alive(child_pid));
    }

    #[test]
    fn test_restart_missing_interface_fails_closed_before_stopping_running_daemon() {
        let env = crate::daemon::test_env();
        let data_dir = env.temp_dir.path().join("restart-missing-iface");
        std::fs::create_dir_all(&data_dir).unwrap();
        let pid_file = data_dir.join("agy-pool.pid");
        let log_file = data_dir.join("agy-pool.log");

        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);

        let cfg_file = env.temp_dir.path().join("restart-missing-iface.json");
        std::fs::write(
            &cfg_file,
            format!(r#"{{"version": 1, "server": {{"listen": "127.0.0.1", "port": {port}}}}}"#),
        )
        .unwrap();

        let opts = super::super::lifecycle::LaunchOptions {
            port,
            state_dir: Some(data_dir.clone()),
            config_file: Some(cfg_file.clone()),
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };
        let res = super::super::lifecycle::start_daemon(opts.clone()).unwrap();
        let mut kill_guard = crate::daemon::lifecycle::ProcessKillGuard::new(res.pid);

        // Update config in place to a missing interface and invoke restart_instance.
        std::fs::write(
            &cfg_file,
            format!(r#"{{"version": 1, "server": {{"listen": ["lo", "agy_no_such0"], "port": {port}}}}}"#),
        )
        .unwrap();
        let bad_ctx =
            super::super::instance::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), None)
                .unwrap();

        let err = restart_instance(&bad_ctx, opts).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected missing interface error, got: {err}"
        );
        assert!(
            super::super::pid::is_process_alive(res.pid),
            "existing daemon must remain alive when new listen interface resolution fails"
        );
        assert!(
            super::super::server::is_daemon_running(Some(&pid_file), port),
            "existing daemon must stay serving cleanly"
        );

        let _ = stop_daemon(Some(&pid_file), port, Duration::from_secs(3));
        kill_guard.disarm();
    }
}
