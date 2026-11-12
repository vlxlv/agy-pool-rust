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
        agy_pool::config::reset_data_dir();
        agy_pool::config::set_test_mode(false);
    }
}

async fn lock_test() -> DashboardTestGuard<'static> {
    let async_guard = DASHBOARD_TEST_LOCK.lock().await;
    let sync_guard = agy_pool::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    agy_pool::config::set_test_mode(true);
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
                    if let Ok((stream, _)) = accept_res {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let h = handler.clone();
                        let service = hyper::service::service_fn(move |req| {
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

    // Assert body references all four backend endpoints
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
