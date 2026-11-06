use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static GO_BIN: OnceLock<PathBuf> = OnceLock::new();
static GOSIDE_BIN: OnceLock<PathBuf> = OnceLock::new();

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

fn get_goside_bin() -> &'static Path {
    GOSIDE_BIN.get_or_init(|| {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
        let tmp = tempfile::tempdir().expect("create tempdir for goside binary");
        let bin_path = tmp.path().join("goside");
        let out = Command::new("go")
            .args(["build", "-o", bin_path.to_str().unwrap(), "./rust/interop/goside"])
            .current_dir(repo_root)
            .env("AGY_TEST_MODE", "1")
            .output()
            .expect("execute go build goside");
        assert!(
            out.status.success(),
            "go build goside failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::mem::forget(tmp);
        bin_path
    })
}

fn seed3(state_dir: &Path) {
    let goside = get_goside_bin();
    let out = Command::new(goside)
        .args([state_dir.to_str().unwrap(), "seed3"])
        .env("AGY_TEST_MODE", "1")
        .output()
        .expect("execute goside seed3");
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("OK <nil>"),
        "seed3 failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

struct ServerGuard {
    child: Option<std::process::Child>,
    stderr_buf: Arc<Mutex<String>>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Ports already handed to a server in this test process. The port is released before the server binds it,
/// so without this two parallel tests can be given the same port.
static HANDED_OUT_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

fn get_ephemeral_port() -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut used = HANDED_OUT_PORTS.lock().unwrap();
        if port != 8899 && port != 8901 && !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

fn spawn_proxy_test_server(bin: &Path, state_dir: &Path, backend_url: &str) -> (ServerGuard, u16) {
    for attempt in 0..5 {
        let port = get_ephemeral_port();
        let mut cmd = Command::new(bin);
        cmd.args([
            "proxy-test-server",
            &port.to_string(),
            state_dir.to_str().unwrap(),
            backend_url,
        ])
        .env("AGY_TEST_MODE", "1")
        .env("AGY_DATA_DIR", state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_e) if attempt < 4 => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => panic!("failed to spawn proxy-test-server: {e}"),
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line);
            }
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut reader, &mut sink);
        });

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = stderr_buf.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8; 512];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                stderr_buf_clone
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
        });

        let guard = ServerGuard {
            child: Some(child),
            stderr_buf,
        };

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(line) if line.trim() == "READY" => {
                // Go's proxy-test-server prints READY before it binds; a server that lost the port exits
                // right away, and requests would reach another test's server.
                std::thread::sleep(Duration::from_millis(200));
                let mut guard = guard;
                let exited = guard
                    .child
                    .as_mut()
                    .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
                if !exited {
                    return (guard, port);
                }
                if attempt < 4 {
                    drop(guard);
                    continue;
                }
                panic!("server exited right after READY (attempt {attempt})");
            }
            other => {
                if attempt < 4 {
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                panic!("server failed to become ready (attempt {attempt}): {other:?}");
            }
        }
    }
    unreachable!()
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct RecordedRequest {
    account_token: String,
    path: String,
    body: Vec<u8>,
}

#[derive(Clone)]
#[allow(dead_code)]
enum UpstreamBehavior {
    Respond(Vec<u8>),
    RespondStream(Vec<Vec<u8>>, Duration),
    RespondAndDrop(Vec<u8>),
    ResetAfterRequest,
    EofBeforeHeaders,
}

struct FakeUpstream {
    port: u16,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

async fn handle_upstream_connection(
    mut stream: tokio::net::TcpStream,
    behavior: UpstreamBehavior,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let mut buf = Vec::new();
    let mut temp = [0u8; 1024];
    let mut header_end = None;

    loop {
        let n = match stream.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&temp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = Some(pos);
            break;
        }
    }

    let Some(header_end_pos) = header_end else {
        return;
    };

    let header_bytes = &buf[..header_end_pos];
    let header_str = String::from_utf8_lossy(header_bytes);
    let mut lines = header_str.lines();
    let request_line = lines.next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();

    let mut account_token = String::new();
    let mut content_length: usize = 0;

    for line in lines {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("authorization:") {
            let val = line["authorization:".len()..].trim();
            if let Some(token) = val.strip_prefix("Bearer ") {
                account_token = token.trim().to_string();
            } else {
                account_token = val.to_string();
            }
        } else if lower.starts_with("content-length:") {
            let val = line["content-length:".len()..].trim();
            if let Ok(cl) = val.parse::<usize>() {
                content_length = cl;
            }
        }
    }

