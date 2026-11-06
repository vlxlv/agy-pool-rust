use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

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

struct TestContext {
    data_dir: PathBuf,
    home_dir: PathBuf,
    config_file: PathBuf,
    gemini_dir: PathBuf,
    _tmp: tempfile::TempDir,
}

impl TestContext {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create test tempdir");
        let data_dir = tmp.path().join("data");
        let home_dir = tmp.path().join("home");
        let config_file = tmp.path().join("config.json");
        let gemini_dir = tmp.path().join("gemini");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();
        std::fs::create_dir_all(&gemini_dir).unwrap();
        Self {
            data_dir,
            home_dir,
            config_file,
            gemini_dir,
            _tmp: tmp,
        }
    }
}

struct RunResult {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_eval(
    bin: &Path,
    args: &[&str],
    stdin_data: Option<&[u8]>,
    data_dir: &Path,
    home_dir: &Path,
    config_file: &Path,
    gemini_dir: &Path,
) -> RunResult {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env("AGY_TEST_MODE", "1")
        .env("AGY_DATA_DIR", data_dir)
        .env("HOME", home_dir)
        .env("AGY_CONFIG_FILE", config_file)
        .env("AGY_GEMINI_DIR", gemini_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn binary");
    if let Some(data) = stdin_data {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data);
        }
    } else {
        drop(child.stdin.take());
    }

    let out = child.wait_with_output().expect("wait on child");
    RunResult {
        exit_code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn json_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
                ai == bi
            } else if let (Some(af), Some(bf)) = (a.as_f64(), b.as_f64()) {
                (af - bf).abs() < 1e-6
            } else {
                a == b
            }
        }
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a == b,
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| json_equivalent(x, y))
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|bv| json_equivalent(v, bv)))
        }
        _ => false,
    }
}

fn assert_differential(args: &[&str], stdin_data: Option<&[u8]>) {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let go_ctx = TestContext::new();
    let rust_ctx = TestContext::new();

    let go_res = run_eval(
        go_bin,
        args,
        stdin_data,
        &go_ctx.data_dir,
        &go_ctx.home_dir,
        &go_ctx.config_file,
        &go_ctx.gemini_dir,
    );
    let rust_res = run_eval(
        rust_bin,
        args,
        stdin_data,
        &rust_ctx.data_dir,
        &rust_ctx.home_dir,
        &rust_ctx.config_file,
        &rust_ctx.gemini_dir,
    );

    assert_eq!(
        go_res.exit_code, rust_res.exit_code,
        "Exit code mismatch for {:?}:\nGo exit: {:?}\nRust exit: {:?}\nGo stderr: {}\nRust stderr: {}",
        args, go_res.exit_code, rust_res.exit_code, go_res.stderr, rust_res.stderr
    );

    if go_res.exit_code == Some(0) {
        let go_json: serde_json::Value = serde_json::from_str(go_res.stdout.trim())
            .unwrap_or_else(|e| panic!("Go stdout not JSON for {:?}: {e}\nstdout: {}", args, go_res.stdout));
        let rust_json: serde_json::Value = serde_json::from_str(rust_res.stdout.trim())
            .unwrap_or_else(|e| panic!("Rust stdout not JSON for {:?}: {e}\nstdout: {}", args, rust_res.stdout));
        assert!(
            json_equivalent(&go_json, &rust_json),
            "JSON mismatch for {:?}\nGo stdout: {}\nRust stdout: {}",
            args,
            go_res.stdout,
            rust_res.stdout
        );
    }
}

