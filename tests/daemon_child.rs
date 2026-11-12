use std::path::PathBuf;
use std::time::{Duration, Instant};

struct ProcessGuard {
    pid: i32,
    active: bool,
}

impl ProcessGuard {
    fn new(pid: i32) -> Self {
        Self { pid, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if self.active && self.pid > 0 {
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
            }
        }
    }
}

fn get_ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn wait_for_healthz(port: u16, timeout: Duration) -> bool {
    let client = reqwest_or_hyper_get(port);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(true) = client() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn reqwest_or_hyper_get(port: u16) -> impl Fn() -> Result<bool, ()> {
    move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).map_err(|_| ())?;
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|_| ())?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .map_err(|_| ())?;
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
        let resp_str = String::from_utf8_lossy(&resp);
        if resp_str.starts_with("HTTP/1.1 200") && resp_str.contains("{\"status\":\"ok\"}") {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[test]
fn test_daemon_child_production_lifecycle() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp_dir.path().join("data");
    let config_dir = tmp_dir.path().join("config");
    let gemini_dir = tmp_dir.path().join("gemini");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&gemini_dir).unwrap();

    let port = get_ephemeral_port();
    let cfg_file = config_dir.join("config.json");
    let config_json = format!(
        r#"{{
  "version": 1,
  "server": {{
    "listen": "127.0.0.1",
    "port": {port}
  }},
  "scheduler": {{
    "strategy": "max_quota"
  }}
}}"#
    );
    std::fs::write(&cfg_file, config_json.as_bytes()).unwrap();

    let ctx =
        agy_pool::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), Some("127.0.0.1"))
            .expect("resolve_instance_context");

    let opts = agy_pool::daemon::LaunchOptions {
        entrypoint_path: Some(PathBuf::from(rust_bin)),
        ..Default::default()
    };

    // A. Start launches real background child
    let res = agy_pool::daemon::start_instance(&ctx, opts.clone()).expect("start_instance");
    let mut kill_guard = ProcessGuard::new(res.pid);

    assert!(res.pid > 0);
    assert!(!res.already_run);
    assert!(agy_pool::daemon::is_process_alive(res.pid));

    // C. Readiness handshake succeeds and /healthz answers
    assert!(agy_pool::daemon::is_port_listening(port));
    assert!(wait_for_healthz(port, Duration::from_secs(3)));

    let pid_info = agy_pool::daemon::get_daemon_info(Some(&ctx.pid_file())).expect("pid info");
    assert_eq!(pid_info.pid, res.pid);

    // B. Child cmdline does not contain daemon-run
    if let Ok(cmd_bytes) = std::fs::read(format!("/proc/{}/cmdline", res.pid)) {
        let cmdline = String::from_utf8_lossy(&cmd_bytes).replace('\0', " ");
        assert!(
            !cmdline.contains("daemon-run"),
            "cmdline must not contain daemon-run: {cmdline}"
        );
    }

    // H & I. Check child environment in /proc/<pid>/environ
    if let Ok(env_bytes) = std::fs::read(format!("/proc/{}/environ", res.pid)) {
        let env_str = String::from_utf8_lossy(&env_bytes);
        assert!(env_str.contains("AGY_POOL_DAEMON_CHILD=1"));
        assert!(env_str.contains(&format!("AGY_DATA_DIR={}", ctx.data_dir.display())));
        assert!(env_str.contains(&format!(
            "AGY_CONFIG_FILE={}",
            ctx.config_path.as_ref().unwrap().display()
        )));
    }

    // D. status identifies same instance
    let (status, info, _) = agy_pool::daemon::check_instance_status(&ctx);
    assert_eq!(status, agy_pool::daemon::InstanceStatus::RunningSameInstance);
    assert_eq!(info.unwrap().pid, res.pid);

    // F. restart works
    let res_restart = agy_pool::daemon::restart_instance(
        &ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    )
    .expect("restart_instance");
    let mut kill_guard2 = ProcessGuard::new(res_restart.pid);
    kill_guard.disarm();

    assert!(res_restart.was_restarted);
    assert_ne!(res_restart.pid, res.pid);
    assert!(agy_pool::daemon::is_process_alive(res_restart.pid));
    assert!(!agy_pool::daemon::is_process_alive(res.pid));
    assert!(agy_pool::daemon::is_port_listening(port));

    // E. stop stops only same instance
    agy_pool::daemon::stop_instance(&ctx, Duration::from_secs(5)).expect("stop_instance");
    kill_guard2.disarm();

    assert!(!agy_pool::daemon::is_port_listening(port));
    assert!(!agy_pool::daemon::is_process_alive(res_restart.pid));
    assert!(!ctx.pid_file().exists());

    // G. Foreign port remains fail-closed
    let foreign_listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind foreign listener");
    let start_fail = agy_pool::daemon::start_instance(&ctx, opts);
    assert!(start_fail.is_err());
    assert!(matches!(
        start_fail,
        Err(agy_pool::daemon::DaemonError::ForeignPortOccupied(_))
    ));
    assert!(agy_pool::daemon::is_port_listening(port));
    drop(foreign_listener);
}