    let mut body = buf[header_end_pos + 4..].to_vec();
    while body.len() < content_length {
        let n = match stream.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        body.extend_from_slice(&temp[..n]);
    }

    records.lock().unwrap().push(RecordedRequest {
        account_token,
        path,
        body,
    });

    match behavior {
        UpstreamBehavior::Respond(data) => {
            let _ = stream.write_all(&data).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
        UpstreamBehavior::RespondStream(chunks, delay) => {
            for chunk in chunks {
                let _ = stream.write_all(&chunk).await;
                let _ = stream.flush().await;
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
            let _ = stream.shutdown().await;
        }
        UpstreamBehavior::RespondAndDrop(data) => {
            let _ = stream.write_all(&data).await;
            let _ = stream.flush().await;
            drop(stream);
        }
        UpstreamBehavior::ResetAfterRequest => {
            if let Ok(std_stream) = stream.into_std() {
                use std::os::fd::AsRawFd;
                let linger = libc::linger {
                    l_onoff: 1,
                    l_linger: 0,
                };
                unsafe {
                    libc::setsockopt(
                        std_stream.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        &linger as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::linger>() as libc::socklen_t,
                    );
                }
                drop(std_stream);
            }
        }
        UpstreamBehavior::EofBeforeHeaders => {
            drop(stream);
        }
    }
}

impl FakeUpstream {
    async fn start(behaviors: Vec<UpstreamBehavior>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake upstream");
        let port = listener.local_addr().unwrap().port();
        let records = Arc::new(Mutex::new(Vec::new()));
        let records_clone = records.clone();
        let behaviors = Arc::new(Mutex::new(behaviors));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let mut conn_idx = 0;
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept_res = listener.accept() => {
                        let (stream, _) = match accept_res {
                            Ok(s) => s,
                            Err(_) => break,
                        };
                        let behavior = {
                            let b = behaviors.lock().unwrap();
                            if conn_idx < b.len() {
                                b[conn_idx].clone()
                            } else {
                                UpstreamBehavior::Respond(
                                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                                )
                            }
                        };
                        conn_idx += 1;
                        let rec = records_clone.clone();
                        tokio::spawn(async move {
                            handle_upstream_connection(stream, behavior, rec).await;
                        });
                    }
                }
            }
        });

        Self {
            port,
            records,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

struct ClientResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn decode_chunked(mut data: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::new();
    while let Some(newline_pos) = data.windows(2).position(|w| w == b"\r\n") {
        let line = String::from_utf8_lossy(&data[..newline_pos]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let Ok(chunk_size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if chunk_size == 0 {
            break;
        }
        let chunk_start = newline_pos + 2;
        if data.len() < chunk_start + chunk_size {
            decoded.extend_from_slice(&data[chunk_start..]);
            break;
        }
        decoded.extend_from_slice(&data[chunk_start..chunk_start + chunk_size]);
        let next_start = chunk_start + chunk_size;
        if data.len() >= next_start + 2 && &data[next_start..next_start + 2] == b"\r\n" {
            data = &data[next_start + 2..];
        } else {
            data = &data[next_start..];
        }
    }
    decoded
}

fn parse_http_response(raw: &[u8]) -> Result<ClientResponse, Box<dyn std::error::Error + Send + Sync>> {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| {
        format!(
            "no header end in response of {} bytes: {:?}",
            raw.len(),
            String::from_utf8_lossy(raw)
        )
    })?;

    let header_str = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_str.lines();
    let status_line = lines.next().ok_or("empty response")?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or("missing status code")?
        .parse::<u16>()?;

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let raw_body = &raw[header_end + 4..];
    let is_chunked = headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false);

    let body = if is_chunked {
        decode_chunked(raw_body)
    } else {
        raw_body.to_vec()
    };

    Ok(ClientResponse { status, headers, body })
}

async fn send_http_request(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<ClientResponse, Box<dyn std::error::Error + Send + Sync>> {
    let start = std::time::Instant::now();
    let mut stream = loop {
        match tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await {
            Ok(s) => break s,
            Err(_e) if start.elapsed() < Duration::from_secs(3) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };

    let mut req_bytes = Vec::new();
    req_bytes.extend_from_slice(format!("POST {path} HTTP/1.1\r\n").as_bytes());
    req_bytes.extend_from_slice(b"Host: 127.0.0.1\r\n");
    req_bytes.extend_from_slice(b"User-Agent: agy-test/1.0\r\n");
    req_bytes.extend_from_slice(b"Connection: close\r\n");
    let mut has_content_length = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        req_bytes.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_content_length {
        req_bytes.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    req_bytes.extend_from_slice(b"\r\n");
    req_bytes.extend_from_slice(body);

    stream.write_all(&req_bytes).await?;
    stream.flush().await?;

    let mut resp_bytes = Vec::new();
    let mut temp = [0u8; 4096];
    loop {
        match stream.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => resp_bytes.extend_from_slice(&temp[..n]),
            Err(_) => break,
        }
    }

    parse_http_response(&resp_bytes)
}

#[derive(Clone, Debug)]
struct ScenarioRunResult {
    status: u16,
    body: Vec<u8>,
    account: Option<String>,
    account_id: Option<String>,
    strategy: Option<String>,
    affinity: Option<String>,
    upstream_accounts: Vec<String>,
    upstream_connection_count: usize,
}

enum GatewayTarget {
    Go,
    Rust,
}

async fn run_single(
    target: GatewayTarget,
    behaviors: Vec<UpstreamBehavior>,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> ScenarioRunResult {
    let upstream = FakeUpstream::start(behaviors).await;
    let backend_url = format!("http://127.0.0.1:{}", upstream.port);

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let state_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    seed3(&state_dir);

    let bin_path = match target {
        GatewayTarget::Go => get_go_bin(),
        GatewayTarget::Rust => Path::new(env!("CARGO_BIN_EXE_agy-pool")),
    };

    let (guard, server_port) = spawn_proxy_test_server(bin_path, &state_dir, &backend_url);

    let target_name = match target {
        GatewayTarget::Go => "Go",
        GatewayTarget::Rust => "Rust",
    };
    let client_res = match send_http_request(server_port, path, headers, body).await {
        Ok(res) => res,
        Err(e) => {
            let stderr = guard.stderr_buf.lock().unwrap().clone();
            panic!("client request to {target_name} on {server_port} failed: {e}; server stderr: {stderr}");
        }
    };

    drop(guard);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = upstream.records.lock().unwrap().clone();
    drop(upstream);

    let account = client_res.headers.get("x-agy-pool-account").cloned();
    let account_id = client_res.headers.get("x-agy-pool-account-id").cloned();
    let strategy = client_res.headers.get("x-agy-pool-strategy").cloned();
    let affinity = client_res.headers.get("x-agy-pool-affinity").cloned();
    let upstream_accounts = records.iter().map(|r| r.account_token.clone()).collect();
    let upstream_connection_count = records.len();

    ScenarioRunResult {
        status: client_res.status,
        body: client_res.body,
        account,
        account_id,
        strategy,
        affinity,
        upstream_accounts,
        upstream_connection_count,
    }
}

fn assert_differential_match(go: &ScenarioRunResult, rust: &ScenarioRunResult) {
    assert_eq!(
        go.status, rust.status,
        "status mismatch: go={}, rust={}",
        go.status, rust.status
    );
    assert_eq!(
        go.body,
        rust.body,
        "body mismatch: go={:?}, rust={:?}",
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rust.body)
    );
    assert_eq!(
        go.account, rust.account,
        "account mismatch: go={:?}, rust={:?}",
        go.account, rust.account
    );
    assert_eq!(
        go.account_id, rust.account_id,
        "account_id mismatch: go={:?}, rust={:?}",
        go.account_id, rust.account_id
    );
    assert_eq!(
        go.strategy, rust.strategy,
        "strategy mismatch: go={:?}, rust={:?}",
        go.strategy, rust.strategy
    );
    assert_eq!(
        go.affinity, rust.affinity,
        "affinity mismatch: go={:?}, rust={:?}",
        go.affinity, rust.affinity
    );
    assert_eq!(
        go.upstream_accounts, rust.upstream_accounts,
        "upstream accounts mismatch: go={:?}, rust={:?}",
        go.upstream_accounts, rust.upstream_accounts
    );
    assert_eq!(
        go.upstream_connection_count, rust.upstream_connection_count,
        "upstream connection count mismatch: go={}, rust={}",
        go.upstream_connection_count, rust.upstream_connection_count
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ok_buffered() {
    let behaviors = vec![UpstreamBehavior::Respond(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"status\":\"ok_buffered_1\"}".to_vec(),
    )];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"{\"status\":\"ok_buffered_1\"}");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ok_sse_three_events() {
    let behaviors = vec![UpstreamBehavior::Respond(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\ndata: 1\n\n\r\n9\r\ndata: 2\n\n\r\n9\r\ndata: 3\n\n\r\n0\r\n\r\n".to_vec(),
    )];
    let path = "/v1/models/gemini-2.5-flash:streamGenerateContent?alt=sse";
    let headers = [("Content-Type", "application/json"), ("Accept", "text/event-stream")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"data: 1\n\ndata: 2\n\ndata: 3\n\n");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_429_fails_over() {
    let behaviors = vec![
        UpstreamBehavior::Respond(
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 46\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"Resource has exhausted\"}}".to_vec(),
        ),
        UpstreamBehavior::Respond(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"status\":\"ok_2\"}".to_vec(),
        ),
    ];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"{\"status\":\"ok_2\"}");
    assert_eq!(rust_res.account.as_deref(), Some("Beta"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_2"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("failover"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1", "token-acc_2"]);
    assert_eq!(rust_res.upstream_connection_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_503_fails_over_and_pauses_model() {
    let behaviors = behaviors_503();
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"{\"status\":\"ok_2\"}");
    assert_eq!(rust_res.account.as_deref(), Some("Beta"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_2"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("failover"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1", "token-acc_2"]);
    assert_eq!(rust_res.upstream_connection_count, 2);

    // The pause: a second request for the same model on the same gateway must skip the paused acc_1.
    let mut paused_behaviors = behaviors_503();
    paused_behaviors.push(UpstreamBehavior::Respond(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"status\":\"ok_3\"}".to_vec(),
    ));
    let (go_r1, go_r2) = run_two_requests(GatewayTarget::Go, paused_behaviors.clone(), body).await;
    let (rust_r1, rust_r2) = run_two_requests(GatewayTarget::Rust, paused_behaviors, body).await;
    assert_differential_match(&go_r1, &rust_r1);
    assert_differential_match(&go_r2, &rust_r2);
    assert_eq!(rust_r2.status, 200);
    assert_eq!(rust_r2.upstream_accounts.len(), 3, "{:?}", rust_r2.upstream_accounts);
    assert_ne!(
        rust_r2.upstream_accounts[2], "token-acc_1",
        "paused account must not get the next request for that model"
    );
}

fn behaviors_503() -> Vec<UpstreamBehavior> {
    vec![
        UpstreamBehavior::Respond(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 42\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"service unavailable\"}}".to_vec(),
        ),
        UpstreamBehavior::Respond(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"status\":\"ok_2\"}".to_vec(),
        ),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_401_fails_over() {
    let behaviors = vec![
        UpstreamBehavior::Respond(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"unauthorized\"}}".to_vec(),
        ),
        UpstreamBehavior::Respond(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"status\":\"ok_2\"}".to_vec(),
        ),
    ];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"{\"status\":\"ok_2\"}");
    assert_eq!(rust_res.account.as_deref(), Some("Beta"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_2"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("failover"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1", "token-acc_2"]);
    assert_eq!(rust_res.upstream_connection_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_500_terminal() {
    let behaviors = vec![UpstreamBehavior::Respond(
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"server error\"}}".to_vec(),
    )];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 500);
    assert_eq!(rust_res.body, b"{\"error\":{\"message\":\"server error\"}}");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reset_after_request_terminal() {
    let behaviors = vec![UpstreamBehavior::ResetAfterRequest];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 502);
    assert_eq!(rust_res.body, b"Upstream request failed\n");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eof_before_headers_terminal() {
    let behaviors = vec![UpstreamBehavior::EofBeforeHeaders];
    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let headers = [("Content-Type", "application/json")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 502);
    assert_eq!(rust_res.body, b"Upstream request failed\n");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_truncated_mid_stream() {
    let behaviors = vec![UpstreamBehavior::RespondAndDrop(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nf\r\ndata: partial\n\n\r\n".to_vec(),
    )];
    let path = "/v1/models/gemini-2.5-flash:streamGenerateContent?alt=sse";
    let headers = [("Content-Type", "application/json"), ("Accept", "text/event-stream")];
    let body = br#"{"model":"gemini-2.5-flash","contents":[]}"#;

    let go_res = run_single(GatewayTarget::Go, behaviors.clone(), path, &headers, body).await;
    let rust_res = run_single(GatewayTarget::Rust, behaviors, path, &headers, body).await;

    assert_differential_match(&go_res, &rust_res);
    assert_eq!(rust_res.status, 200);
    assert_eq!(rust_res.body, b"data: partial\n\n");
    assert_eq!(rust_res.account.as_deref(), Some("Alpha"));
    assert_eq!(rust_res.account_id.as_deref(), Some("acc_1"));
    assert_eq!(rust_res.strategy.as_deref(), Some("smart"));
    assert_eq!(rust_res.affinity.as_deref(), Some("none"));
    assert_eq!(rust_res.upstream_accounts, vec!["token-acc_1"]);
    assert_eq!(rust_res.upstream_connection_count, 1);
}

async fn run_two_requests(
    target: GatewayTarget,
    behaviors: Vec<UpstreamBehavior>,
    body: &[u8],
) -> (ScenarioRunResult, ScenarioRunResult) {
    let upstream = FakeUpstream::start(behaviors).await;
    let backend_url = format!("http://127.0.0.1:{}", upstream.port);

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let state_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    seed3(&state_dir);

    let bin_path = match target {
        GatewayTarget::Go => get_go_bin(),
        GatewayTarget::Rust => Path::new(env!("CARGO_BIN_EXE_agy-pool")),
    };

    let path = "/v1/models/gemini-2.5-flash:generateContent";
    let (guard, server_port) = spawn_proxy_test_server(bin_path, &state_dir, &backend_url);
    let headers = [("Content-Type", "application/json")];
    let res1 = send_http_request(server_port, path, &headers, body)
        .await
        .expect("client request 1 failed");

    let res2 = send_http_request(server_port, path, &headers, body)
        .await
        .expect("client request 2 failed");

    drop(guard);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let records = upstream.records.lock().unwrap().clone();
    drop(upstream);

    let r1 = ScenarioRunResult {
        status: res1.status,
        body: res1.body,
        account: res1.headers.get("x-agy-pool-account").cloned(),
        account_id: res1.headers.get("x-agy-pool-account-id").cloned(),
        strategy: res1.headers.get("x-agy-pool-strategy").cloned(),
        affinity: res1.headers.get("x-agy-pool-affinity").cloned(),
        upstream_accounts: vec![records.first().map(|r| r.account_token.clone()).unwrap_or_default()],
        upstream_connection_count: records.len(),
    };

    let r2 = ScenarioRunResult {
        status: res2.status,
        body: res2.body,
        account: res2.headers.get("x-agy-pool-account").cloned(),
        account_id: res2.headers.get("x-agy-pool-account-id").cloned(),
        strategy: res2.headers.get("x-agy-pool-strategy").cloned(),
        affinity: res2.headers.get("x-agy-pool-affinity").cloned(),
        upstream_accounts: records.iter().map(|r| r.account_token.clone()).collect(),
        upstream_connection_count: records.len(),
    };

    (r1, r2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_affinity_sticks_across_two_requests() {
    let behaviors = vec![
        UpstreamBehavior::Respond(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"status\":\"resp_1\"}".to_vec(),
        ),
        UpstreamBehavior::Respond(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"status\":\"resp_2\"}".to_vec(),
        ),
    ];
    let body = br#"{"model":"gemini-2.5-flash","request":{"labels":{"cascade_id":"cascade-diff-test"}}}"#;
    let (go_r1, go_r2) = run_two_requests(GatewayTarget::Go, behaviors.clone(), body).await;
    let (rust_r1, rust_r2) = run_two_requests(GatewayTarget::Rust, behaviors, body).await;

    // Both gateways succeed on request 1 with same account and affinity "none"
    assert_eq!(go_r1.status, rust_r1.status);
    assert_eq!(go_r1.account_id, rust_r1.account_id);
    assert_eq!(go_r1.affinity, rust_r1.affinity);

    // Both gateways succeed on request 2 with SAME account and affinity "hit"
    assert_eq!(go_r2.status, rust_r2.status);
    assert_eq!(go_r2.account_id, rust_r2.account_id);
    assert_eq!(
        rust_r2.account_id, rust_r1.account_id,
        "rust session affinity must stick across requests"
    );
    assert_eq!(
        go_r2.account_id, go_r1.account_id,
        "go session affinity must stick across requests"
    );
    assert_eq!(go_r2.affinity, rust_r2.affinity);
    assert_eq!(rust_r2.affinity.as_deref(), Some("hit"));

    // Upstream saw same accounts
    assert_eq!(go_r2.upstream_accounts, rust_r2.upstream_accounts);
    assert_eq!(go_r2.upstream_connection_count, rust_r2.upstream_connection_count);
}