fn assert_differential_with_ctx(build_args_and_stdin: impl Fn(&TestContext) -> (Vec<String>, Option<Vec<u8>>)) {
    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let go_ctx = TestContext::new();
    let (go_args, go_stdin) = build_args_and_stdin(&go_ctx);
    let go_args_refs: Vec<&str> = go_args.iter().map(|s| s.as_str()).collect();

    let rust_ctx = TestContext::new();
    let (rust_args, rust_stdin) = build_args_and_stdin(&rust_ctx);
    let rust_args_refs: Vec<&str> = rust_args.iter().map(|s| s.as_str()).collect();

    let go_res = run_eval(
        go_bin,
        &go_args_refs,
        go_stdin.as_deref(),
        &go_ctx.data_dir,
        &go_ctx.home_dir,
        &go_ctx.config_file,
        &go_ctx.gemini_dir,
    );
    let rust_res = run_eval(
        rust_bin,
        &rust_args_refs,
        rust_stdin.as_deref(),
        &rust_ctx.data_dir,
        &rust_ctx.home_dir,
        &rust_ctx.config_file,
        &rust_ctx.gemini_dir,
    );

    assert_eq!(
        go_res.exit_code, rust_res.exit_code,
        "Exit code mismatch:\nGo exit: {:?}\nRust exit: {:?}\nGo stderr: {}\nRust stderr: {}",
        go_res.exit_code, rust_res.exit_code, go_res.stderr, rust_res.stderr
    );

    if go_res.exit_code == Some(0) {
        let go_json: serde_json::Value = serde_json::from_str(go_res.stdout.trim())
            .unwrap_or_else(|e| panic!("Go stdout not JSON: {e}\nGo stdout: {}", go_res.stdout));
        let rust_json: serde_json::Value = serde_json::from_str(rust_res.stdout.trim())
            .unwrap_or_else(|e| panic!("Rust stdout not JSON: {e}\nRust stdout: {}", rust_res.stdout));
        assert!(
            json_equivalent(&go_json, &rust_json),
            "JSON mismatch\nGo stdout: {}\nRust stdout: {}",
            go_res.stdout,
            rust_res.stdout
        );
    }
}

