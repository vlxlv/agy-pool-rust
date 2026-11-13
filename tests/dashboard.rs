use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::http::{HeaderMap, Method, StatusCode};
use tempfile::TempDir;
use tokio::sync::Mutex;

static DASHBOARD_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub struct DashboardTestGuard<'a> {
    _async_guard: tokio::sync::MutexGuard<'a, ()>,
    _sync_guard: MutexGuard<'static, ()>,
}

impl<'a> Drop for DashboardTestGuard<'a> {
    fn drop(&mut self) {
        agy_pool::config::set_static_config(agy_pool::config::default_static_config());
        agy_pool::config::reset_data_dir();
        agy_pool::config::set_test_mode(false);
    }
}

async fn lock_test() -> DashboardTestGuard<'static> {
    let async_guard = DASHBOARD_TEST_LOCK.lock().await;
    let sync_guard = agy_pool::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    agy_pool::config::set_test_mode(true);
    agy_pool::dashboard::auth::reset_auth_failures();
    DashboardTestGuard {
        _async_guard: async_guard,
        _sync_guard: sync_guard,
    }
}

fn setup_test_env() -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    agy_pool::config::configure_state_dir(tmp.path()).expect("failed to configure state dir");
    tmp
}

fn setup_sample_pool() {
    let acc1 = agy_pool::storage::Account {
        id: "acc_1".to_string(),
        email: "alice@example.com".to_string(),
        ..Default::default()
    };
    let acc2 = agy_pool::storage::Account {
        id: "acc_2".to_string(),
        email: "bob@example.com".to_string(),
        ..Default::default()
    };
    let pool = agy_pool::storage::Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    agy_pool::storage::save_pool(&pool).expect("failed to save pool");
}

async fn start_server() -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(agy_pool::proxy::Handler::new());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                accept_res = listener.accept() => {
                    if let Ok((stream, peer)) = accept_res {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let h = handler.clone();
                        let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                            req.extensions_mut().insert(agy_pool::dashboard::PeerAddr(peer));
                            let h = h.clone();
                            async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                        });
                        tokio::spawn(async move {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service)
                                .await;
                        });
                    } else {
                        break;
                    }
                }
            }
        }
    });

    (addr, server_task, shutdown_tx)
}

async fn send_request(
    addr: SocketAddr,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, Bytes) {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut req_builder = hyper::Request::builder().method(method).uri(path_and_query);
    if !headers.contains_key(hyper::header::HOST) {
        req_builder = req_builder.header(hyper::header::HOST, addr.to_string());
    }
    if !headers.contains_key(hyper::header::CONNECTION) {
        req_builder = req_builder.header(hyper::header::CONNECTION, "close");
    }
    for (k, v) in headers.iter() {
        req_builder = req_builder.header(k, v);
    }
    let req = req_builder.body(http_body_util::Full::new(body)).unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let mut body_bytes = bytes::BytesMut::new();
    let mut resp_body = resp.into_body();
    while let Some(frame_res) = resp_body.frame().await {
        if let Ok(frame) = frame_res
            && let Some(chunk) = frame.data_ref()
        {
            body_bytes.extend_from_slice(chunk);
        }
    }
    drop(sender);
    (status, resp_headers, body_bytes.freeze())
}

const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn set_dashboard_config(token: Option<&str>, allow_unauthenticated_control: bool) {
    let mut cfg = agy_pool::config::default_static_config();
    cfg.dashboard.auth_token = token.map(str::to_string);
    cfg.dashboard.allow_unauthenticated_control = allow_unauthenticated_control;
    agy_pool::config::set_static_config(cfg);
}

fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(*k, v.parse().unwrap());
    }
    h
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn error_code(body: &Bytes) -> String {
    let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    json["error"]["code"].as_str().unwrap_or_default().to_string()
}

