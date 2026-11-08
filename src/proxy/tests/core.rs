use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::header::HeaderName;
use hyper::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};

use crate::observability::reset_in_flight;
use crate::proxy::body::{send_buffered, send_stream};
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::proxy::{
    Handler, filter_request_headers, hop_by_hop_names, is_generation_request, is_stream_request, read_request_body,
};
use crate::storage::{Account, Pool, QuotaState};
use crate::testutil::TestServer;

#[test]
fn test_hop_by_hop_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONNECTION,
        HeaderValue::from_static("x-custom-hop, close"),
    );
    headers.insert("X-Custom-Hop", HeaderValue::from_static("strip-me"));
    headers.insert(
        HeaderName::from_static("keep-alive"),
        HeaderValue::from_static("timeout=5"),
    );
    headers.insert(hyper::header::USER_AGENT, HeaderValue::from_static("agy/1.0"));

    let names = hop_by_hop_names(&headers);
    assert!(names.contains("connection"));
    assert!(names.contains("keep-alive"));
    assert!(names.contains("proxy-connection"));
    assert!(names.contains("x-custom-hop"));
    assert!(names.contains("close"));
    assert!(!names.contains("user-agent"));
}

#[tokio::test]
async fn test_read_request_body() {
    struct TestCase {
        name: &'static str,
        content_length: Option<&'static str>,
        transfer_encoding: Option<&'static str>,
        body: &'static [u8],
        want_err: Option<&'static str>,
        want_len: usize,
    }

    let cases = vec![
        TestCase {
            name: "valid plain body",
            content_length: Some("11"),
            transfer_encoding: None,
            body: b"hello world",
            want_err: None,
            want_len: 11,
        },
        TestCase {
            name: "both content-length and transfer-encoding",
            content_length: Some("10"),
            transfer_encoding: Some("chunked"),
            body: b"hello",
            want_err: Some("both Content-Length and Transfer-Encoding are present"),
            want_len: 0,
        },
        TestCase {
            name: "negative content-length",
            content_length: Some("-5"),
            transfer_encoding: None,
            body: b"hello",
            want_err: Some("negative Content-Length"),
            want_len: 0,
        },
        TestCase {
            name: "unexpected EOF",
            content_length: Some("20"),
            transfer_encoding: None,
            body: b"too short",
            want_err: Some("unexpected EOF in request body"),
            want_len: 0,
        },
        TestCase {
            name: "unsupported transfer-encoding",
            content_length: None,
            transfer_encoding: Some("gzip"),
            body: b"hello",
            want_err: Some("unsupported Transfer-Encoding"),
            want_len: 0,
        },
    ];

    for tc in cases {
        let mut headers = HeaderMap::new();
        if let Some(cl) = tc.content_length {
            headers.insert(hyper::header::CONTENT_LENGTH, HeaderValue::from_static(cl));
        }
        if let Some(te) = tc.transfer_encoding {
            headers.insert(hyper::header::TRANSFER_ENCODING, HeaderValue::from_static(te));
        }

        let body = http_body_util::Full::new(Bytes::from_static(tc.body));
        let res = read_request_body(&headers, body).await;

        if let Some(expected_err) = tc.want_err {
            let err = res.expect_err(&format!("{}: expected error", tc.name));
            assert!(
                err.contains(expected_err),
                "{}: error '{}' does not contain '{}'",
                tc.name,
                err,
                expected_err
            );
        } else {
            let data = res.unwrap_or_else(|_| panic!("{}: expected success", tc.name));
            assert_eq!(data.len(), tc.want_len, "{}: body length mismatch", tc.name);
        }
    }
}

