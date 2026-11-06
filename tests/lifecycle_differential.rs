use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static GO_BIN: OnceLock<PathBuf> = OnceLock::new();

fn get_go_bin() -> &'static Path {
    GO_BIN.get_or_init(|| {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
        let tmp = tempfile::tempdir().expect("create tempdir for go binary");
        let bin_path = tmp.path().join("agy-pool-go");
        let out = Command::new("go")
            .args(["build", "-o", bin_path.to_str().unwrap(), "./cmd/agy-pool"])
            .current_dir(repo_root)
            .env("AGY_TEST_MODE", "1")
            .output()
            .expect("execute go build");
        assert!(
            out.status.success(),
            "go build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::mem::forget(tmp);
        bin_path
    })
}

fn get_rust_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_agy-pool"))
}

/// Ports already handed out in this test process; the port is released before a daemon binds it.
static HANDED_OUT_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

fn get_ephemeral_port() -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut used = HANDED_OUT_PORTS.lock().unwrap();
        if port != 8899 && port != 8901 && port > 1024 && !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

struct TestInstance {
    temp_dir: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    gemini_dir: PathBuf,
    bin_dir: PathBuf,
    config_file: PathBuf,
    token_file: PathBuf,
    port: u16,
    fake_agy: PathBuf,
    #[allow(dead_code)]
    fake_systemctl: PathBuf,
    tracked_pids: Arc<Mutex<Vec<i32>>>,
}

impl TestInstance {
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("create instance tempdir");
        let root = temp_dir.path();
        let home = root.join("home");
        let data_dir = root.join("data");
        let gemini_dir = root.join("gemini");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&gemini_dir).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();

        let token_dir = gemini_dir.join("antigravity-cli");
        std::fs::create_dir_all(&token_dir).unwrap();
        let token_file = token_dir.join("antigravity-oauth-token");

        let port = get_ephemeral_port();
        assert!(port != 8899 && port != 8901, "port must never be 8899 or 8901");

        let config_file = root.join("config.json");
        let cfg_json = serde_json::json!({
            "version": 1,
            "server": {
                "listen": "127.0.0.1",
                "port": port
            }
        });
        std::fs::write(&config_file, serde_json::to_string_pretty(&cfg_json).unwrap()).unwrap();

        let fake_agy = bin_dir.join("agy");
        let fake_agy_script = r#"#!/bin/sh
echo "ARGV: $@"
echo "CLOUD_CODE_URL=$CLOUD_CODE_URL"
TOKEN_PATH="${TOKEN_FILE:-$AGY_GEMINI_DIR/antigravity-cli/antigravity-oauth-token}"
if [ -f "$TOKEN_PATH" ]; then
    cat "$TOKEN_PATH"
fi
exit 0
"#;
        std::fs::write(&fake_agy, fake_agy_script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_agy, std::fs::Permissions::from_mode(0o755)).unwrap();

        let fake_systemctl = bin_dir.join("systemctl");
        let fake_systemctl_script = r#"#!/bin/sh