async fn post_strategy(addr: SocketAddr, headers: HeaderMap, body: &'static str) -> (StatusCode, HeaderMap, Bytes) {
    send_request(
        addr,
        Method::POST,
        "/api/v1/strategy",
        headers,
        Bytes::from_static(body.as_bytes()),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dashboard_html_and_rest_routes() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    // 1. GET / and GET /dashboard and GET /dashboard/ return 200 OK with text/html; charset=utf-8
    for path in ["/", "/dashboard", "/dashboard/"] {
        let (status, headers, body) = send_request(addr, Method::GET, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::OK, "path {path} status");
        let content_type = headers.get(hyper::header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(
            content_type.contains("text/html; charset=utf-8"),
            "content type for {path}: {content_type}"
        );
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("agy-pool dashboard"), "body for {path}: {body_str}");
    }

    // 2. GET /api/v1/status returns 200 OK JSON containing version, strategy, latency.ttfb_p50_ms, qps
    let (status, headers, body) =
        send_request(addr, Method::GET, "/api/v1/status", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json from /api/v1/status");
    assert!(json.get("version").is_some());
    assert!(json.get("strategy").is_some());
    assert!(json.get("qps").is_some());
    let latency = json.get("latency").expect("latency object");
    assert!(latency.get("ttfb_p50_ms").is_some());

    // 3. GET /api/v1/accounts returns 200 OK JSON with email_masked, omits email, and reports show_email_allowed=false
    let (status, headers, body) =
        send_request(addr, Method::GET, "/api/v1/accounts", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json from /api/v1/accounts");
    assert_eq!(json.get("show_email_allowed").and_then(|v| v.as_bool()), Some(false));
    let accounts = json.get("accounts").and_then(|a| a.as_array()).expect("accounts array");
    assert!(!accounts.is_empty());
    let first = &accounts[0];
    assert!(first.get("email_masked").is_some());
    assert!(first.get("email").is_none());

    // 4. GET /api/v1/accounts?show_email=true still omits email while server config disables it
    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts?show_email=true",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json with show_email=true gated");
    assert_eq!(json.get("show_email_allowed").and_then(|v| v.as_bool()), Some(false));
    let accounts = json.get("accounts").and_then(|a| a.as_array()).expect("accounts array");
    assert!(accounts[0].get("email").is_none());

    // 4b. When pool enables show_email=Some(true), ?show_email=true reveals full email while ?show_email=false masks it
    let mut pool = agy_pool::storage::load_pool().unwrap();
    pool.show_email = Some(true);
    agy_pool::storage::save_pool(&pool).unwrap();

    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts?show_email=false",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json with show_email=false");
    assert_eq!(json.get("show_email_allowed").and_then(|v| v.as_bool()), Some(true));
    assert!(json["accounts"][0].get("email").is_none());

    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts?show_email=true",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json with show_email=true allowed");
    assert_eq!(json.get("show_email_allowed").and_then(|v| v.as_bool()), Some(true));
    let accounts = json.get("accounts").and_then(|a| a.as_array()).expect("accounts array");
    assert_eq!(
        accounts[0].get("email").and_then(|e| e.as_str()),
        Some("alice@example.com")
    );

    // 5. GET /api/v1/forecast returns 200 OK JSON with is_sustainable, runway_text
    let (status, headers, body) =
        send_request(addr, Method::GET, "/api/v1/forecast", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json from /api/v1/forecast");
    assert!(json.get("is_sustainable").is_some());
    assert!(json.get("runway_text").is_some());

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dashboard_non_get_returns_405() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    // POST /api/v1/status and POST /dashboard return 405 Method Not Allowed with Allow: GET header
    for path in ["/api/v1/status", "/dashboard"] {
        let (status, headers, _) =
            send_request(addr, Method::POST, path, HeaderMap::new(), Bytes::from_static(b"{}")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "path {path} should return 405");
        let allow = headers.get(hyper::header::ALLOW).expect("Allow header present");
        assert_eq!(allow, "GET", "Allow header value for {path}");
    }

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_proxy_routes_remain_unaffected() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    // Empty pool so proxy will return 503 Service Unavailable for generation
    let (addr, server_task, shutdown_tx) = start_server().await;

    // 1. GET /healthz returns 200 OK {"status":"ok"}
    let (status, headers, body) = send_request(addr, Method::GET, "/healthz", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
    assert_eq!(body.as_ref(), b"{\"status\":\"ok\"}");

    // 2. GET /metrics returns 200 OK Prometheus text
    let (status, headers, body) = send_request(addr, Method::GET, "/metrics", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(hyper::header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("text/plain"));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("agy_pool_") || text.contains("# HELP"));

    // 3. POST /v1/models/gemini:generateContent passes through proxy dispatch (503 on empty pool)
    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/v1/models/gemini:generateContent",
        HeaderMap::new(),
        Bytes::from_static(b"{\"contents\":[]}"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // 4. Non-GET / falls through to proxy dispatch rather than returning 405 Method Not Allowed
    let (status, _, _) = send_request(addr, Method::POST, "/", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_host_and_origin_validation() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    // 1. Disallowed DNS-rebinding & malformed Hosts are rejected with 403 Forbidden
    for path in ["/", "/dashboard", "/api/v1/accounts", "/api/v1/events"] {
        for bad_host in [
            "evil.com:8899",
            "localhost.evil.com:8899",
            ".local:8899",
            "evil..ts.net:8899",
        ] {
            let mut h = HeaderMap::new();
            h.insert(hyper::header::HOST, bad_host.parse().unwrap());
            let (status, _, _) = send_request(addr, Method::GET, path, h, Bytes::new()).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "host {bad_host} must be blocked on {path}"
            );
        }
    }

    // 2. Allowed Hosts: localhost, IPv6 loopback, Tailscale IPv4 & IPv6, Tailscale MagicDNS (short + FQDN), and mDNS .local
    for allowed_host in [
        "localhost:8899",
        "[::1]:8899",
        "100.64.1.2:8899",
        "[fd7a:115c:a1e0::1]:8899",
        "myhost:8899",
        "myhost.local:8899",
        "devbox.tail1234.ts.net:8899",
    ] {
        let mut h = HeaderMap::new();
        h.insert(hyper::header::HOST, allowed_host.parse().unwrap());
        let (status, _, _) = send_request(addr, Method::GET, "/api/v1/status", h, Bytes::new()).await;
        assert_eq!(status, StatusCode::OK, "host {allowed_host} should be allowed");
    }

    // 3. Matching Origin is allowed; mismatched Origin, port, or Origin: null is rejected
    let mut same_origin = HeaderMap::new();
    same_origin.insert(hyper::header::HOST, "100.64.1.2:8899".parse().unwrap());
    same_origin.insert(hyper::header::ORIGIN, "http://100.64.1.2:8899".parse().unwrap());
    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/accounts", same_origin, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);

    for bad_origin in ["http://evil.com", "null", "http://100.64.1.2:9999"] {
        let mut h = HeaderMap::new();
        h.insert(hyper::header::HOST, "100.64.1.2:8899".parse().unwrap());
        h.insert(hyper::header::ORIGIN, bad_origin.parse().unwrap());
        let (status, _, _) = send_request(addr, Method::GET, "/api/v1/accounts", h, Bytes::new()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "origin {bad_origin} must be rejected");
    }

    // 4. Sec-Fetch-Site: cross-site is rejected on API routes and non-document fetches, allowed only on top-level document navigation
    let mut cross_api = HeaderMap::new();
    cross_api.insert("sec-fetch-site", "cross-site".parse().unwrap());
    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/accounts", cross_api, Bytes::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let mut cross_iframe = HeaderMap::new();
    cross_iframe.insert("sec-fetch-site", "cross-site".parse().unwrap());
    cross_iframe.insert("sec-fetch-mode", "navigate".parse().unwrap());
    cross_iframe.insert("sec-fetch-dest", "iframe".parse().unwrap());
    let (status, _, _) = send_request(addr, Method::GET, "/", cross_iframe, Bytes::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let mut cross_doc = HeaderMap::new();
    cross_doc.insert("sec-fetch-site", "cross-site".parse().unwrap());
    cross_doc.insert("sec-fetch-mode", "navigate".parse().unwrap());
    cross_doc.insert("sec-fetch-dest", "document".parse().unwrap());
    let (status, _, _) = send_request(addr, Method::GET, "/", cross_doc, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sse_events_replay_and_live_stream() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    agy_pool::dashboard::telemetry::reset_for_test();

    // Call dashboard::telemetry::record_attempt(...) before connecting
    agy_pool::dashboard::telemetry::record_attempt(
        agy_pool::dashboard::telemetry::DashboardAuditEvent {
            id: 0,
            timestamp: jiff::Timestamp::now(),
            model: "gemini-2.5-flash".to_string(),
            account: "Alice".to_string(),
            ttfb_ms: 100,
            total_ms: 200,
            tokens: 50,
            output_tokens: 0,
            thinking_tokens: 0,
            status: 200,
            failover: false,
        },
        true,
    );

    let (addr, server_task, shutdown_tx) = start_server().await;

    // Open GET /api/v1/events via Hyper HTTP/1 client
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let req = hyper::Request::builder()
        .method(Method::GET)
        .uri("/api/v1/events")
        .header(hyper::header::HOST, addr.to_string())
        .header(hyper::header::CONNECTION, "keep-alive")
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/event-stream; charset=utf-8"), "content type: {ct}");

    let mut body = resp.into_body();

    // Read initial frames (body.frame().await with timeout) and assert pre-recorded event is replayed
    let replay_event = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(frame_res) = body.frame().await {
            if let Ok(frame) = frame_res
                && let Some(chunk) = frame.data_ref()
            {
                let s = String::from_utf8_lossy(chunk);
                if s.contains("event: audit") && s.contains("gemini-2.5-flash") {
                    return s.to_string();
                }
            }
        }
        String::new()
    })
    .await
    .expect("timed out waiting for replay audit event");

    assert!(replay_event.contains("id: 1\nevent: audit\ndata: "));
    assert!(replay_event.contains("gemini-2.5-flash"));

    // Call dashboard::telemetry::record_attempt(...) again while stream is open
    agy_pool::dashboard::telemetry::record_attempt(
        agy_pool::dashboard::telemetry::DashboardAuditEvent {
            id: 0,
            timestamp: jiff::Timestamp::now(),
            model: "gemini-2.5-pro".to_string(),
            account: "Bob".to_string(),
            ttfb_ms: 150,
            total_ms: 350,
            tokens: 80,
            output_tokens: 0,
            thinking_tokens: 0,
            status: 200,
            failover: true,
        },
        false,
    );

    // Assert the new live event arrives over the same SSE stream
    let live_event = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(frame_res) = body.frame().await {
            if let Ok(frame) = frame_res
                && let Some(chunk) = frame.data_ref()
            {
                let s = String::from_utf8_lossy(chunk);
                if s.contains("event: audit") && s.contains("gemini-2.5-pro") {
                    return s.to_string();
                }
            }
        }
        String::new()
    })
    .await
    .expect("timed out waiting for live audit event");

    assert!(live_event.contains("id: 2\nevent: audit\ndata: "));
    assert!(live_event.contains("gemini-2.5-pro"));

    // Fire 16 concurrent live events; out-of-order broadcast arrivals must NOT drop lower IDs
    let mut handles = Vec::new();
    for i in 0..16 {
        handles.push(tokio::spawn(async move {
            agy_pool::dashboard::telemetry::record_attempt(
                agy_pool::dashboard::telemetry::DashboardAuditEvent {
                    id: 0,
                    timestamp: jiff::Timestamp::now(),
                    model: format!("concurrent-model-{i}"),
                    account: "ConcAcc".to_string(),
                    ttfb_ms: 10,
                    total_ms: 20,
                    tokens: 1,
                    output_tokens: 0,
                    thinking_tokens: 0,
                    status: 200,
                    failover: false,
                },
                false,
            );
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let mut seen_ids = std::collections::HashSet::new();
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while seen_ids.len() < 16 {
            let Some(frame_res) = body.frame().await else {
                break;
            };
            if let Ok(frame) = frame_res
                && let Some(chunk) = frame.data_ref()
            {
                buf.push_str(&String::from_utf8_lossy(chunk));
                while let Some(end_idx) = buf.find("\n\n") {
                    let block = buf[..end_idx].to_string();
                    buf.drain(..end_idx + 2);
                    if block.contains("event: audit") && block.contains("concurrent-model-") {
                        for line in block.lines() {
                            if let Some(id_str) = line.strip_prefix("id: ")
                                && let Ok(id) = id_str.trim().parse::<u64>()
                            {
                                seen_ids.insert(id);
                            }
                        }
                    }
                }
            }
        }
    })
    .await;
    assert_eq!(
        seen_ids.len(),
        16,
        "expected all 16 concurrent SSE live events without drops, got {:?}",
        seen_ids
    );

    // Drop client stream/sender and verify clean teardown without panic
    drop(body);
    drop(sender);
    let _ = tokio::time::timeout(Duration::from_millis(500), conn_task).await;

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_embedded_spa_asset_constraints() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, headers, body) = send_request(addr, Method::GET, "/dashboard", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(hyper::header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("text/html; charset=utf-8"));

    let body_str = String::from_utf8(body.to_vec()).expect("valid UTF-8 HTML body");

    // Assert body length > 2048 and < 90_000 bytes
    assert!(
        body_str.len() > 2048 && body_str.len() < 90_000,
        "body length {} not in range (2048, 90000)",
        body_str.len()
    );

    // Assert body contains no external CDN URLs in <script or <link tags
    let body_lower = body_str.to_lowercase();
    for tag_start in ["<script", "<link"] {
        let mut rest = body_lower.as_str();
        while let Some(idx) = rest.find(tag_start) {
            let after = &rest[idx..];
            if let Some(end_tag) = after.find('>') {
                let tag_content = &after[..=end_tag];
                assert!(
                    !tag_content.contains("https://") && !tag_content.contains("http://"),
                    "external URL found in tag: {tag_content}"
                );
                rest = &after[end_tag + 1..];
            } else {
                break;
            }
        }
    }

    // Assert body references all backend endpoints
    assert!(body_str.contains("/api/v1/status"), "missing /api/v1/status reference");
    assert!(
        body_str.contains("/api/v1/accounts"),
        "missing /api/v1/accounts reference"
    );
    assert!(
        body_str.contains("/api/v1/forecast"),
        "missing /api/v1/forecast reference"
    );
    assert!(body_str.contains("/api/v1/events"), "missing /api/v1/events reference");
    assert!(body_str.contains("/api/v1/tokens"), "missing /api/v1/tokens reference");
    assert!(body_str.contains("token-usage-body"), "missing token usage table");
    assert!(body_str.contains("data-token-range"), "missing token range buttons");
    assert!(body_str.contains("/api/v1/update"), "missing /api/v1/update reference");
    assert!(body_str.contains("update-banner"), "missing update-banner element");
    assert!(body_str.contains("upgrade-btn"), "missing upgrade-btn element");
    assert!(body_str.contains("version-btn"), "missing version-btn element");
    assert!(
        body_str.contains("version-update-badge"),
        "missing version-update-badge element"
    );

    // Assert body includes the email visibility toggle control (show_email)
    assert!(body_str.contains("show_email"), "missing show_email toggle control");

    // Assert body includes system/manual theme switching support
    assert!(body_str.contains("theme-switcher"), "missing theme-switcher control");
    assert!(
        body_str.contains("prefers-color-scheme: light"),
        "missing prefers-color-scheme: light detection"
    );
    assert!(
        body_str.contains(r#"data-theme="light""#),
        "missing light theme CSS override"
    );
    assert!(body_str.contains("agy_theme"), "missing agy_theme persistence key");
    // The theme must be applied in <head>, before the body paints, or light-mode users see a dark flash.
    let bootstrap = body_str
        .find("window.__applyAgyTheme(saved)")
        .expect("missing theme bootstrap");
    let head_end = body_str.find("</head>").expect("missing </head>");
    assert!(bootstrap < head_end, "theme bootstrap must run inside <head>");
    // Blocked storage (privacy mode, sandboxed iframe) throws; one unguarded access halts the whole script.
    for line in body_str.lines().filter(|l| l.contains("localStorage.")) {
        assert!(line.contains("try {"), "unguarded localStorage access: {}", line.trim());
    }
    assert!(
        body_str.contains("window.location.host"),
        "missing window.location.host endpoint pill formatting"
    );

    // Auth: the token stays per tab and goes out as a Bearer header; EventSource cannot send headers.
    for needle in [
        "agy_dashboard_token",
        "Authorization",
        "X-Agy-Pool-Control",
        "/api/v1/auth/status",
        "/api/v1/strategy",
        "token-card",
        "strategy-select",
        "getReader()",
    ] {
        assert!(body_str.contains(needle), "missing {needle}");
    }
    assert!(
        !body_str.contains("new EventSource("),
        "EventSource cannot send the Bearer token"
    );
    for line in body_str.lines().filter(|l| l.contains("sessionStorage.")) {
        assert!(
            line.contains("try {"),
            "unguarded sessionStorage access: {}",
            line.trim()
        );
    }

    // Account controls (Phase 2).
    for needle in [
        "/api/v1/accounts/refresh-all",
        "/api/v1/accounts/reorder",
        "data-act=",
        "refresh-all-btn",
        "verify-dialog",
        "toast",
        "Disable the last enabled account?",
    ] {
        assert!(body_str.contains(needle), "missing {needle}");
    }
    assert!(!body_str.contains("alert("), "use the toast instead of alert()");

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_host_validation_accepts_multi_listen_runtime_meta() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();

    agy_pool::dashboard::dto::set_runtime_meta(
        "127.0.0.1:8890, custom-gw.internal:8890".to_string(),
        jiff::Timestamp::now(),
    );

    let (addr, server_task, shutdown_tx) = start_server().await;

    let mut allowed = HeaderMap::new();
    allowed.insert(hyper::header::HOST, "custom-gw.internal:8890".parse().unwrap());
    let (status, _, body) = send_request(addr, Method::GET, "/api/v1/status", allowed, Bytes::new()).await;
    assert_eq!(status, StatusCode::OK, "expected multi-listen host entry to be allowed");
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["listen"], "127.0.0.1:8890, custom-gw.internal:8890");

    let mut denied = HeaderMap::new();
    denied.insert(hyper::header::HOST, "evil.internal:8890".parse().unwrap());
    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/status", denied, Bytes::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trailing_slash_and_unknown_api_paths() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let mut pool = agy_pool::storage::load_pool().unwrap();
    pool.show_email = Some(true);
    agy_pool::storage::save_pool(&pool).unwrap();
    let (addr, server_task, shutdown_tx) = start_server().await;

    // A trailing slash must not drop the query string.
    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts/?show_email=true",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["accounts"][0]["email"], "alice@example.com");

    // Unknown /api/v1/* paths are answered locally (JSON 404), never proxied upstream.
    for (method, path) in [(Method::GET, "/api/v1/nope"), (Method::POST, "/api/v1/nope/deeper")] {
        let (status, headers, body) = send_request(addr, method, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "NOT_FOUND", "{path}");
    }

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reads_without_token_stay_open() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/status", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) =
        send_request(addr, Method::GET, "/api/v1/auth/status", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"auth_required": false, "authenticated": true, "control_enabled": false})
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reads_require_token_when_configured() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    // The HTML shell carries no account data and stays reachable so the browser can ask for the token.
    let (status, _, _) = send_request(addr, Method::GET, "/dashboard", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);

    for path in [
        "/api/v1/status",
        "/api/v1/accounts",
        "/api/v1/forecast",
        "/api/v1/events",
    ] {
        let (status, headers, body) = send_request(addr, Method::GET, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            headers.get(hyper::header::WWW_AUTHENTICATE).unwrap(),
            "Bearer",
            "{path}"
        );
        assert_eq!(error_code(&body), "UNAUTHORIZED", "{path}");
    }
    for path in ["/api/v1/status", "/api/v1/accounts", "/api/v1/forecast"] {
        let (status, _, _) = send_request(
            addr,
            Method::GET,
            path,
            hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
            Bytes::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let (_, _, body) = send_request(addr, Method::GET, "/api/v1/auth/status", HeaderMap::new(), Bytes::new()).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"auth_required": true, "authenticated": false, "control_enabled": true})
    );
    let (_, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/auth/status",
        hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
        Bytes::new(),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["authenticated"], true);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_strategy_control_matrix() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;
    let auth = bearer(TEST_TOKEN);
    let good = || {
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json"),
            ("authorization", auth.as_str()),
        ])
    };
    let body = r#"{"strategy":"max_quota"}"#;
    let strategy = || agy_pool::storage::load_pool().unwrap().strategy;

    // No token configured: controls stay off even from loopback.
    let (status, _, resp) = post_strategy(addr, good(), body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&resp), "TOKEN_REQUIRED_FOR_CONTROL");
    assert_eq!(strategy(), "smart");

    set_dashboard_config(Some(TEST_TOKEN), false);
    let (status, _, resp) = post_strategy(
        addr,
        hdrs(&[("content-type", "application/json"), ("authorization", auth.as_str())]),
        body,
    )
    .await;
    assert_eq!(
        (status, error_code(&resp).as_str()),
        (StatusCode::FORBIDDEN, "CONTROL_HEADER_REQUIRED")
    );
    let (status, _, resp) = post_strategy(
        addr,
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "text/plain"),
            ("authorization", auth.as_str()),
        ]),
        body,
    )
    .await;
    assert_eq!(
        (status, error_code(&resp).as_str()),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON_REQUIRED")
    );
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[("x-agy-pool-control", "1"), ("content-type", "application/json")]),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json"),
            ("authorization", "Bearer wrong"),
        ]),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Cross-site browsers are stopped by the Host/Origin guard before auth.
    let mut cross_site = good();
    cross_site.insert(hyper::header::ORIGIN, "http://evil.example.com".parse().unwrap());
    let (status, _, _) = post_strategy(addr, cross_site, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(strategy(), "smart");

    for bad in [
        r#"{"strategy":"round_robin"}"#,
        r#"{"strategy":"smart","x":1}"#,
        "not json",
    ] {
        let (status, _, resp) = post_strategy(addr, good(), bad).await;
        assert_eq!(
            (status, error_code(&resp).as_str()),
            (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT"),
            "{bad}"
        );
    }

    let (status, headers, resp) = post_strategy(addr, good(), body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    assert_eq!(json, serde_json::json!({"ok": true, "strategy": "max_quota"}));
    assert_eq!(strategy(), "max_quota");
    let (_, _, resp) = send_request(
        addr,
        Method::GET,
        "/api/v1/status",
        hdrs(&[("authorization", auth.as_str())]),
        Bytes::new(),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    assert_eq!(json["strategy"], "max_quota");

    let (status, headers, _) =
        send_request(addr, Method::GET, "/api/v1/strategy", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(hyper::header::ALLOW).unwrap(), "POST");

    // Explicit opt-out: no token, controls allowed.
    set_dashboard_config(None, true);
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json; charset=utf-8"),
        ]),
        r#"{"strategy":"smart"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(strategy(), "smart");

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_auth_failures_are_throttled() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;
    let get = |path: &'static str, token: Option<&'static str>| {
        let headers = match token {
            Some(t) => hdrs(&[("authorization", bearer(t).as_str())]),
            None => HeaderMap::new(),
        };
        send_request(addr, Method::GET, path, headers, Bytes::new())
    };

    // Missing tokens (a fresh page load) never count.
    for _ in 0..10 {
        assert_eq!(get("/api/v1/status", None).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(get("/api/v1/status", Some(TEST_TOKEN)).await.0, StatusCode::OK);

    // Five wrong tokens lock this client out, even with the right token afterwards.
    for _ in 0..5 {
        assert_eq!(
            get("/api/v1/status", Some("wrong-token")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, headers, body) = get("/api/v1/status", Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get(hyper::header::RETRY_AFTER).unwrap(), "60");
    assert_eq!(error_code(&body), "TOO_MANY_AUTH_FAILURES");

    // /auth/status is not a free guessing oracle.
    agy_pool::dashboard::auth::reset_auth_failures();
    for _ in 0..5 {
        assert_eq!(get("/api/v1/auth/status", Some("wrong-token")).await.0, StatusCode::OK);
    }
    assert_eq!(
        get("/api/v1/auth/status", Some(TEST_TOKEN)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        get("/api/v1/status", Some(TEST_TOKEN)).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

fn control_headers() -> HeaderMap {
    hdrs(&[
        ("x-agy-pool-control", "1"),
        ("content-type", "application/json"),
        ("authorization", bearer(TEST_TOKEN).as_str()),
    ])
}

async fn post_control(addr: SocketAddr, path: &str, body: &str) -> (StatusCode, Bytes) {
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        path,
        control_headers(),
        Bytes::copy_from_slice(body.as_bytes()),
    )
    .await;
    (status, body)
}

fn stored_account(id: &str) -> agy_pool::storage::Account {
    agy_pool::storage::load_pool()
        .unwrap()
        .accounts
        .into_iter()
        .find(|a| a.id == id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_account_mutations() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    // Gate: no control header / no token are refused before anything changes.
    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/accounts/acc_1/disable",
        hdrs(&[("content-type", "application/json")]),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, headers, _) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts/acc_1/disable",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(hyper::header::ALLOW).unwrap(), "POST");
    assert_eq!(stored_account("acc_1").status, "");

    // Disable / enable.
    assert_eq!(
        post_control(addr, "/api/v1/accounts/acc_1/disable", "{}").await.0,
        StatusCode::OK
    );
    assert_eq!(stored_account("acc_1").status, agy_pool::storage::STATUS_DISABLED);
    assert_eq!(
        post_control(addr, "/api/v1/accounts/acc_1/enable", "{}").await.0,
        StatusCode::OK
    );
    assert_eq!(stored_account("acc_1").status, "");

    // Unknown id and unknown action.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/disable", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND")
    );
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/explode", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::NOT_FOUND, "NOT_FOUND")
    );
    // A list position or an email is not an id.
    let (status, _) = post_control(addr, "/api/v1/accounts/1/disable", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Rename trims and validates.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/rename", r#"{"name":"  Work  "}"#).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(stored_account("acc_2").name, "Work");
    let long = format!(r#"{{"name":"{}"}}"#, "a".repeat(65));
    for bad in [
        r#"{"name":"   "}"#,
        long.as_str(),
        r#"{"name":"a\nb"}"#,
        r#"{"title":"x"}"#,
        "nope",
    ] {
        let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/rename", bad).await;
        assert_eq!(
            (status, error_code(&body).as_str()),
            (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT"),
            "{bad}"
        );
    }
    assert_eq!(stored_account("acc_2").name, "Work");

    // Reorder: a full permutation is applied; anything else means the page is stale.
    let ids = || -> Vec<String> {
        agy_pool::storage::load_pool()
            .unwrap()
            .accounts
            .into_iter()
            .map(|a| a.id)
            .collect()
    };
    assert_eq!(
        post_control(addr, "/api/v1/accounts/reorder", r#"{"ordered_ids":["acc_2","acc_1"]}"#)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(ids(), vec!["acc_2", "acc_1"]);
    for stale in [
        r#"{"ordered_ids":["acc_1"]}"#,
        r#"{"ordered_ids":["acc_1","acc_1"]}"#,
        r#"{"ordered_ids":["acc_1","acc_3"]}"#,
        r#"{"ordered_ids":["acc_1","acc_2","acc_3"]}"#,
    ] {
        let (status, body) = post_control(addr, "/api/v1/accounts/reorder", stale).await;
        assert_eq!(
            (status, error_code(&body).as_str()),
            (StatusCode::CONFLICT, "POOL_CHANGED"),
            "{stale}"
        );
    }
    assert_eq!(ids(), vec!["acc_2", "acc_1"]);

    // Without a configured token, controls stay off.
    set_dashboard_config(None, false);
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/disable", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::FORBIDDEN, "TOKEN_REQUIRED_FOR_CONTROL")
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_switch_account_by_exact_id() {
    let _guard = lock_test().await;
    let tmp = setup_test_env();
    let native = tmp.path().join("native_gemini");
    std::fs::create_dir_all(&native).unwrap();
    agy_pool::config::set_native_agy_dir(&native);
    let expiry = Some(agy_pool::clock::now_unix() + 3600.0);
    let pool = agy_pool::storage::Pool {
        accounts: vec![
            // Named like the other account's id: an exact id must still win.
            agy_pool::storage::Account {
                id: "acc_1".to_string(),
                name: "acc_2".to_string(),
                access_token: "tok1".to_string(),
                token_expiry: expiry,
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_2".to_string(),
                access_token: "tok2".to_string(),
                token_expiry: expiry,
                ..Default::default()
            },
        ],
        active_account_id: Some("acc_1".to_string()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    agy_pool::storage::save_pool(&pool).unwrap();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/switch", "{}").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        agy_pool::storage::load_pool().unwrap().active_account_id.as_deref(),
        Some("acc_2")
    );
    let (_, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts",
        hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
        Bytes::new(),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["accounts"][0]["is_cli_base"], false);
    assert_eq!(json["accounts"][1]["is_cli_base"], true);

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/switch", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND")
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_quota_refresh_controls() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    let pool = agy_pool::storage::Pool {
        accounts: vec![
            agy_pool::storage::Account {
                id: "acc_1".to_string(),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_2".to_string(),
                status: agy_pool::storage::STATUS_DISABLED.to_string(),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_3".to_string(),
                status: agy_pool::storage::STATUS_VALIDATION_REQUIRED.to_string(),
                validation_url: Some("https://accounts.google.com/signin/continue?x=1".to_string()),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_4".to_string(),
                status: agy_pool::storage::STATUS_VALIDATION_REQUIRED.to_string(),
                validation_url: Some("javascript:alert(1)".to_string()),
                ..Default::default()
            },
        ],
        strategy: "smart".to_string(),
        ..Default::default()
    };
    agy_pool::storage::save_pool(&pool).unwrap();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/refresh", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::CONFLICT, "ACCOUNT_DISABLED")
    );
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/refresh", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND")
    );

    // Test mode has no upstream: query_quota fails fast and the error text stays out of the response.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/refresh", "{}").await;
    assert_eq!(
        (status, error_code(&body).as_str()),
        (StatusCode::BAD_GATEWAY, "QUOTA_REFRESH_FAILED")
    );
    assert!(!String::from_utf8_lossy(&body).contains("TEST GUARD"));

    // A still-restricted account reports its https verification link; other schemes are dropped.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_3/refresh", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "validation_required");
    assert_eq!(
        json["validation_url"],
        "https://accounts.google.com/signin/continue?x=1"
    );
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_4/refresh", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["validation_url"], serde_json::Value::Null);

    // Refresh all skips disabled accounts.
    let (status, body) = post_control(addr, "/api/v1/accounts/refresh-all", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(
        json["refreshed"].as_u64().unwrap() + json["failed"].as_u64().unwrap(),
        3
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dashboard_update_endpoints() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    agy_pool::cli::update::clear_update_cache();

    struct ResetUpdate;
    impl Drop for ResetUpdate {
        fn drop(&mut self) {
            agy_pool::cli::update::set_releases_api(None);
            agy_pool::cli::update::set_update_target_path(None);
            agy_pool::cli::update::set_dpkg_file_list(None);
            agy_pool::cli::update::clear_update_cache();
        }
    }
    let _reset = ResetUpdate;

    let bin_name = format!(
        "agy-pool-{}-{}",
        agy_pool::cli::update::current_go_os(),
        agy_pool::cli::update::current_go_arch()
    );
    let new_bin = b"new dashboard binary".to_vec();
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&new_bin);
    let sum_hex = hex::encode(hasher.finalize());
    let sums = format!("{sum_hex}  {bin_name}\n");
    let tag = "v9.9.9".to_string();

    struct MockState {
        tag: String,
        bin_name: String,
        new_bin: Vec<u8>,
        sums: String,
    }
    let state = Arc::new(std::sync::Mutex::new(MockState {
        tag,
        bin_name: bin_name.clone(),
        new_bin,
        sums,
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mock_url = format!("http://127.0.0.1:{port}");
    let mock_state = state.clone();
    let mock_server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let state = mock_state.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let n = match socket.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => return,
                };
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let first_line = req_str.lines().next().unwrap_or("");
                let path = first_line.split_whitespace().nth(1).unwrap_or("");

                let (tag, bin_name, new_bin, sums) = {
                    let st = state.lock().unwrap();
                    (st.tag.clone(), st.bin_name.clone(), st.new_bin.clone(), st.sums.clone())
                };

                if path.contains("/releases") {
                    let deb = format!(
                        "agy-pool_{}_{}.deb",
                        tag.trim_start_matches('v'),
                        agy_pool::cli::update::current_go_arch()
                    );
                    let body = format!(
                        r#"[{{"tag_name":"{tag}","assets":[{{"name":"{bin_name}","url":"http://127.0.0.1:{port}/bin"}},{{"name":"SHA256SUMS","url":"http://127.0.0.1:{port}/sums"}},{{"name":"{deb}","url":"http://127.0.0.1:{port}/deb"}}]}}]"#
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if path.contains("/sums") {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sums.len(),
                        sums
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                } else if path.contains("/bin") {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        new_bin.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&new_bin).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            });
        }
    });

    agy_pool::cli::update::set_releases_api(Some(format!("{mock_url}/releases")));

    static DASHBOARD_TEST_BIN: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
    fn test_bin_provider() -> std::path::PathBuf {
        let lock = DASHBOARD_TEST_BIN.lock().unwrap();
        lock.clone().expect("DASHBOARD_TEST_BIN must be set")
    }

    let temp_target_dir = tempfile::tempdir().unwrap();
    let target_bin = temp_target_dir.path().join("agy-pool");
    std::fs::write(&target_bin, b"old binary").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&target_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    *DASHBOARD_TEST_BIN.lock().unwrap() = Some(target_bin.clone());
    agy_pool::cli::update::set_update_target_path(Some(test_bin_provider));
    // Never consult the host's real /var/lib/dpkg list.
    let dpkg_list = temp_target_dir.path().join("agy-pool.list");
    agy_pool::cli::update::set_dpkg_file_list(Some(dpkg_list.clone()));

    // Dashboard configured with auth token
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    // 1. GET /api/v1/update without auth token -> 401 UNAUTHORIZED
    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/update", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 2. GET /api/v1/update with auth token -> 200 OK with update details
    let mut authed_headers = HeaderMap::new();
    authed_headers.insert(hyper::header::AUTHORIZATION, bearer(TEST_TOKEN).parse().unwrap());
    let (status, headers, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/update",
        authed_headers.clone(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json from GET /api/v1/update");
    assert_eq!(json["has_update"], true);
    assert_eq!(json["latest_version"], "9.9.9");
    assert_eq!(json["current_version"], agy_pool::VERSION);
    assert_eq!(json["package_managed"], false);
    assert!(json["deb_package_name"].is_null());

    // 3. POST /api/v1/update controls gating:
    // a) Missing X-Agy-Pool-Control header -> 403 Forbidden
    let mut post_no_control = authed_headers.clone();
    post_no_control.insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/update",
        post_no_control,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "CONTROL_HEADER_REQUIRED");

    // b) Missing Content-Type: application/json -> 415 Unsupported Media Type
    let mut post_bad_ct = authed_headers.clone();
    post_bad_ct.insert("x-agy-pool-control", "1".parse().unwrap());
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/update",
        post_bad_ct,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(error_code(&body), "JSON_REQUIRED");

    // c) Missing Bearer token when required -> 401 Unauthorized
    let mut post_no_token = HeaderMap::new();
    post_no_token.insert("x-agy-pool-control", "1".parse().unwrap());
    post_no_token.insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/update",
        post_no_token,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let mut valid_post = authed_headers.clone();
    valid_post.insert("x-agy-pool-control", "1".parse().unwrap());
    valid_post.insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());

    // d) Binary owned by the Debian package: the check says so, and the apply error carries the apt command.
    std::fs::write(&dpkg_list, format!("/.\n{}\n", target_bin.display())).unwrap();
    agy_pool::cli::update::clear_update_cache();
    let deb = format!("agy-pool_9.9.9_{}.deb", agy_pool::cli::update::current_go_arch());
    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/update?refresh=true",
        authed_headers.clone(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["package_managed"], true);
    assert_eq!(json["deb_package_name"], deb.as_str());
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/update",
        valid_post.clone(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error_code(&body), "UPDATE_APPLY_FAILED");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let msg = json["error"]["message"].as_str().unwrap();
    assert!(msg.contains("Debian package"), "{msg}");
    assert!(msg.contains(&format!("sudo apt install ./{deb}")), "{msg}");
    assert_eq!(std::fs::read(&target_bin).unwrap(), b"old binary");
    std::fs::remove_file(&dpkg_list).unwrap();
    agy_pool::cli::update::clear_update_cache();

    // e) Valid POST with all headers -> 200 OK and target binary is replaced!
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/update",
        valid_post,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json from POST /api/v1/update");
    assert_eq!(json["ok"], true);
    assert_eq!(json["version"], "9.9.9");

    // Verify file content on disk
    let replaced_bin = std::fs::read(&target_bin).unwrap();
    assert_eq!(replaced_bin, b"new dashboard binary");

    // 4. Method not allowed for PUT/DELETE on /api/v1/update -> 405 Method Not Allowed with Allow: GET, POST
    let (status, headers, _) =
        send_request(addr, Method::DELETE, "/api/v1/update", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(hyper::header::ALLOW).unwrap(), "GET, POST");

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
    mock_server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tokens_endpoint() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    agy_pool::storage::tokens::clear_pending();
    let now = agy_pool::clock::now_unix() as i64;
    agy_pool::storage::tokens::record_token_usage_at("acc_1", 1000, 400, 200, 100, false, now);
    // cached not reported (-1) and an aborted stream
    agy_pool::storage::tokens::record_token_usage_at("acc_2", 500, -1, 100, 0, true, now);
    // three days ago: only in 7d and total
    agy_pool::storage::tokens::record_token_usage_at("acc_1", 7000, 0, 0, 0, false, now - 3 * 86400);
    agy_pool::storage::tokens::flush_token_usage().unwrap();

    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/tokens", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "reads need the token");

    let auth = bearer(TEST_TOKEN);
    let get = |path: &'static str| {
        let headers = hdrs(&[("authorization", auth.as_str())]);
        async move { send_request(addr, Method::GET, path, headers, Bytes::new()).await }
    };

    // Default range is 24h.
    let (status, headers, body) = get("/api/v1/tokens").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["range"], "24h");
    assert!(json["window_start"].as_i64().unwrap() <= now);
    let summary = &json["summary"];
    assert_eq!(summary["prompt_tokens"], 1500);
    assert_eq!(summary["cached_tokens"], 400);
    assert_eq!(summary["output_tokens"], 300);
    assert_eq!(summary["thinking_tokens"], 100);
    assert_eq!(summary["total_tokens"], 1900, "cached is part of prompt");
    assert_eq!(summary["request_count"], 2);
    assert_eq!(summary["aborted_count"], 1);
    let accounts = json["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["account_id"], "acc_1", "sorted by total, largest first");
    assert_eq!(accounts[0]["total_tokens"], 1300);
    assert!((accounts[0]["pool_share"].as_f64().unwrap() - 1300.0 / 1900.0).abs() < 1e-9);
    assert_eq!(accounts[1]["cached_tokens"], 0);
    assert!(
        !String::from_utf8_lossy(&body).contains("@example.com"),
        "never exposes emails"
    );

    let (_, _, body) = get("/api/v1/tokens?range=7d").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["summary"]["prompt_tokens"], 8500);

    let (_, _, body) = get("/api/v1/tokens?range=total").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["window_start"].is_null());

    let (status, _, body) = get("/api/v1/tokens?range=2d").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "INVALID_ARGUMENT");

    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/tokens",
        hdrs(&[("authorization", auth.as_str())]),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    agy_pool::storage::tokens::clear_pending();
    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