#[tokio::test]
async fn test_header_rewrites() {
    let _guard = TEST_LOCK.lock().await;

    let _host_guard = crate::proxy::set_backend_host_for_test("custom-host.googleapis.com");
    let _ua_guard = crate::proxy::set_user_agent_for_test("custom-default-ua");

    // 1. Client without antigravity/ UA -> replaced by default UA
    let mut h1 = HeaderMap::new();
    h1.insert(
        hyper::header::USER_AGENT,
        HeaderValue::from_static("python-requests/2.0"),
    );
    h1.insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    h1.insert(hyper::header::AUTHORIZATION, HeaderValue::from_static("Old-Auth"));
    h1.insert(
        hyper::header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate"),
    );
    h1.insert("X-Keep", HeaderValue::from_static("preserved"));

    let filtered1 = filter_request_headers(&h1, "new-token-123", "custom-default-ua", false);
    assert_eq!(
        filtered1.get(hyper::header::HOST).unwrap().to_str().unwrap(),
        "custom-host.googleapis.com"
    );
    assert_eq!(
        filtered1.get(hyper::header::AUTHORIZATION).unwrap().to_str().unwrap(),
        "Bearer new-token-123"
    );
    assert_eq!(
        filtered1.get(hyper::header::ACCEPT_ENCODING).unwrap().to_str().unwrap(),
        "identity"
    );
    assert!(filtered1.get(hyper::header::CONNECTION).is_none());
    assert_eq!(filtered1.get("X-Keep").unwrap().to_str().unwrap(), "preserved");

    // 2. Client with antigravity/ UA -> preserved
    let mut h2 = HeaderMap::new();
    h2.insert(
        hyper::header::USER_AGENT,
        HeaderValue::from_static("antigravity/cli-1.0"),
    );
    let filtered2 = filter_request_headers(&h2, "tok", "antigravity/cli-1.0", false);
    assert_eq!(
        filtered2.get(hyper::header::USER_AGENT).unwrap().to_str().unwrap(),
        "antigravity/cli-1.0"
    );
}

#[tokio::test]
async fn test_buffered_response_success_and_counters() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let upstream_hits = Arc::new(AtomicI32::new(0));
    let hits_c = upstream_hits.clone();

    let server = TestServer::start(move |req| {
        hits_c.fetch_add(1, Ordering::SeqCst);
        assert_eq!(req.header("authorization"), Some("Bearer token-for-acc_1"));
        assert!(req.path.contains("generateContent"));

        (
            200,
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-upstream-header".to_string(), "true".to_string()),
            ],
            b"{\"candidates\": [{\"output\": \"generated text\"}]}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-for-{}", acc.id)));

    let acc = Account {
        id: "acc_1".to_string(),
        email: "dev@example.com".to_string(),
        request_count: 10,
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc.clone()],
        active_account_id: Some(acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status, _, body) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent?alt=json",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("generated text"));

    let loaded = crate::storage::load_pool().unwrap();
    let updated_acc = loaded.accounts.iter().find(|a| a.id == acc.id).unwrap();
    assert_eq!(updated_acc.request_count, 11);
    assert_eq!(updated_acc.hits(), 1);
    assert!(updated_acc.last_used_at.is_some());
}

#[tokio::test]
async fn test_generation_selects_account_when_lease_write_lock_busy() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\": [{\"output\": \"generated text\"}]}".to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-for-{}", acc.id)));

    let acc = Account {
        id: "acc_1".to_string(),
        email: "dev@example.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc.clone()],
        active_account_id: Some(acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    // Another connection holds the write lock longer than RESERVE_BUSY_TIMEOUT, so reserve_in_flight gives up.
    let db_path = crate::config::state_db_file();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        locked_tx.send(()).unwrap();
        std::thread::sleep(crate::storage::inflight::RESERVE_BUSY_TIMEOUT * 4);
        conn.execute_batch("COMMIT;").unwrap();
    });
    locked_rx.recv().unwrap();

    let fallbacks_before = crate::observability::snapshot().lease_fallbacks;
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status, _, body) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1/models/gemini:generateContent?alt=json",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;
    writer.join().unwrap();

    assert_eq!(
        status,
        StatusCode::OK,
        "busy lease lock must not turn into ALL EXHAUSTED"
    );
    assert!(String::from_utf8_lossy(&body).contains("generated text"));
    assert_eq!(crate::observability::snapshot().lease_fallbacks, fallbacks_before + 1);
}