#[cfg(target_os = "linux")]
#[test]
fn test_daemon_child_listen_interface_and_fail_closed() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");
    let tmp_dir = tempfile::tempdir().expect("tempdir");

    // Pick a port free across all addresses resolved from `lo`.
    let mut port = 0;
    for _ in 0..20 {
        let p = get_ephemeral_port();
        if p == 8890 || p == 8899 || p == 8901 {
            continue;
        }
        if let Ok(addrs) = agy_pool::daemon::resolve_listen_targets(&["lo".to_string()], p)
            && !addrs.is_empty()
            && addrs.iter().all(|sa| std::net::TcpListener::bind(sa).is_ok())
        {
            port = p;
            break;
        }
    }
    assert_ne!(port, 0, "could not find free port across lo addresses");

    let data_dir = tmp_dir.path().join("lo-data");
    let config_dir = tmp_dir.path().join("lo-config");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();

    let cfg_file = config_dir.join("config.json");
    std::fs::write(
        &cfg_file,
        format!(r#"{{"version": 1, "server": {{"listen": ["lo"], "port": {port}}}}}"#),
    )
    .unwrap();

    let ctx = agy_pool::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), None)
        .expect("resolve lo context");
    assert_eq!(ctx.listen_targets, vec!["lo".to_string()]);

    let res = agy_pool::daemon::start_instance(
        &ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    )
    .expect("start_instance on lo");
    let mut kill_guard = ProcessGuard::new(res.pid);

    assert!(wait_for_healthz(port, Duration::from_secs(3)));
    let info = agy_pool::daemon::get_daemon_info(Some(&ctx.pid_file())).expect("pid info");
    assert!(
        info.listen_addrs.contains(&format!("127.0.0.1:{port}")),
        "PID info should record bound 127.0.0.1:{port}, got {:?}",
        info.listen_addrs
    );
    let expected_addrs = agy_pool::daemon::resolve_context_socket_addrs(&ctx).unwrap();
    assert_eq!(
        info.listen_addrs.len(),
        expected_addrs.len(),
        "PID info should record every resolved lo socket address"
    );

    agy_pool::daemon::stop_instance(&ctx, Duration::from_secs(5)).expect("stop_instance on lo");
    kill_guard.disarm();
    assert!(!agy_pool::daemon::is_port_listening(port));

    // Missing interface must fail closed without spawning or creating a PID file.
    let bad_data = tmp_dir.path().join("bad-iface-data");
    std::fs::create_dir_all(&bad_data).unwrap();
    let bad_cfg = config_dir.join("bad-iface.json");
    std::fs::write(
        &bad_cfg,
        format!(r#"{{"version": 1, "server": {{"listen": ["agy_no_such0"], "port": {port}}}}}"#),
    )
    .unwrap();
    let bad_ctx = agy_pool::daemon::resolve_instance_context(Some(&bad_data), Some(&bad_cfg), Some(port), None)
        .expect("resolve syntax-valid missing interface context");
    let err = agy_pool::daemon::start_instance(
        &bad_ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("not found"), "unexpected error: {err}");
    assert!(!bad_ctx.pid_file().exists());
}

#[test]
fn test_daemon_child_applies_config_model_mapping() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let port = get_ephemeral_port();
    let cfg_file = tmp_dir.path().join("config.json");
    std::fs::write(
        &cfg_file,
        format!(
            r#"{{"version": 1, "server": {{"listen": "127.0.0.1", "port": {port}}}, "model_mapping": {{"agy-old-model": "agy-new-model"}}}}"#
        ),
    )
    .unwrap();

    let ctx = agy_pool::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), None)
        .expect("resolve_instance_context");
    let res = agy_pool::daemon::start_instance(
        &ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    )
    .expect("start_instance");
    let mut kill_guard = ProcessGuard::new(res.pid);
    assert!(wait_for_healthz(port, Duration::from_secs(3)));

    // The daemon logs the remap before picking an account, so no upstream is needed.
    {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = "{}";
        write!(
            s,
            "POST /v1beta/models/agy-old-model:generateContent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut log = String::new();
    while Instant::now() < deadline {
        log = std::fs::read_to_string(ctx.log_file()).unwrap_or_default();
        if log.contains("agy-old-model -> agy-new-model") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    agy_pool::daemon::stop_instance(&ctx, Duration::from_secs(5)).expect("stop_instance");
    kill_guard.disarm();
    assert!(
        log.contains("agy-old-model -> agy-new-model"),
        "daemon must apply model_mapping from its config file; log:\n{log}"
    );
}

#[test]
fn test_daemon_child_reports_invalid_config() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let port = get_ephemeral_port();
    let cfg_file = tmp_dir.path().join("config.json");
    // Typo'd key: deny_unknown_fields rejects the whole file, which used to be dropped silently.
    std::fs::write(
        &cfg_file,
        r#"{"version": 1, "server": {"keep_alive_sec": 10}, "model_mapping": {"a": "b"}}"#,
    )
    .unwrap();

    let ctx = agy_pool::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), None)
        .expect("resolve_instance_context");
    let res = agy_pool::daemon::start_instance(
        &ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    )
    .expect("start_instance");
    let mut kill_guard = ProcessGuard::new(res.pid);
    assert!(wait_for_healthz(port, Duration::from_secs(3)));
    let log = std::fs::read_to_string(ctx.log_file()).unwrap_or_default();
    agy_pool::daemon::stop_instance(&ctx, Duration::from_secs(5)).expect("stop_instance");
    kill_guard.disarm();
    assert!(
        log.contains("ignoring invalid config"),
        "daemon must report a rejected config file; log:\n{log}"
    );
}