echo "fake systemctl called: $@" >&2
exit 1
"#;
        std::fs::write(&fake_systemctl, fake_systemctl_script).unwrap();
        std::fs::set_permissions(&fake_systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self {
            temp_dir,
            home,
            data_dir,
            gemini_dir,
            bin_dir,
            config_file,
            token_file,
            port,
            fake_agy,
            fake_systemctl,
            tracked_pids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn seed_account(&self, expiry: f64) {
        let db_path = self.data_dir.join("state.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open state.db");
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS accounts (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL DEFAULT '',
                email TEXT NOT NULL DEFAULT '',
                access_token TEXT NOT NULL DEFAULT '',
                refresh_token TEXT NOT NULL DEFAULT '',
                id_token TEXT NOT NULL DEFAULT '',
                token_expiry REAL,
                created_at INTEGER,
                updated_at INTEGER,
                extra TEXT
            );
            CREATE TABLE IF NOT EXISTS runtime_global (key TEXT PRIMARY KEY, value TEXT);
            CREATE TABLE IF NOT EXISTS account_runtime (
                account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                request_count INTEGER NOT NULL DEFAULT 0,
                gen_count INTEGER,
                error_count INTEGER NOT NULL DEFAULT 0,
                last_used_at INTEGER,
                status TEXT NOT NULL DEFAULT '',
                validation_url TEXT,
                rate_limited_until REAL
            );
            CREATE TABLE IF NOT EXISTS account_quota (
                account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                gemini_5h_fraction REAL,
                gemini_5h_reset TEXT,
                gemini_weekly_fraction REAL,
                gemini_weekly_reset TEXT,
                third_party_5h_fraction REAL,
                third_party_5h_reset TEXT,
                third_party_weekly_fraction REAL,
                third_party_weekly_reset TEXT,
                remaining_fraction REAL,
                reset_time REAL,
                updated_at INTEGER,
                raw_extra TEXT
            );
            CREATE TABLE IF NOT EXISTS inflight_leases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                pid INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                pid_ns TEXT NOT NULL DEFAULT ''
            );
            INSERT INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT INTO runtime_global (key, value) VALUES ('active_account_id', 'acc_1');
            INSERT INTO runtime_global (key, value) VALUES ('strategy', 'smart');
            INSERT INTO accounts (id, name, email, access_token, refresh_token, id_token, token_expiry, created_at, updated_at, extra)
                VALUES ('acc_1', 'Account 1', 'test@example.com', 'test-access-token', 'test-refresh-token', 'test-id-token', 0, 1700000000, 1700000000, '{}');
            INSERT INTO account_runtime (account_id, status) VALUES ('acc_1', 'active');
            "#,
        ).expect("seed single account");
        conn.execute("UPDATE accounts SET token_expiry = ?1 WHERE id = 'acc_1'", [expiry])
            .expect("set expiry");
    }

    fn command(&self, bin: &Path) -> Command {
        let mut cmd = Command::new(bin);
        cmd.env("HOME", &self.home);
        cmd.env("AGY_DATA_DIR", &self.data_dir);
        cmd.env("AGY_CONFIG_FILE", &self.config_file);
        cmd.env("AGY_GEMINI_DIR", &self.gemini_dir);
        cmd.env("AGY_ENTRYPOINT_PATH", bin);
        cmd.env("AGY_BIN", &self.fake_agy);
        cmd.env("PATH", format!("{}:/usr/bin:/bin", self.bin_dir.display()));
        cmd.env("TOKEN_FILE", &self.token_file);
        cmd.env_remove("AGY_TEST_MODE");
        cmd
    }

    fn record_pid_from_file(&self) -> Option<i32> {
        let pid_file = self.data_dir.join("agy-pool.pid");
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            let pid = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                val.get("pid").and_then(|v| v.as_i64()).map(|p| p as i32)
            } else {
                content.trim().parse::<i32>().ok()
            };
            if let Some(p) = pid
                && p > 1
            {
                let mut lock = self.tracked_pids.lock().unwrap();
                if !lock.contains(&p) {
                    lock.push(p);
                }
                return Some(p);
            }
        }
        None
    }

    fn wait_for_port(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for TestInstance {
    fn drop(&mut self) {
        let mut pids = self.tracked_pids.lock().unwrap().clone();
        let pid_file = self.data_dir.join("agy-pool.pid");
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(pid) = val.get("pid").and_then(|v| v.as_i64()) {
                    let p = pid as i32;
                    if !pids.contains(&p) {
                        pids.push(p);
                    }
                }
            } else if let Ok(p) = content.trim().parse::<i32>()
                && !pids.contains(&p)
            {
                pids.push(p);
            }
        }
        for pid in &pids {
            if *pid > 1 {
                unsafe {
                    libc::kill(*pid, libc::SIGTERM);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        for pid in &pids {
            if *pid > 1 && unsafe { libc::kill(*pid, 0) } == 0 {
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                    libc::waitpid(*pid, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
        }
    }
}

#[test]
fn run_writes_same_token_file_and_env_as_go() {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let go_inst = TestInstance::new();
    let rust_inst = TestInstance::new();

    let expiry = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600) as f64;

    go_inst.seed_account(expiry);
    rust_inst.seed_account(expiry);

    let go_out = go_inst
        .command(go_bin)
        .args([
            "run",
            "-c",
            go_inst.config_file.to_str().unwrap(),
            "-D",
            go_inst.data_dir.to_str().unwrap(),
            "--",
            "--model",
            "x",
        ])
        .output()
        .expect("execute go run");

    assert_eq!(
        go_out.status.code(),
        Some(0),
        "go run failed with exit code {:?}, stderr: {}",
        go_out.status.code(),
        String::from_utf8_lossy(&go_out.stderr)
    );
    let go_pid = go_inst.record_pid_from_file();
    assert!(go_pid.is_some(), "go run should have written a PID file");
    assert!(
        go_inst.wait_for_port(Duration::from_secs(5)),
        "go daemon must be listening on port {}",
        go_inst.port
    );

    let rust_out = rust_inst
        .command(rust_bin)
        .args([
            "run",
            "-c",
            rust_inst.config_file.to_str().unwrap(),
            "-D",
            rust_inst.data_dir.to_str().unwrap(),
            "--",
            "--model",
            "x",
        ])
        .output()
        .expect("execute rust run");

    assert_eq!(
        rust_out.status.code(),
        Some(0),
        "rust run failed with exit code {:?}, stderr: {}",
        rust_out.status.code(),
        String::from_utf8_lossy(&rust_out.stderr)
    );
    let rust_pid = rust_inst.record_pid_from_file();
    assert!(rust_pid.is_some(), "rust run should have written a PID file");
    assert!(
        rust_inst.wait_for_port(Duration::from_secs(5)),
        "rust daemon must be listening on port {}",
        rust_inst.port
    );

    let go_stdout = String::from_utf8_lossy(&go_out.stdout);
    let rust_stdout = String::from_utf8_lossy(&rust_out.stdout);

    // Normalize port in CLOUD_CODE_URL for comparing fake agy output
    let go_norm = go_stdout.replace(&format!(":{}", go_inst.port), ":<PORT>");
    let rust_norm = rust_stdout.replace(&format!(":{}", rust_inst.port), ":<PORT>");

    assert_eq!(
        go_norm, rust_norm,
        "fake-agy output mismatch:\n--- GO ---\n{}\n--- RUST ---\n{}",
        go_stdout, rust_stdout
    );

    // Verify token file bytes and file mode
    let go_token_bytes = std::fs::read(&go_inst.token_file).expect("read go token file");
    let rust_token_bytes = std::fs::read(&rust_inst.token_file).expect("read rust token file");

    use std::os::unix::fs::PermissionsExt;
    let go_mode = std::fs::metadata(&go_inst.token_file)
        .expect("stat go token file")
        .permissions()
        .mode()
        & 0o777;
    let rust_mode = std::fs::metadata(&rust_inst.token_file)
        .expect("stat rust token file")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(go_mode, 0o600, "go token file mode must be 0600");
    assert_eq!(rust_mode, 0o600, "rust token file mode must be 0600");

    assert_eq!(
        String::from_utf8_lossy(&go_token_bytes),
        String::from_utf8_lossy(&rust_token_bytes),
        "token file content mismatch:\n--- GO ---\n{}\n--- RUST ---\n{}",
        String::from_utf8_lossy(&go_token_bytes),
        String::from_utf8_lossy(&rust_token_bytes)
    );
}

#[test]
fn go_daemon_then_rust_restart_takes_over() {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let inst = TestInstance::new();

    // 1. Start instance with Go binary
    let start_out = inst
        .command(go_bin)
        .args([
            "start",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute go start");

    assert!(
        start_out.status.success(),
        "go start failed: {}",
        String::from_utf8_lossy(&start_out.stderr)
    );
    let go_pid = inst.record_pid_from_file().expect("go PID file must exist");
    assert!(
        inst.wait_for_port(Duration::from_secs(5)),
        "go daemon must be listening"
    );

    let go_exe = std::fs::read_link(format!("/proc/{}/exe", go_pid)).expect("read /proc/go_pid/exe");
    assert_eq!(
        go_exe.canonicalize().unwrap(),
        go_bin.canonicalize().unwrap(),
        "initial daemon must be Go binary"
    );

    // 2. Restart with Rust binary
    let restart_out = inst
        .command(rust_bin)
        .args([
            "restart",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute rust restart");

    assert!(
        restart_out.status.success(),
        "rust restart failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&restart_out.stdout),
        String::from_utf8_lossy(&restart_out.stderr)
    );

    assert!(
        inst.wait_for_port(Duration::from_secs(5)),
        "gateway port must answer after rust restart"
    );

    let rust_pid = inst.record_pid_from_file().expect("rust PID file must exist");
    assert_ne!(rust_pid, go_pid, "new PID must differ from old Go PID");

    let rust_exe = std::fs::read_link(format!("/proc/{}/exe", rust_pid)).expect("read /proc/rust_pid/exe");
    assert_eq!(
        rust_exe.canonicalize().unwrap(),
        rust_bin.canonicalize().unwrap(),
        "restarted daemon process exe must be Rust binary"
    );

    // Old Go PID must be gone
    assert!(
        unsafe { libc::kill(go_pid, 0) } != 0,
        "old Go PID {} must no longer be alive",
        go_pid
    );

    // Go's status on the same instance reports running
    let status_out = inst
        .command(go_bin)
        .args([
            "status",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute go status");

    assert!(
        status_out.status.success(),
        "go status failed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.to_lowercase().contains("running"),
        "go status should report running, got:\n{}",
        status_text
    );
}

#[test]
fn rust_daemon_then_go_restart_takes_over() {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let inst = TestInstance::new();

    // 1. Start instance with Rust binary
    let start_out = inst
        .command(rust_bin)
        .args([
            "start",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute rust start");

    assert!(
        start_out.status.success(),
        "rust start failed: {}",
        String::from_utf8_lossy(&start_out.stderr)
    );
    let rust_pid = inst.record_pid_from_file().expect("rust PID file must exist");
    assert!(
        inst.wait_for_port(Duration::from_secs(5)),
        "rust daemon must be listening"
    );

    let rust_exe = std::fs::read_link(format!("/proc/{}/exe", rust_pid)).expect("read /proc/rust_pid/exe");
    assert_eq!(
        rust_exe.canonicalize().unwrap(),
        rust_bin.canonicalize().unwrap(),
        "initial daemon must be Rust binary"
    );

    // 2. Restart with Go binary
    let restart_out = inst
        .command(go_bin)
        .args([
            "restart",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute go restart");

    assert!(
        restart_out.status.success(),
        "go restart failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&restart_out.stdout),
        String::from_utf8_lossy(&restart_out.stderr)
    );

    assert!(
        inst.wait_for_port(Duration::from_secs(5)),
        "gateway port must answer after go restart"
    );

    let go_pid = inst.record_pid_from_file().expect("go PID file must exist");
    assert_ne!(go_pid, rust_pid, "new PID must differ from old Rust PID");

    let go_exe = std::fs::read_link(format!("/proc/{}/exe", go_pid)).expect("read /proc/go_pid/exe");
    assert_eq!(
        go_exe.canonicalize().unwrap(),
        go_bin.canonicalize().unwrap(),
        "restarted daemon process exe must be Go binary"
    );

    // Old Rust PID must be gone
    assert!(
        unsafe { libc::kill(rust_pid, 0) } != 0,
        "old Rust PID {} must no longer be alive",
        rust_pid
    );

    // Rust's status on the same instance reports running
    let status_out = inst
        .command(rust_bin)
        .args([
            "status",
            "-c",
            inst.config_file.to_str().unwrap(),
            "-D",
            inst.data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("execute rust status");

    assert!(
        status_out.status.success(),
        "rust status failed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.to_lowercase().contains("running"),
        "rust status should report running, got:\n{}",
        status_text
    );
}

#[test]
fn run_with_daemon_down_and_agy_missing_matches_go() {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let go_inst = TestInstance::new();
    let rust_inst = TestInstance::new();

    let missing_go = go_inst.temp_dir.path().join("missing-agy");
    let missing_rust = rust_inst.temp_dir.path().join("missing-agy");

    let mut go_cmd = go_inst.command(go_bin);
    go_cmd.env("AGY_BIN", &missing_go);
    go_cmd.args([
        "run",
        "-c",
        go_inst.config_file.to_str().unwrap(),
        "-D",
        go_inst.data_dir.to_str().unwrap(),
        "--",
        "--model",
        "x",
    ]);

    let go_out = go_cmd.output().expect("execute go run");
    go_inst.record_pid_from_file();

    let mut rust_cmd = rust_inst.command(rust_bin);
    rust_cmd.env("AGY_BIN", &missing_rust);
    rust_cmd.args([
        "run",
        "-c",
        rust_inst.config_file.to_str().unwrap(),
        "-D",
        rust_inst.data_dir.to_str().unwrap(),
        "--",
        "--model",
        "x",
    ]);

    let rust_out = rust_cmd.output().expect("execute rust run");
    rust_inst.record_pid_from_file();

    assert_eq!(
        go_out.status.code(),
        rust_out.status.code(),
        "exit codes must match: go={:?}, rust={:?}",
        go_out.status.code(),
        rust_out.status.code()
    );

    // The fake agy was never run: stdout should be empty
    assert!(go_out.stdout.is_empty(), "go stdout should be empty");
    assert!(rust_out.stdout.is_empty(), "rust stdout should be empty");

    let go_err = agy_pool::cli::width::strip_ansi(&String::from_utf8_lossy(&go_out.stderr));
    let rust_err = agy_pool::cli::width::strip_ansi(&String::from_utf8_lossy(&rust_out.stderr));

    // Normalize paths and platform-specific ENOENT error formatting (Go os.Stat vs Rust io::Error)
    let go_norm = go_err.replace(missing_go.to_str().unwrap(), "<MISSING_AGY>").replace(
        "stat <MISSING_AGY>: no such file or directory",
        "no such file or directory",
    );
    let rust_norm = rust_err
        .replace(missing_rust.to_str().unwrap(), "<MISSING_AGY>")
        .replace("No such file or directory (os error 2)", "no such file or directory");

    assert_eq!(
        go_norm, rust_norm,
        "stderr mismatch:\n--- GO ---\n{}\n--- RUST ---\n{}",
        go_norm, rust_norm
    );
}

/// The daemon child must get the same AGY_* environment from Go and Rust `start`; an extra
/// AGY_GEMINI_DIR would move the gateway's native agy root away from ~/.gemini.
#[test]
fn daemon_env_matches_go() {
    let mut agy_keys = Vec::new();
    for bin in [get_go_bin(), get_rust_bin()] {
        let inst = TestInstance::new();
        let out = inst
            .command(bin)
            .env_remove("AGY_GEMINI_DIR")
            .args([
                "start",
                "-c",
                inst.config_file.to_str().unwrap(),
                "-D",
                inst.data_dir.to_str().unwrap(),
            ])
            .output()
            .expect("execute start");
        assert!(
            out.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pid = inst.record_pid_from_file().expect("PID file must exist");
        let environ = std::fs::read(format!("/proc/{pid}/environ")).expect("read daemon environ");
        let mut keys: Vec<String> = environ
            .split(|b| *b == 0)
            .filter_map(|kv| String::from_utf8_lossy(kv).split_once('=').map(|(k, _)| k.to_string()))
            .filter(|k| k.starts_with("AGY_"))
            .collect();
        keys.sort();
        agy_keys.push(keys);
    }
    assert_eq!(
        agy_keys[0], agy_keys[1],
        "daemon AGY_* env keys: Go (left) vs Rust (right)"
    );
}