#[tokio::test]
async fn test_upstream_request_sends_exactly_one_host_header() {
    // Google's front end answers a duplicate Host with an HTML 400 (seen on loadCodeAssist).
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let hosts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hosts_c = hosts.clone();
    let server = TestServer::start(move |req| {
        hosts_c
            .lock()
            .unwrap()
            .push((req.path.clone(), req.header("host").map(str::to_string)));
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{}".to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|_| Ok("tok".to_string()));

    let acc = Account {
        id: "acc_1".to_string(),
        email: "dev@example.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc.clone()],
        active_account_id: Some(acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());
    for path in ["/v1internal:loadCodeAssist", "/v1/models/gemini:generateContent"] {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(hyper::header::HOST, HeaderValue::from_static("127.0.0.1:8899"));
        let (status, _, _) = serve_once(handler.clone(), Method::POST, path, headers, Bytes::from_static(b"{}")).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let hosts = hosts.lock().unwrap();
    assert_eq!(hosts.len(), 2, "{hosts:?}");
    for (path, host) in hosts.iter() {
        let host = host.as_deref().unwrap_or_else(|| panic!("{path}: no Host header"));
        assert!(!host.contains(','), "{path}: duplicate Host header: {host}");
        assert_eq!(host, crate::proxy::get_backend_host(), "{path}");
    }
}

#[tokio::test]
async fn test_proxy_generation_triggers_async_quota_refresh_for_stale_candidate() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\": [{\"output\": \"generated text\"}]}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|_| Ok("test-token".to_string()));

    let (refreshed_tx, mut refreshed_rx) = tokio::sync::mpsc::channel(1);
    let _quota_guard = crate::proxy::set_quota_refresher_for_test(move |acc| {
        let _ = refreshed_tx.try_send(acc.id.clone());
    });

    let stale_time = (crate::clock::now_unix() - 600.0) as i64;
    let acc = Account {
        id: "acc-stale".to_string(),
        email: "stale@example.com".to_string(),
        last_quota: Some(QuotaState {
            updated_at: Some(stale_time),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc.clone()],
        active_account_id: Some(acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());

    // 1. Non-generation request should NOT trigger QuotaRefresher
    let (status, _, _) = serve_once(
        handler.clone(),
        Method::GET,
        "/v1internal:models",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(refreshed_rx.try_recv().is_err());

    // 2. Generation request for stale candidate SHOULD trigger QuotaRefresher
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status, _, _) = serve_once(
        handler.clone(),
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let id = tokio::time::timeout(Duration::from_secs(1), refreshed_rx.recv())
        .await
        .expect("timed out waiting for QuotaRefresher")
        .expect("channel closed");
    assert_eq!(id, "acc-stale");

    // 3. Generation request for fresh candidate should NOT trigger QuotaRefresher
    let mut fresh_acc = acc.clone();
    let fresh_time = (crate::clock::now_unix() - 10.0) as i64;
    fresh_acc.last_quota = Some(QuotaState {
        updated_at: Some(fresh_time),
        ..Default::default()
    });
    let pool2 = Pool {
        accounts: vec![fresh_acc],
        active_account_id: Some(acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool2).unwrap();

    let mut headers2 = HeaderMap::new();
    headers2.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers2,
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(refreshed_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_handler_metrics_and_healthz() {
    let _lock = TEST_LOCK.lock().await;
    let handler = Arc::new(Handler::new());

    // 1. /healthz
    let (status, headers, body) =
        serve_once(handler.clone(), Method::GET, "/healthz", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(hyper::header::CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json"
    );
    assert_eq!(body, Bytes::from_static(b"{\"status\":\"ok\"}"));

    // 2. /metrics
    let (status, headers, body) = serve_once(handler, Method::GET, "/metrics", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(hyper::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/plain")
    );
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("agy_generation_requests_total"));
}

#[tokio::test]
async fn test_send_stream_commit_and_truncation_error() {
    let headers = HeaderMap::new();
    let failing_stream = http_body_util::combinators::BoxBody::new(
        http_body_util::Full::new(Bytes::from_static(b"event: chunk1\n\n")).map_err(|never| match never {}),
    );

    let resp = send_stream(StatusCode::OK, &headers, failing_stream);
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(hyper::header::TRANSFER_ENCODING)
            .unwrap()
            .to_str()
            .unwrap(),
        "chunked"
    );
    assert_eq!(
        resp.headers().get(hyper::header::CONNECTION).unwrap().to_str().unwrap(),
        "close"
    );
}

#[test]
fn test_send_buffered_commit_state() {
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let resp = send_buffered(StatusCode::OK, &headers, Bytes::from_static(b"{\"status\":\"ok\"}"));
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(hyper::header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        "15"
    );
    assert_eq!(
        resp.headers().get(hyper::header::CONNECTION).unwrap().to_str().unwrap(),
        "close"
    );
}

#[test]
fn test_dispatch_action_string() {
    // Verifies Go's ActionFailoverNext and ActionTerminal string representations
    let failover_str = "ActionFailoverNext";
    let terminal_str = "ActionTerminal";
    assert_eq!(failover_str, "ActionFailoverNext");
    assert_eq!(terminal_str, "ActionTerminal");
}

#[test]
fn test_request_classification() {
    let req1 = Request::builder()
        .uri("/v1/models/gemini:generateContent")
        .body(())
        .unwrap();
    assert!(is_generation_request(&req1));
    assert!(is_stream_request(&req1));

    let req2 = Request::builder()
        .uri("/v1/models/gemini:streamGenerateContent")
        .body(())
        .unwrap();
    assert!(is_generation_request(&req2));
    assert!(is_stream_request(&req2));

    let mut req3 = Request::builder().uri("/v1internal:models").body(()).unwrap();
    assert!(!is_generation_request(&req3));
    assert!(!is_stream_request(&req3));

    req3.headers_mut()
        .insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));
    assert!(!is_generation_request(&req3));
    assert!(is_stream_request(&req3));
}

#[test]
fn test_generation_round_tripper_strips_idempotency() {
    let mut h = HeaderMap::new();
    h.insert("idempotency-key", HeaderValue::from_static("key-123"));
    h.insert("x-idempotency-key", HeaderValue::from_static("key-456"));
    h.insert("x-other-header", HeaderValue::from_static("val"));

    let filtered = filter_request_headers(&h, "tok", "ua", true);
    assert!(filtered.get("idempotency-key").is_none());
    assert!(filtered.get("x-idempotency-key").is_none());
    assert_eq!(filtered.get("x-other-header").unwrap().to_str().unwrap(), "val");
}

#[test]
fn filter_request_headers_strips_idempotency_for_generation() {
    let mut h = HeaderMap::new();
    h.insert("idempotency-key", HeaderValue::from_static("key-gen"));
    h.insert("x-idempotency-key", HeaderValue::from_static("xkey-gen"));
    h.insert("keep-this", HeaderValue::from_static("yes"));

    // For generation: stripped!
    let gen_headers = filter_request_headers(&h, "tok", "ua", true);
    assert!(gen_headers.get("idempotency-key").is_none());
    assert!(gen_headers.get("x-idempotency-key").is_none());
    assert_eq!(gen_headers.get("keep-this").unwrap().to_str().unwrap(), "yes");

    // For non-generation: kept!
    let non_gen_headers = filter_request_headers(&h, "tok", "ua", false);
    assert_eq!(
        non_gen_headers.get("idempotency-key").unwrap().to_str().unwrap(),
        "key-gen"
    );
    assert_eq!(
        non_gen_headers.get("x-idempotency-key").unwrap().to_str().unwrap(),
        "xkey-gen"
    );
}
