use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
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

#[test]
fn second_rust_gateway_refused_while_go_runs() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let go_bin = tmp_dir.path().join("agy-pool-go");

    // Build Go binary
    let go_build = std::process::Command::new("go")
        .args(["build", "-o", go_bin.to_str().unwrap(), "./cmd/agy-pool"])
        .current_dir(repo_root)
        .output()
        .expect("execute go build");
    assert!(
        go_build.status.success(),
        "go build failed: {}",
        String::from_utf8_lossy(&go_build.stderr)
    );

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

    let pid_file = data_dir.join("agy-pool.pid");
    let log_file = data_dir.join("agy-pool.log");

    // ==========================================
    // Scenario 1: Go holds the lock, Rust is refused
    // ==========================================
    let mut go_child = std::process::Command::new(&go_bin);
    let log_out = OpenOptions::new().create(true).append(true).open(&log_file).unwrap();
    let log_err = log_out.try_clone().unwrap();
    go_child.stdout(log_out);
    go_child.stderr(log_err);
    go_child.env("AGY_POOL_DAEMON_CHILD", "1");
    go_child.env("AGY_DATA_DIR", &data_dir);
    go_child.env("AGY_CONFIG_FILE", &cfg_file);
    go_child.env("AGY_GEMINI_DIR", &gemini_dir);
    unsafe {
        use std::os::unix::process::CommandExt;
        go_child.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut go_proc = go_child.spawn().expect("spawn Go gateway");
    let go_spawned_pid = go_proc.id() as i32;
    let mut go_kill_guard = ProcessGuard::new(go_spawned_pid);

    // Wait for Go gateway to write PID file and respond on /healthz
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut go_pid = go_spawned_pid;
    while Instant::now() < deadline {
        if pid_file.exists()
            && agy_pool::daemon::is_port_listening(port)
            && let Ok(info) = agy_pool::daemon::read_pid_file(&pid_file)
        {
            go_pid = info.pid;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut go_real_kill = ProcessGuard::new(go_pid);
    assert!(
        wait_for_healthz(port, Duration::from_secs(3)),
        "Go gateway failed to answer /healthz"
    );

    // Start Rust binary on SAME data dir -> must exit non-zero with "daemon is already running"
    let rust_out = std::process::Command::new(rust_bin)
        .env("AGY_POOL_DAEMON_CHILD", "1")
        .env("AGY_DATA_DIR", &data_dir)
        .env("AGY_CONFIG_FILE", &cfg_file)
        .env("AGY_GEMINI_DIR", &gemini_dir)
        .output()
        .expect("execute rust binary");

    assert!(
        !rust_out.status.success(),
        "Rust gateway should have been refused while Go runs"
    );
    let rust_stderr = String::from_utf8_lossy(&rust_out.stderr);
    assert!(
        rust_stderr.contains("daemon is already running"),
        "expected lock-held message, got: {rust_stderr}"
    );

    // Go gateway must still be healthy
    assert!(
        wait_for_healthz(port, Duration::from_secs(2)),
        "Go gateway should still answer /healthz after Rust refusal"
    );

    // Stop Go gateway
    unsafe {
        libc::kill(go_pid, libc::SIGTERM);
    }
    let _ = go_proc.wait();
    go_kill_guard.disarm();
    go_real_kill.disarm();

    let stop_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < stop_deadline {
        if !agy_pool::daemon::is_port_listening(port) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&pid_file);

    // ==========================================
    // Scenario 2: Rust holds the lock, Go is refused
    // ==========================================
    let mut rust_child = std::process::Command::new(rust_bin);
    let log_out2 = OpenOptions::new().create(true).append(true).open(&log_file).unwrap();
    let log_err2 = log_out2.try_clone().unwrap();
    rust_child.stdout(log_out2);
    rust_child.stderr(log_err2);
    rust_child.env("AGY_POOL_DAEMON_CHILD", "1");
    rust_child.env("AGY_DATA_DIR", &data_dir);
    rust_child.env("AGY_CONFIG_FILE", &cfg_file);
    rust_child.env("AGY_GEMINI_DIR", &gemini_dir);
    unsafe {
        use std::os::unix::process::CommandExt;
        rust_child.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut rust_proc = rust_child.spawn().expect("spawn Rust gateway");
    let rust_spawned_pid = rust_proc.id() as i32;
    let mut rust_kill_guard = ProcessGuard::new(rust_spawned_pid);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rust_pid = rust_spawned_pid;
    while Instant::now() < deadline {
        if pid_file.exists()
            && agy_pool::daemon::is_port_listening(port)
            && let Ok(info) = agy_pool::daemon::read_pid_file(&pid_file)
        {
            rust_pid = info.pid;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut rust_real_kill = ProcessGuard::new(rust_pid);
    assert!(
        wait_for_healthz(port, Duration::from_secs(3)),
        "Rust gateway failed to answer /healthz"
    );

    // Start Go binary on SAME data dir -> must exit non-zero with "daemon is already running"
    let go_out = std::process::Command::new(&go_bin)
        .env("AGY_POOL_DAEMON_CHILD", "1")
        .env("AGY_DATA_DIR", &data_dir)
        .env("AGY_CONFIG_FILE", &cfg_file)
        .env("AGY_GEMINI_DIR", &gemini_dir)
        .output()
        .expect("execute go binary");

    assert!(
        !go_out.status.success(),
        "Go gateway should have been refused while Rust runs"
    );
    let go_stderr = String::from_utf8_lossy(&go_out.stderr);
    assert!(
        go_stderr.contains("daemon is already running"),
        "expected lock-held message, got: {go_stderr}"
    );

    // Rust gateway must still be healthy
    assert!(
        wait_for_healthz(port, Duration::from_secs(2)),
        "Rust gateway should still answer /healthz after Go refusal"
    );

    // Stop Rust gateway
    unsafe {
        libc::kill(rust_pid, libc::SIGTERM);
    }
    let _ = rust_proc.wait();
    rust_kill_guard.disarm();
    rust_real_kill.disarm();
}
