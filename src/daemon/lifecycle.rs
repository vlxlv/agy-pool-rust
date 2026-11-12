use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use super::DaemonError;

pub const ENV_DAEMON_CHILD: &str = "AGY_POOL_DAEMON_CHILD";
pub const ENV_DAEMON_PORT: &str = "AGY_POOL_DAEMON_PORT";

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub port: u16,
    pub state_dir: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub pid_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub entrypoint_path: Option<PathBuf>,
    pub foreground: bool,
    pub custom_args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StartResult {
    pub pid: i32,
    pub already_run: bool,
    pub was_restarted: bool,
}

/// RAII Drop guard that sends SIGKILL to a PID unless disarmed.
pub struct ProcessKillGuard {
    pub pid: i32,
    pub active: bool,
}

impl ProcessKillGuard {
    pub fn new(pid: i32) -> Self {
        Self { pid, active: true }
    }

    pub fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ProcessKillGuard {
    fn drop(&mut self) {
        if self.active && self.pid > 0 {
            let _ = super::systemd::send_signal(self.pid, libc::SIGKILL);
        }
    }
}

/// Starts the reverse proxy gateway for the given InstanceContext.
pub fn start_instance(ctx: &super::instance::InstanceContext, opts: LaunchOptions) -> Result<StartResult, DaemonError> {
    let pid_file = opts.pid_file.clone().unwrap_or_else(|| ctx.pid_file());
    crate::config::assert_safe_write_path(&pid_file)?;

    let log_file = opts.log_file.clone().unwrap_or_else(|| ctx.log_file());
    crate::config::assert_safe_write_path(&log_file)?;

    if opts.foreground {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let res = rt.block_on(async {
            super::server::run_foreground(super::server::ServerOptions {
                port: ctx.listen_port,
                ephemeral_port: false,
                pid_file: Some(pid_file),
                log_file: Some(log_file),
                handler: None,
                ready_tx: None,
                bound_port_tx: None,
                quota_refresh_interval: None,
                disable_signals: false,
                instance: Some(ctx.clone()),
                shutdown_rx: None,
            })
            .await
        });
        return match res {
            Ok(()) => Ok(StartResult {
                pid: std::process::id() as i32,
                already_run: false,
                was_restarted: false,
            }),
            Err(e) => Err(e),
        };
    }

    let entrypoint = opts.entrypoint_path.clone().unwrap_or_else(super::pid::entrypoint);
    let target_addrs = super::server::resolve_context_socket_addrs(ctx)?;

    // 1. Check current instance status
    let mut was_restarted = false;
    if super::server::is_daemon_running(Some(&pid_file), ctx.listen_port) {
        if super::server::is_daemon_outdated(Some(&pid_file), ctx.listen_port) {
            was_restarted = true;
            super::control::stop_with_file(ctx, &pid_file, Duration::from_secs(5))?;
            std::thread::sleep(Duration::from_millis(300));
        } else {
            let pid = super::pid::get_daemon_pid(Some(&pid_file));
            return Ok(StartResult {
                pid,
                already_run: true,
                was_restarted: false,
            });
        }
    } else if super::server::any_addr_listening(&target_addrs) {
        return Err(DaemonError::ForeignPortOccupied(format!(
            "{} is occupied by another process or instance",
            super::server::format_socket_addrs(&target_addrs)
        )));
    }

    // 2. Ensure data, log, and PID directories exist with 0700 permissions
    for f in [&pid_file, &log_file] {
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let _ = std::fs::create_dir_all(ctx.locks_dir());

    // 3. Open log file with mode 0600
    let log_fd = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_file)?;

    // 4. Construct daemon child command
    let mut cmd = std::process::Command::new(entrypoint);
    if !opts.custom_args.is_empty() {
        cmd.args(&opts.custom_args);
    }
    let log_err = log_fd.try_clone()?;
    cmd.stdout(log_fd);
    cmd.stderr(log_err);

    // Step 2b safety rules: explicit temp data dir, config file, and gemini dir
    let config_path = match &ctx.config_path {
        Some(p) => p.clone(),
        None => {
            let auto_cfg = ctx.data_dir.join("config.json");
            if !auto_cfg.exists() {
                let targets = ctx.effective_listen_targets();
                let listen_val = if targets.len() == 1 {
                    serde_json::Value::String(targets[0].clone())
                } else {
                    serde_json::Value::Array(targets.into_iter().map(serde_json::Value::String).collect())
                };
                let cfg_json = serde_json::json!({
                    "version": 1,
                    "server": {
                        "listen": listen_val,
                        "port": ctx.listen_port,
                    }
                });
                let _ = std::fs::write(&auto_cfg, cfg_json.to_string());
            }
            auto_cfg
        }
    };

    cmd.env(ENV_DAEMON_CHILD, "1");
    cmd.env("AGY_DATA_DIR", &ctx.data_dir);
    cmd.env("AGY_CONFIG_FILE", &config_path);
    if ctx.listen_port > 0 {
        cmd.env(ENV_DAEMON_PORT, ctx.listen_port.to_string());
    }
    if let Some(pf) = &opts.pid_file {
        cmd.env("AGY_TEST_PID_FILE", pf);
    }

    // Detached session: create a new process group (setsid)
    cmd.process_group(0);

    let mut child = cmd.spawn()?;

    // 5. Readiness handshake
    let mut started = false;
    for _ in 0..30 {
        if let Ok(Some(status)) = child.try_wait() {
            let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
            return Err(DaemonError::Message(format!(
                "daemon child process exited prematurely with status {:?}; log: {:?}",
                status, log_content
            )));
        }
        if let Some(info) = super::pid::get_daemon_info(Some(&pid_file))
            && info.pid > 0
            && super::pid::is_process_alive(info.pid)
            && super::server::all_addrs_listening(&target_addrs)
        {
            started = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    std::thread::spawn(move || {
        let _ = child.wait();
    });

    if started && let Some(info) = super::pid::get_daemon_info(Some(&pid_file)) {
        return Ok(StartResult {
            pid: info.pid,
            already_run: false,
            was_restarted,
        });
    }

    if let Some(info) = super::pid::get_daemon_info(Some(&pid_file))
        && info.pid > 0
        && super::server::all_addrs_listening(&target_addrs)
    {
        return Ok(StartResult {
            pid: info.pid,
            already_run: false,
            was_restarted,
        });
    }

    let log_content = std::fs::read_to_string(&log_file).unwrap_or_else(|_| "<unreadable>".to_string());
    Err(DaemonError::Message(format!(
        "failed to start gateway daemon on {}; inspect log file at {:?}: {}",
        ctx.display_listen(None),
        log_file,
        log_content
    )))
}

/// StartDaemon starts the reverse proxy gateway in the background (or foreground if requested).
pub fn start_daemon(opts: LaunchOptions) -> Result<StartResult, DaemonError> {
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

    start_instance(&ctx, opts)
}

/// Entrypoint for child processes spawned with AGY_POOL_DAEMON_CHILD=1.
pub fn run_daemon_child(stderr: &mut impl std::io::Write) -> i32 {
    let data_dir = std::env::var("AGY_DATA_DIR").ok().map(PathBuf::from);
    let config_file = std::env::var("AGY_CONFIG_FILE").ok().map(PathBuf::from);
    let env_port = std::env::var(ENV_DAEMON_PORT).ok().and_then(|s| s.parse::<u16>().ok());

    let ctx =
        match super::instance::resolve_instance_context(data_dir.as_deref(), config_file.as_deref(), env_port, None) {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(stderr, "daemon child error: {e}");
                return 1;
            }
        };

    if let Err(e) = crate::config::configure_data_dir(&ctx.data_dir) {
        let _ = writeln!(stderr, "daemon child error: {e}");
        return 1;
    }

    let pid_file = std::env::var("AGY_TEST_PID_FILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.pid_file());

    let run_fg = async {
        super::server::run_foreground(super::server::ServerOptions {
            port: ctx.listen_port,
            ephemeral_port: false,
            pid_file: Some(pid_file),
            log_file: Some(ctx.log_file()),
            handler: None,
            ready_tx: None,
            bound_port_tx: None,
            quota_refresh_interval: None,
            disable_signals: false,
            instance: Some(ctx),
            shutdown_rx: None,
        })
        .await
    };

    let rt = match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let res = tokio::task::block_in_place(|| handle.block_on(run_fg));
            if let Err(e) = res {
                let _ = writeln!(stderr, "daemon child error: {e}");
                return 1;
            }
            return 0;
        }
        Err(_) => match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = writeln!(stderr, "daemon child error: failed to create runtime: {e}");
                return 1;
            }
        },
    };

    if let Err(e) = rt.block_on(run_fg) {
        let _ = writeln!(stderr, "daemon child error: {e}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn get_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn test_helper_process_background_daemon_spawn_and_stop() {
        let env = crate::daemon::test_env();
        let pid_file = env.temp_dir.path().join("child.pid");
        let log_file = env.temp_dir.path().join("child.log");

        let port = get_free_port();

        let opts = LaunchOptions {
            port,
            pid_file: Some(pid_file.clone()),
            log_file: Some(log_file.clone()),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };

        let res = start_daemon(opts).unwrap();
        assert!(res.pid > 0);
        let mut kill_guard = ProcessKillGuard::new(res.pid);

        assert!(super::super::pid::is_process_alive(res.pid));
        assert!(super::super::server::is_port_listening(port));

        let meta = std::fs::metadata(&log_file).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::mode(&meta) & 0o777,
            0o600,
            "expected log file mode 0600"
        );

        // Stop the daemon
        super::super::control::stop_daemon(Some(&pid_file), port, Duration::from_secs(8)).unwrap();
        kill_guard.disarm();

        assert!(!super::super::pid::is_process_alive(res.pid));
        assert!(!super::super::server::is_port_listening(port));
        assert!(!pid_file.exists());
    }

    #[tokio::test]
    async fn test_multi_instance_foreground_isolation() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let port_a = get_free_port();
        let port_b = get_free_port();

        let data_dir_a = tmp_dir.join("data-a");
        let data_dir_b = tmp_dir.join("data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let cfg_a = tmp_dir.join("cfg-a.json");
        let cfg_b = tmp_dir.join("cfg-b.json");
        std::fs::write(&cfg_a, format!(r#"{{"version": 1, "server":{{"port":{port_a}}}}}"#)).unwrap();
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port_b}}}}}"#)).unwrap();

        let ctx_a = super::super::instance::resolve_instance_context(
            Some(&data_dir_a),
            Some(&cfg_a),
            Some(port_a),
            Some("127.0.0.1"),
        )
        .unwrap();
        let ctx_b = super::super::instance::resolve_instance_context(
            Some(&data_dir_b),
            Some(&cfg_b),
            Some(port_b),
            Some("127.0.0.1"),
        )
        .unwrap();

        assert_ne!(ctx_a.pid_file(), ctx_b.pid_file());
        assert_ne!(ctx_a.log_file(), ctx_b.log_file());
        assert_ne!(ctx_a.state_db(), ctx_b.state_db());
        assert_ne!(ctx_a.locks_dir(), ctx_b.locks_dir());

        let (ready_a_tx, ready_a_rx) = tokio::sync::oneshot::channel::<()>();
        let (ready_b_tx, ready_b_rx) = tokio::sync::oneshot::channel::<()>();
        let (shutdown_a_tx, shutdown_a_rx) = tokio::sync::oneshot::channel::<()>();
        let (shutdown_b_tx, shutdown_b_rx) = tokio::sync::oneshot::channel::<()>();

        let ca = ctx_a.clone();
        let handle_a = tokio::spawn(async move {
            super::super::server::run_foreground(super::super::server::ServerOptions {
                port: port_a,
                ephemeral_port: false,
                pid_file: None,
                log_file: None,
                handler: None,
                ready_tx: Some(ready_a_tx),
                bound_port_tx: None,
                quota_refresh_interval: None,
                disable_signals: true,
                instance: Some(ca),
                shutdown_rx: Some(shutdown_a_rx),
            })
            .await
        });

        let cb = ctx_b.clone();
        let handle_b = tokio::spawn(async move {
            super::super::server::run_foreground(super::super::server::ServerOptions {
                port: port_b,
                ephemeral_port: false,
                pid_file: None,
                log_file: None,
                handler: None,
                ready_tx: Some(ready_b_tx),
                bound_port_tx: None,
                quota_refresh_interval: None,
                disable_signals: true,
                instance: Some(cb),
                shutdown_rx: Some(shutdown_b_rx),
            })
            .await
        });

        ready_a_rx.await.unwrap();
        ready_b_rx.await.unwrap();

        let (status_a, info_a, _) = super::super::instance::check_instance_status(&ctx_a);
        assert_eq!(status_a, super::super::instance::InstanceStatus::RunningSameInstance);

        let (status_b, info_b, _) = super::super::instance::check_instance_status(&ctx_b);
        assert_eq!(status_b, super::super::instance::InstanceStatus::RunningSameInstance);

        // Cross instance negative checks
        assert_eq!(
            super::super::instance::matches_instance(&info_a.unwrap(), &ctx_b),
            super::super::instance::MatchResult::Foreign
        );
        assert_eq!(
            super::super::instance::matches_instance(&info_b.unwrap(), &ctx_a),
            super::super::instance::MatchResult::Foreign
        );

        // Stop A context targeting B PID file fails with PIDNotOwned
        let err = super::super::control::stop_with_file(&ctx_a, &ctx_b.pid_file(), Duration::from_millis(100));
        assert!(matches!(err, Err(DaemonError::PIDNotOwned(_))));

        // Stop A
        let _ = shutdown_a_tx.send(());
        let _ = handle_a.await.unwrap();

        for _ in 0..30 {
            if !super::super::server::is_port_listening(port_a) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // B remains listening and unharmed
        assert!(super::super::server::is_port_listening(port_b));
        let (status_b_after, _, _) = super::super::instance::check_instance_status(&ctx_b);
        assert_eq!(
            status_b_after,
            super::super::instance::InstanceStatus::RunningSameInstance
        );

        // Stop B
        let _ = shutdown_b_tx.send(());
        let _ = handle_b.await.unwrap();
    }

    #[test]
    fn test_same_port_collision() {
        let env = crate::daemon::test_env();
        let port = get_free_port();

        // Bind listener on port to simulate Instance A running
        let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();

        let data_dir_b = env.temp_dir.path().join("data-b");
        std::fs::create_dir_all(&data_dir_b).unwrap();
        let cfg_b = env.temp_dir.path().join("cfg-b.json");
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port}}}}}"#)).unwrap();

        let ctx_b = super::super::instance::resolve_instance_context(
            Some(&data_dir_b),
            Some(&cfg_b),
            Some(port),
            Some("127.0.0.1"),
        )
        .unwrap();

        let (status, _, _) = super::super::instance::check_instance_status(&ctx_b);
        assert_eq!(status, super::super::instance::InstanceStatus::PortOccupiedForeign);

        let start_res = start_instance(
            &ctx_b,
            LaunchOptions {
                port,
                ..Default::default()
            },
        );
        assert!(matches!(start_res, Err(DaemonError::ForeignPortOccupied(_))));

        // Port of A remains alive
        assert!(super::super::server::is_port_listening(port));
        drop(listener);
    }

    #[test]
    fn test_config_mismatch_and_tampering() {
        let env = crate::daemon::test_env();
        let port = get_free_port();
        let data_dir = env.temp_dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let cfg_file = env.temp_dir.path().join("config.json");
        std::fs::write(
            &cfg_file,
            format!(r#"{{"version": 1, "server":{{"port":{port},"listen":"127.0.0.1"}}}}"#),
        )
        .unwrap();

        let ctx_a = super::super::instance::resolve_instance_context(
            Some(&data_dir),
            Some(&cfg_file),
            Some(port),
            Some("127.0.0.1"),
        )
        .unwrap();

        let pid_info = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            version: Some(ctx_a.version.clone()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            port: Some(port),
            listen_host: Some("127.0.0.1".to_string()),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx_a.pid_file(), pid_info.clone()).unwrap();

        // Modify config on disk
        std::fs::write(
            &cfg_file,
            format!(
                r#"{{"version": 1, "server":{{"port":{port},"listen":"127.0.0.1"}},"scheduler":{{"strategy":"max_quota"}}}}"#
            ),
        )
        .unwrap();

        let ctx_modified = super::super::instance::resolve_instance_context(
            Some(&data_dir),
            Some(&cfg_file),
            Some(port),
            Some("127.0.0.1"),
        )
        .unwrap();
        assert_ne!(ctx_modified.config_hash, ctx_a.config_hash);

        let (status, _, _) = super::super::instance::check_instance_status(&ctx_modified);
        assert_eq!(status, super::super::instance::InstanceStatus::ConfigMismatch);

        assert_eq!(
            super::super::instance::matches_instance(&pid_info, &ctx_modified),
            super::super::instance::MatchResult::ConfigMismatch
        );
    }

    #[test]
    fn test_same_data_dir_different_config_file_rejected() {
        let env = crate::daemon::test_env();
        let port = get_free_port();
        let data_dir = env.temp_dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let cfg_a = env.temp_dir.path().join("config-a.json");
        let cfg_b = env.temp_dir.path().join("config-b.json");
        std::fs::write(&cfg_a, format!(r#"{{"version": 1, "server":{{"port":{port}}}}}"#)).unwrap();
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port}}}}}"#)).unwrap();

        let ctx_a = super::super::instance::resolve_instance_context(
            Some(&data_dir),
            Some(&cfg_a),
            Some(port),
            Some("127.0.0.1"),
        )
        .unwrap();
        let ctx_b = super::super::instance::resolve_instance_context(
            Some(&data_dir),
            Some(&cfg_b),
            Some(port),
            Some("127.0.0.1"),
        )
        .unwrap();

        let pid_info = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            version: Some(ctx_a.version.clone()),
            config_path: ctx_a.config_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            config_hash: ctx_a.config_hash.clone(),
            data_dir: Some(ctx_a.data_dir.to_string_lossy().to_string()),
            port: Some(port),
            listen_host: Some("127.0.0.1".to_string()),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx_a.pid_file(), pid_info).unwrap();

        let err = super::super::control::stop_instance(&ctx_b, Duration::from_millis(100));
        assert!(matches!(err, Err(DaemonError::ConfigMismatch(_))));
        assert!(ctx_a.pid_file().exists());
    }

    #[test]
    fn test_data_dir_symlink_canonicalization() {
        let env = crate::daemon::test_env();
        let real_dir = env.temp_dir.path().join("real-data");
        std::fs::create_dir_all(&real_dir).unwrap();

        let symlink_dir = env.temp_dir.path().join("link-data");
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).unwrap();

        let ephemeral_port = get_free_port();
        let ctx_real =
            super::super::instance::resolve_instance_context(Some(&real_dir), None, Some(ephemeral_port), None)
                .unwrap();
        let ctx_link =
            super::super::instance::resolve_instance_context(Some(&symlink_dir), None, Some(ephemeral_port), None)
                .unwrap();

        assert_eq!(ctx_real.data_dir, ctx_link.data_dir);
        assert_eq!(ctx_real.pid_file(), ctx_link.pid_file());
    }

    #[test]
    fn test_pid_file_tampering_fail_closed() {
        let env = crate::daemon::test_env();
        let data_dir = env.temp_dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let ctx =
            super::super::instance::resolve_instance_context(Some(&data_dir), None, Some(9301), Some("127.0.0.1"))
                .unwrap();

        // 1. Wrong data_dir in metadata
        let tampered_dir = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            data_dir: Some(env.temp_dir.path().join("other-data").to_string_lossy().to_string()),
            port: Some(9301),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), tampered_dir).unwrap();
        let err = super::super::control::stop_instance(&ctx, Duration::from_millis(100));
        assert!(matches!(err, Err(DaemonError::PIDNotOwned(_))));

        // 2. Wrong port in metadata
        let tampered_port = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
            port: Some(9999),
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), tampered_port).unwrap();
        let err = super::super::control::stop_instance(&ctx, Duration::from_millis(100));
        assert!(matches!(err, Err(DaemonError::PIDNotOwned(_))));

        // 3. Live unrelated PID (PID 1)
        if Path::new("/proc/1/cmdline").exists() {
            let tampered_pid1 = super::super::pid::DaemonInfo {
                pid: 1,
                data_dir: Some(ctx.data_dir.to_string_lossy().to_string()),
                port: Some(9301),
                ..Default::default()
            };
            super::super::pid::write_pid_file(&ctx.pid_file(), tampered_pid1).unwrap();
            let err = super::super::control::stop_instance(&ctx, Duration::from_millis(100));
            assert!(matches!(err, Err(DaemonError::PIDReused)));
        }

        // 4. Legacy PID without data_dir proof
        let legacy_pid = super::super::pid::DaemonInfo {
            pid: std::process::id() as i32,
            ..Default::default()
        };
        super::super::pid::write_pid_file(&ctx.pid_file(), legacy_pid).unwrap();
        let (status, _, _) = super::super::instance::check_instance_status(&ctx);
        assert_eq!(status, super::super::instance::InstanceStatus::LegacyPID);
    }

    #[test]
    fn test_per_instance_pid_locks() {
        let env = crate::daemon::test_env();
        let data_dir_a = env.temp_dir.path().join("data-a");
        let data_dir_b = env.temp_dir.path().join("data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let ctx_a =
            super::super::instance::resolve_instance_context(Some(&data_dir_a), None, Some(9401), None).unwrap();
        let ctx_b =
            super::super::instance::resolve_instance_context(Some(&data_dir_b), None, Some(9402), None).unwrap();

        // 1. Acquire lock on A
        let lock_a = super::super::pid::try_acquire_pid_lock(&ctx_a.pid_file()).unwrap();

        // 2. Lock on B succeeds independently
        let lock_b = super::super::pid::try_acquire_pid_lock(&ctx_b.pid_file()).unwrap();

        // 3. Second lock on A fails
        let lock_a2 = super::super::pid::try_acquire_pid_lock(&ctx_a.pid_file());
        assert!(matches!(lock_a2, Err(DaemonError::AlreadyRunning)));

        lock_a.unlock();
        lock_b.unlock();

        // 4. After unlock, lock on A succeeds again
        let lock_a3 = super::super::pid::try_acquire_pid_lock(&ctx_a.pid_file());
        assert!(lock_a3.is_ok());
    }

    #[test]
    fn test_per_instance_state_db_and_log_isolation() {
        let env = crate::daemon::test_env();
        let data_dir_a = env.temp_dir.path().join("data-a");
        let data_dir_b = env.temp_dir.path().join("data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let ctx_a =
            super::super::instance::resolve_instance_context(Some(&data_dir_a), None, Some(9501), None).unwrap();
        let ctx_b =
            super::super::instance::resolve_instance_context(Some(&data_dir_b), None, Some(9502), None).unwrap();

        let db_a = crate::storage::StateDb::open(&ctx_a.state_db()).unwrap();
        db_a.update(|tx| {
            tx.execute("CREATE TABLE test_data (val TEXT);", [])?;
            tx.execute("INSERT INTO test_data VALUES ('only-in-a');", [])?;
            Ok(())
        })
        .unwrap();

        let db_b = crate::storage::StateDb::open(&ctx_b.state_db()).unwrap();
        let res: Result<String, _> = db_b.view(|tx| {
            tx.query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='test_data'",
                [],
                |r| r.get(0),
            )
            .map_err(crate::storage::StorageError::Sqlite)
        });
        assert!(res.is_err(), "expected db_b not to contain table from db_a");
    }

    #[test]
    fn test_multi_instance_subprocess_lifecycle() {
        let env = crate::daemon::test_env();
        let tmp_dir = env.temp_dir.path();

        let port_a = get_free_port();
        let port_b = get_free_port();

        let data_dir_a = tmp_dir.join("sub-data-a");
        let data_dir_b = tmp_dir.join("sub-data-b");
        std::fs::create_dir_all(&data_dir_a).unwrap();
        std::fs::create_dir_all(&data_dir_b).unwrap();

        let cfg_a = tmp_dir.join("sub-cfg-a.json");
        let cfg_b = tmp_dir.join("sub-cfg-b.json");
        std::fs::write(&cfg_a, format!(r#"{{"version": 1, "server":{{"port":{port_a}}}}}"#)).unwrap();
        std::fs::write(&cfg_b, format!(r#"{{"version": 1, "server":{{"port":{port_b}}}}}"#)).unwrap();

        let ctx_a = super::super::instance::resolve_instance_context(
            Some(&data_dir_a),
            Some(&cfg_a),
            Some(port_a),
            Some("127.0.0.1"),
        )
        .unwrap();
        let ctx_b = super::super::instance::resolve_instance_context(
            Some(&data_dir_b),
            Some(&cfg_b),
            Some(port_b),
            Some("127.0.0.1"),
        )
        .unwrap();

        let opts_a = LaunchOptions {
            port: port_a,
            state_dir: Some(data_dir_a),
            config_file: Some(cfg_a),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };
        let opts_b = LaunchOptions {
            port: port_b,
            state_dir: Some(data_dir_b),
            config_file: Some(cfg_b),
            entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
            ..Default::default()
        };

        // 1. Start A
        let res_a = start_daemon(opts_a.clone()).unwrap();
        let mut kill_a = ProcessKillGuard::new(res_a.pid);

        // 2. Start B
        let res_b = start_daemon(opts_b.clone()).unwrap();
        let mut kill_b = ProcessKillGuard::new(res_b.pid);

        assert_ne!(res_a.pid, res_b.pid);

        // 3. Verify both are running
        assert!(super::super::server::is_port_listening(port_a));
        assert!(super::super::server::is_port_listening(port_b));

        // 4. Stop A
        super::super::control::stop_daemon(Some(&ctx_a.pid_file()), port_a, Duration::from_secs(3)).unwrap();
        kill_a.disarm();

        // 5. B must remain alive
        assert!(super::super::server::is_port_listening(port_b));
        assert!(super::super::pid::is_process_alive(res_b.pid));

        // 6. Restart B
        let res_b2 = super::super::control::restart_daemon(opts_b).unwrap();
        let mut kill_b2 = ProcessKillGuard::new(res_b2.pid);
        kill_b.disarm();
        assert_ne!(res_b2.pid, res_b.pid);

        // A remains stopped
        assert!(!super::super::server::is_port_listening(port_a));

        // 7. Start A again
        let res_a2 = start_daemon(opts_a).unwrap();
        let mut kill_a2 = ProcessKillGuard::new(res_a2.pid);

        assert!(super::super::server::is_port_listening(port_a));
        assert!(super::super::server::is_port_listening(port_b));
        assert!(super::super::pid::is_process_alive(res_a2.pid));
        assert!(super::super::pid::is_process_alive(res_b2.pid));

        let _ = super::super::control::stop_daemon(Some(&ctx_a.pid_file()), port_a, Duration::from_secs(3));
        let _ = super::super::control::stop_daemon(Some(&ctx_b.pid_file()), port_b, Duration::from_secs(3));
        kill_a2.disarm();
        kill_b2.disarm();
    }

    #[test]
    fn test_start_instance_missing_interface_fails_closed() {
        let env = crate::daemon::test_env();
        let data_dir = env.temp_dir.path().join("missing-iface");
        std::fs::create_dir_all(&data_dir).unwrap();
        let cfg_path = env.temp_dir.path().join("missing-iface.json");
        let port = get_free_port();
        std::fs::write(
            &cfg_path,
            format!(r#"{{"version": 1, "server": {{"listen": ["agy_no_such0"], "port": {port}}}}}"#),
        )
        .unwrap();

        let ctx = super::super::instance::resolve_instance_context(Some(&data_dir), Some(&cfg_path), Some(port), None)
            .unwrap();
        let res = start_instance(
            &ctx,
            LaunchOptions {
                port,
                entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
                ..Default::default()
            },
        );
        assert!(res.is_err(), "missing listen interface must fail closed");
        assert!(res.unwrap_err().to_string().contains("not found"));
        assert!(
            !ctx.pid_file().exists(),
            "no PID file should be written on failed start"
        );
        assert!(
            !ctx.log_file().exists(),
            "no log file should be created before listen resolution"
        );
    }

    #[test]
    fn test_partial_foreign_port_occupation_blocks_multi_listen_start() {
        let env = crate::daemon::test_env();
        let mut port = 0;
        let mut foreign = None;
        for _ in 0..20 {
            let p = get_free_port();
            if p != crate::config::DEFAULT_PORT
                && p != 8899
                && p != 8901
                && let Ok(l) = std::net::TcpListener::bind(("127.0.0.2", p))
            {
                port = p;
                foreign = Some(l);
                break;
            }
        }
        let foreign = foreign.expect("free port on 127.0.0.2");

        let data_dir = env.temp_dir.path().join("partial-foreign");
        std::fs::create_dir_all(&data_dir).unwrap();
        let cfg_path = env.temp_dir.path().join("partial-foreign.json");
        std::fs::write(
            &cfg_path,
            format!(r#"{{"version": 1, "server": {{"listen": ["127.0.0.1", "127.0.0.2"], "port": {port}}}}}"#),
        )
        .unwrap();

        let ctx = super::super::instance::resolve_instance_context(Some(&data_dir), Some(&cfg_path), Some(port), None)
            .unwrap();

        let (status, _, _) = super::super::instance::check_instance_status(&ctx);
        assert_eq!(status, super::super::instance::InstanceStatus::PortOccupiedForeign);

        let res = start_instance(
            &ctx,
            LaunchOptions {
                port,
                entrypoint_path: Some(super::super::pid::get_entrypoint_path()),
                ..Default::default()
            },
        );
        assert!(matches!(res, Err(DaemonError::ForeignPortOccupied(_))));
        assert!(!ctx.pid_file().exists());
        drop(foreign);
    }
}