struct ProcessGuard(Option<Child>);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// 1. eval-scheduler
#[test]
fn test_eval_scheduler() {
    // Normal input
    let normal_input = serde_json::json!({
        "candidates": [
            {
                "id": "acc_b",
                "name": "Beta",
                "quota": 0.8,
                "last_quota": {
                    "gemini_5h": { "fraction": 0.8 },
                    "gemini_weekly": { "fraction": 0.9 },
                    "updated_at": 1700000000
                }
            },
            {
                "id": "acc_a",
                "name": "Alpha",
                "quota": 0.4,
                "last_quota": {
                    "gemini_5h": { "fraction": 0.4 },
                    "gemini_weekly": { "fraction": 0.5 },
                    "updated_at": 1700000000
                }
            }
        ],
        "strategy": "smart",
        "now": 1700000100.0
    });
    assert_differential(&["eval-scheduler"], Some(normal_input.to_string().as_bytes()));

    // Edge input: empty candidates
    let empty_input = serde_json::json!({
        "candidates": [],
        "strategy": "smart",
        "now": 1700000100.0
    });
    assert_differential(&["eval-scheduler"], Some(empty_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-scheduler"], Some(b"not-json"));
}

// 2. eval-capacity
#[test]
fn test_eval_capacity() {
    // Normal input
    let normal_input = serde_json::json!({
        "account": {
            "id": "acc_1",
            "name": "Work",
            "quota": 0.5,
            "last_quota": {
                "gemini_5h": { "fraction": 0.3, "reset_time": "2026-11-06T15:00:00Z" },
                "gemini_weekly": { "fraction": 0.6, "reset_time": "2026-11-07T00:00:00Z" },
                "updated_at": 1700000000
            }
        },
        "now": 1700001000.0
    });
    assert_differential(&["eval-capacity"], Some(normal_input.to_string().as_bytes()));

    // Edge input: depleted quota, 0.0 fraction
    let edge_input = serde_json::json!({
        "account": {
            "id": "acc_depleted",
            "quota": 0.0,
            "last_quota": {
                "gemini_5h": { "fraction": 0.0 },
                "updated_at": 1700000000
            }
        },
        "now": 1700001000.0
    });
    assert_differential(&["eval-capacity"], Some(edge_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-capacity"], Some(b"not-json"));
}

// 3. eval-account-target
#[test]
fn test_eval_account_target() {
    // Normal input: target matches email
    let normal_input = serde_json::json!({
        "accounts": [
            { "id": "acc_1", "email": "alice@corp.com", "name": "Alice" },
            { "id": "acc_2", "email": "bob@corp.com", "name": "Bob" }
        ],
        "target": "alice@corp.com"
    });
    assert_differential(&["eval-account-target"], Some(normal_input.to_string().as_bytes()));

    // Edge input: target not found
    let edge_input = serde_json::json!({
        "accounts": [
            { "id": "acc_1", "email": "alice@corp.com", "name": "Alice" }
        ],
        "target": "nonexistent"
    });
    assert_differential(&["eval-account-target"], Some(edge_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-account-target"], Some(b"not-json"));
}

// 4. eval-display-name
#[test]
fn test_eval_display_name() {
    // Normal input: explicit name
    let normal_input = serde_json::json!({
        "account": { "id": "acc_1", "name": "Alice Work", "email": "alice@corp.com" }
    });
    assert_differential(&["eval-display-name"], Some(normal_input.to_string().as_bytes()));

    // Edge input: no name, fallback to id suffix
    let edge_input = serde_json::json!({
        "account": { "id": "acc_42" }
    });
    assert_differential(&["eval-display-name"], Some(edge_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-display-name"], Some(b"not-json"));
}

// 5. eval-jwt
#[test]
fn test_eval_jwt() {
    // Normal input: valid JWT token payload
    let token = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiIxMjM0NTY3ODkwIiwiZW1haWwiOiJ0ZXN0QGV4YW1wbGUuY29tIiwibmFtZSI6IlRlc3QgVXNlciJ9.";
    let normal_input = serde_json::json!({
        "jwt": token
    });
    assert_differential(&["eval-jwt"], Some(normal_input.to_string().as_bytes()));

    // Edge input: malformed jwt string
    let edge_input = serde_json::json!({
        "jwt": "invalid-token"
    });
    assert_differential(&["eval-jwt"], Some(edge_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-jwt"], Some(b"not-json"));
}

// 6. eval-error-classifier
#[test]
fn test_eval_error_classifier() {
    // Normal input: 403 validation error
    let normal_input = serde_json::json!({
        "status": 403,
        "body": "{\"error\": \"validation_required\", \"validation_url\": \"https://accounts.google.com/verify\"}"
    });
    assert_differential(&["eval-error-classifier"], Some(normal_input.to_string().as_bytes()));

    // Edge input: 200 normal response
    let edge_input = serde_json::json!({
        "status": 200,
        "body": "{\"candidates\": []}"
    });
    assert_differential(&["eval-error-classifier"], Some(edge_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-error-classifier"], Some(b"not-json"));
}

// 7. eval-crypto
#[test]
fn test_eval_crypto() {
    // Fixed salt and nonce (16 bytes base64-encoded)
    let salt = "AAAAAAAAAAAAAAAAAAAAAA==";
    let nonce = "AQEBAQEBAQEBAQEBAQEBAQ==";

    // Normal encrypt input
    let encrypt_input = serde_json::json!({
        "action": "encrypt",
        "data": "secret-account-token",
        "password": "super-strong-password",
        "salt": salt,
        "nonce": nonce
    });
    assert_differential(&["eval-crypto"], Some(encrypt_input.to_string().as_bytes()));

    // Normal decrypt input with a known bundle
    let bundle = serde_json::json!({
        "format": "agy-pool-encrypted-v1",
        "kdf": "pbkdf2-hmac-sha256",
        "salt": salt,
        "nonce": nonce,
        "iterations": 100000,
        "ciphertext": "0h3e4Uu5w6w8pDk=",
        "tag": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    });
    // Edge decrypt input with invalid password / tag mismatch
    let decrypt_bad_input = serde_json::json!({
        "action": "decrypt",
        "password": "wrong-password",
        "bundle": bundle
    });
    assert_differential(&["eval-crypto"], Some(decrypt_bad_input.to_string().as_bytes()));

    // Invalid input: malformed json
    assert_differential(&["eval-crypto"], Some(b"not-json"));
}

// 8. eval-daemon
#[test]
fn test_eval_daemon() {
    // 8a. format_size: normal & edge
    let format_normal = serde_json::json!({
        "action": "format_size",
        "bytes": 1048576
    });
    assert_differential(&["eval-daemon"], Some(format_normal.to_string().as_bytes()));

    let format_edge = serde_json::json!({
        "action": "format_size",
        "bytes": -5
    });
    assert_differential(&["eval-daemon"], Some(format_edge.to_string().as_bytes()));

    // 8b. read_pid: normal (existing PID file) & edge (missing PID file)
    assert_differential_with_ctx(|ctx| {
        let pid_file = ctx.data_dir.join("test.pid");
        let content = serde_json::json!({
            "pid": 12345,
            "port": 9999,
            "listen": "127.0.0.1",
            "data_dir": "/var/run/agy-pool"
        });
        std::fs::write(&pid_file, content.to_string()).unwrap();
        let input = serde_json::json!({
            "action": "read_pid",
            "path": pid_file.to_str().unwrap()
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    assert_differential_with_ctx(|ctx| {
        let missing_pid = ctx.data_dir.join("missing.pid");
        let input = serde_json::json!({
            "action": "read_pid",
            "path": missing_pid.to_str().unwrap()
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    // 8c. get_daemon_info: normal/edge with missing file
    assert_differential_with_ctx(|ctx| {
        let missing_pid = ctx.data_dir.join("missing.pid");
        let input = serde_json::json!({
            "action": "get_daemon_info",
            "path": missing_pid.to_str().unwrap()
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    // 8d. rotate_log: normal & edge
    assert_differential_with_ctx(|ctx| {
        let log_file = ctx.data_dir.join("daemon.log");
        std::fs::write(&log_file, "a".repeat(200)).unwrap();
        let input = serde_json::json!({
            "action": "rotate_log",
            "path": log_file.to_str().unwrap(),
            "max_bytes": 50,
            "backup_count": 2,
            "force": true
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    assert_differential_with_ctx(|ctx| {
        let missing_log = ctx.data_dir.join("missing.log");
        let input = serde_json::json!({
            "action": "rotate_log",
            "path": missing_log.to_str().unwrap(),
            "max_bytes": 50,
            "backup_count": 2,
            "force": false
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    // 8e. clear_log: normal & edge
    assert_differential_with_ctx(|ctx| {
        let log_file = ctx.data_dir.join("daemon.log");
        std::fs::write(&log_file, "some log data").unwrap();
        let input = serde_json::json!({
            "action": "clear_log",
            "path": log_file.to_str().unwrap(),
            "backup_count": 2
        });
        (vec!["eval-daemon".to_string()], Some(input.to_string().into_bytes()))
    });

    // 8f. stop_daemon: against spawned sleep 30 process
    // Normal: spawned sleep process with matching agy-pool name and data_dir
    let go_sleep = Command::new("bash")
        .args(["-c", "exec -a agy-pool sleep 30"])
        .spawn()
        .expect("spawn sleep process for go");
    let _go_guard = ProcessGuard(Some(go_sleep));

    let rust_sleep = Command::new("bash")
        .args(["-c", "exec -a agy-pool sleep 30"])
        .spawn()
        .expect("spawn sleep process for rust");
    let _rust_guard = ProcessGuard(Some(rust_sleep));

    let go_pid = _go_guard.0.as_ref().unwrap().id();
    let rust_pid = _rust_guard.0.as_ref().unwrap().id();

    let go_bin = get_go_bin();
    let rust_bin = get_rust_bin();

    let go_ctx = TestContext::new();
    let go_pid_file = go_ctx.data_dir.join("agy-pool.pid");
    std::fs::write(
        &go_pid_file,
        serde_json::json!({
            "pid": go_pid,
            "port": 18877,
            "data_dir": go_ctx.data_dir.to_str().unwrap()
        })
        .to_string(),
    )
    .unwrap();

    let rust_ctx = TestContext::new();
    let rust_pid_file = rust_ctx.data_dir.join("agy-pool.pid");
    std::fs::write(
        &rust_pid_file,
        serde_json::json!({
            "pid": rust_pid,
            "port": 18878,
            "data_dir": rust_ctx.data_dir.to_str().unwrap()
        })
        .to_string(),
    )
    .unwrap();

    let go_input = serde_json::json!({
        "action": "stop_daemon",
        "path": go_pid_file.to_str().unwrap(),
        "port": 18877
    });
    let rust_input = serde_json::json!({
        "action": "stop_daemon",
        "path": rust_pid_file.to_str().unwrap(),
        "port": 18878
    });

    let go_res = run_eval(
        go_bin,
        &["eval-daemon"],
        Some(go_input.to_string().as_bytes()),
        &go_ctx.data_dir,
        &go_ctx.home_dir,
        &go_ctx.config_file,
        &go_ctx.gemini_dir,
    );
    let rust_res = run_eval(
        rust_bin,
        &["eval-daemon"],
        Some(rust_input.to_string().as_bytes()),
        &rust_ctx.data_dir,
        &rust_ctx.home_dir,
        &rust_ctx.config_file,
        &rust_ctx.gemini_dir,
    );

    assert_eq!(go_res.exit_code, rust_res.exit_code);
    let go_json: serde_json::Value = serde_json::from_str(go_res.stdout.trim()).unwrap();
    let rust_json: serde_json::Value = serde_json::from_str(rust_res.stdout.trim()).unwrap();
    assert!(json_equivalent(&go_json, &rust_json));

    // Invalid input: malformed json
    assert_differential(&["eval-daemon"], Some(b"not-json"));
}

// 9. eval-conversation
#[test]
fn test_eval_conversation() {
    // Normal input: sqlite DB exists with conversation
    assert_differential_with_ctx(|ctx| {
        let db_path = ctx.gemini_dir.join("conversation_summaries.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT,
                title TEXT,
                workspace_uris TEXT,
                last_modified_time INTEGER
            );",
            [],
        )
        .unwrap();

        let ws_dir = ctx.home_dir.join("my-project");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_uri = format!("file://{}", ws_dir.canonicalize().unwrap().display());
        let uris_json = serde_json::to_string(&vec![ws_uri]).unwrap();

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, workspace_uris, last_modified_time)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["conv-1234", "Differential Title", uris_json, 1700000000],
        )
        .unwrap();

        (
            vec!["eval-conversation".to_string(), ws_dir.to_str().unwrap().to_string()],
            None,
        )
    });

    // Edge input: no DB, random non-existent directory
    assert_differential_with_ctx(|_ctx| {
        (
            vec!["eval-conversation".to_string(), "/nonexistent/directory".to_string()],
            None,
        )
    });
}

// 10. eval-resolve-continue
#[test]
fn test_eval_resolve_continue() {
    // Normal input: --conversation already provided (should return args unchanged)
    assert_differential(
        &["eval-resolve-continue", "--conversation=conv-explicit", "prompt text"],
        None,
    );

    // Edge input: -c provided when no conversation exists (returns unchanged args)
    assert_differential(&["eval-resolve-continue", "-c", "continue prompt"], None);

    // Normal input with conversation: -c resolves to existing conversation
    assert_differential_with_ctx(|ctx| {
        let db_path = ctx.gemini_dir.join("conversation_summaries.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT,
                title TEXT,
                workspace_uris TEXT,
                last_modified_time INTEGER
            );",
            [],
        )
        .unwrap();

        let cwd = std::env::current_dir().unwrap();
        let cwd_can = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let ws_uri = format!("file://{}", cwd_can.display());
        let uris_json = serde_json::to_string(&vec![ws_uri]).unwrap();

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, workspace_uris, last_modified_time)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["conv-res-999", "Continue Title", uris_json, 1700000000],
        )
        .unwrap();

        (
            vec![
                "eval-resolve-continue".to_string(),
                "-c".to_string(),
                "prompt text".to_string(),
            ],
            None,
        )
    });
}
