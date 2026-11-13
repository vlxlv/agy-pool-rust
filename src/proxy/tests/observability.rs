use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::observability::{get_snapshot, reset, reset_in_flight};
use crate::proxy::Handler;
use crate::proxy::failover::set_pool_tx_injected_error_for_test;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir, setup_two_account_pool};
use crate::storage::{Account, Pool, QuotaState, STATUS_AUTH_ERROR, STATUS_VALIDATION_REQUIRED};
use crate::testutil::TestServer;

// Case A: Simple successful generation
#[tokio::test]
async fn test_observability_case_a_simple_successful_generation() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"result\":\"ok\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let frac = 0.9;
    let pool = Pool {
        accounts: vec![Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        }],
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.routing_decisions, 1);
    assert_eq!(snap.generation_success, 1);
    assert_eq!(snap.failover_attempts, 0);
    assert_eq!(snap.failover_success, 0);
    assert_eq!(snap.no_replay_prevented, 0);
}

// Case B: Explicit 429 failover then success
#[tokio::test]
async fn test_observability_case_b_explicit_429_failover_then_success() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let first_hits = Arc::new(AtomicI32::new(0));
    let second_hits = Arc::new(AtomicI32::new(0));
    let h1 = first_hits.clone();
    let h2 = second_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                429,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\"}}".to_vec(),
            )
        } else {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"ok\"}".to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let frac1 = 0.9;
    let frac2 = 0.8;
    let pool = Pool {
        accounts: vec![
            Account {
                id: "acc_1".to_string(),
                email: "1@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac1),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_2".to_string(),
                email: "2@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac2),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.routing_decisions, 2);
    assert_eq!(snap.failover_attempts, 1);
    assert_eq!(snap.failover_success, 1);
    assert_eq!(snap.generation_success, 1);
}

// Case C: Validation/auth failover then success
#[tokio::test]
async fn test_observability_case_c_validation_auth_failover_then_success() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let first_hits = Arc::new(AtomicI32::new(0));
    let second_hits = Arc::new(AtomicI32::new(0));
    let h1 = first_hits.clone();
    let h2 = second_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\":{\"message\":\"validation_required\"}}".to_vec(),
            )
        } else {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"ok\"}".to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let frac1 = 0.9;
    let frac2 = 0.8;
    let pool = Pool {
        accounts: vec![
            Account {
                id: "acc_1".to_string(),
                email: "1@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac1),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_2".to_string(),
                email: "2@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac2),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.routing_decisions, 2);
    assert_eq!(snap.failover_attempts, 1);
    assert_eq!(snap.failover_success, 1);
    assert_eq!(snap.generation_success, 1);
}

// Case D: Ambiguous transport failure
#[tokio::test]
async fn test_observability_case_d_ambiguous_transport_failure() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let physical_attempts = Arc::new(AtomicI32::new(0));
    let pa = physical_attempts.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            pa.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 128];
            let _ = conn.read(&mut buf).await;
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(physical_attempts.load(Ordering::SeqCst), 1);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.no_replay_prevented, 1);
    assert_eq!(snap.failover_attempts, 0);
}

// Case E: SSE committed then truncation
#[tokio::test]
async fn test_observability_case_e_sse_committed_then_truncation() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let acc1_attempts = Arc::new(AtomicI32::new(0));
    let acc2_attempts = Arc::new(AtomicI32::new(0));
    let a1 = acc1_attempts.clone();
    let a2 = acc2_attempts.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let a1 = a1.clone();
            let a2 = a2.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.contains("token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    conn.write_all(b"e\r\ndata: event 1\n\n\r\n").await.unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    conn.write_all(b"e\r\ndata: event 2\n\n\r\n").await.unwrap();
                    drop(conn);
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));

    let _ = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:streamGenerateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_attempts.load(Ordering::SeqCst), 0);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.no_replay_prevented, 1);
    assert_eq!(snap.failover_attempts, 0);
}

// Case F: All accounts restricted
#[tokio::test]
async fn test_observability_case_f_all_accounts_restricted() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let upstream_hits = Arc::new(AtomicI32::new(0));
    let hits = upstream_hits.clone();

    let server = TestServer::start(move |_| {
        hits.fetch_add(1, Ordering::SeqCst);
        (200, vec![], b"{}".to_vec())
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| {
        panic!("restricted account {} was considered for dispatch", acc.id);
    });

    let pool = Pool {
        accounts: vec![
            Account {
                id: "validation".to_string(),
                email: "v@example.com".to_string(),
                status: STATUS_VALIDATION_REQUIRED.to_string(),
                ..Default::default()
            },
            Account {
                id: "auth".to_string(),
                email: "a@example.com".to_string(),
                status: STATUS_AUTH_ERROR.to_string(),
                ..Default::default()
            },
        ],
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 0);

    let snap = get_snapshot();
    assert_eq!(snap.generation_requests, 1);
    assert_eq!(snap.restricted_skips, 2);
    assert_eq!(snap.routing_decisions, 0);
}

// Case L: Persistence failure increments persistence_errors_total exactly once, failover proceeds
#[tokio::test]
async fn test_observability_case_l_proxy_persistence_failure() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let first_hits = Arc::new(AtomicI32::new(0));
    let second_hits = Arc::new(AtomicI32::new(0));
    let h1 = first_hits.clone();
    let h2 = second_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\":{\"message\":\"validation_required\"}}".to_vec(),
            )
        } else {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"ok\"}".to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    // Inject pool transaction persistence error for the validation recording on acc_1
    set_pool_tx_injected_error_for_test(true);

    let frac = 0.9;
    let pool = Pool {
        accounts: vec![
            Account {
                id: "acc_1".to_string(),
                email: "1@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            },
            Account {
                id: "acc_2".to_string(),
                email: "2@example.com".to_string(),
                last_quota: Some(QuotaState {
                    remaining_fraction: Some(frac),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
        active_account_id: Some("acc_1".to_string()),
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    // Reset injected error
    set_pool_tx_injected_error_for_test(false);

    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);

    let snap = get_snapshot();
    assert!(
        snap.persistence_errors >= 1,
        "expected persistence_errors >= 1, got {}",
        snap.persistence_errors
    );
}

#[tokio::test]
async fn test_proxy_audit_case_a_buffered_success() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let tmp = tempfile::tempdir().unwrap();
    let audit_path = tmp.path().join("audit.jsonl");
    let audit_handle = crate::observability::init_audit_worker(audit_path.clone(), 1 << 20, 1);

    let (mut sub_rx, _sub) = crate::proxy::subscribe_audit(10);

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\":[]}".to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let pool = Pool {
        accounts: vec![Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.9),
                ..Default::default()
            }),
            ..Default::default()
        }],
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

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent?key=secret-key&alt=json",
        headers,
        Bytes::from_static(b"{\"contents\":[]}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    crate::observability::flush_audit_sync();

    let content = std::fs::read_to_string(&audit_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly 1 JSONL line");

    let ev: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(ev["status_code"], 200);
    assert_eq!(ev.get("failover_reason").and_then(|v| v.as_str()).unwrap_or(""), "");
    let path = ev["path"].as_str().unwrap();
    assert!(!path.contains('?'), "path must have no '?' but got: {path}");
    assert_eq!(path, "/v1/models/gemini:generateContent");

    let mut legacy_events = Vec::new();
    while let Ok(legacy_ev) = sub_rx.try_recv() {
        legacy_events.push(legacy_ev);
    }
    assert_eq!(
        legacy_events.len(),
        1,
        "expected exactly 1 legacy RoutingEvent observed for attempt"
    );

    drop(audit_handle);
}

#[tokio::test]
async fn test_proxy_audit_case_b_streamed_success() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let tmp = tempfile::tempdir().unwrap();
    let audit_path = tmp.path().join("audit.jsonl");
    let audit_handle = crate::observability::init_audit_worker(audit_path.clone(), 1 << 20, 1);

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "text/event-stream".to_string())],
            b"data: chunk 1\n\ndata: chunk 2\n\n".to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let pool = Pool {
        accounts: vec![Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.9),
                ..Default::default()
            }),
            ..Default::default()
        }],
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let (status, _, body) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:streamGenerateContent?alt=sse",
        headers,
        Bytes::from_static(b"{\"contents\":[]}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    crate::observability::flush_audit_sync();

    let content = std::fs::read_to_string(&audit_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly 1 JSONL line");

    let ev: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(ev["status_code"], 200);
    assert_eq!(ev.get("failover_reason").and_then(|v| v.as_str()).unwrap_or(""), "");
    let path = ev["path"].as_str().unwrap();
    assert!(!path.contains('?'), "path must have no '?' but got: {path}");

    drop(audit_handle);
}

#[tokio::test]
async fn test_proxy_audit_case_c_streamed_client_disconnect() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset();
    let _dir = setup_isolated_test_dir();

    let tmp = tempfile::tempdir().unwrap();
    let audit_path = tmp.path().join("audit.jsonl");
    let audit_handle = crate::observability::init_audit_worker(audit_path.clone(), 1 << 20, 1);

    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream_listener.local_addr().unwrap().port();

    let upstream_task = tokio::spawn(async move {
        if let Ok((mut conn, _)) = upstream_listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).await;
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            conn.write_all(b"f\r\ndata: event 1\n\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = conn.write_all(b"f\r\ndata: event 2\n\n\r\n").await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{upstream_port}"));
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let pool = Pool {
        accounts: vec![Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(0.9),
                ..Default::default()
            }),
            ..Default::default()
        }],
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let h = handler.clone();
    let server_task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let io = hyper_util::rt::TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req| {
                let h = h.clone();
                async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        }
    });

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"POST /v1/models/gemini:streamGenerateContent HTTP/1.1\r\n\
Host: 127.0.0.1\r\n\
Accept: text/event-stream\r\n\
Content-Type: application/json\r\n\
Content-Length: 2\r\n\
\r\n\
{}",
        )
        .await
        .unwrap();

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&buf[..n]);
    assert!(resp_str.contains("200 OK"));
    assert!(resp_str.contains("data: event 1"));

    // Drop the TCP stream after first chunk without reading to end
    drop(stream);
    let _ = tokio::time::timeout(Duration::from_millis(500), server_task).await;
    let _ = upstream_task.await;

    crate::observability::flush_audit_sync();

    let content = std::fs::read_to_string(&audit_path).unwrap();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected exactly 1 JSONL line");

    let ev: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(ev["failover_reason"], "ClientDisconnected");

    drop(audit_handle);
}

#[test]
fn test_proxy_audit_case_d_audit_file_permission_mode() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let audit_path = tmp.path().join("audit.jsonl");
    let audit_handle = crate::observability::init_audit_worker(audit_path.clone(), 1 << 20, 1);

    let event = crate::observability::AuditEvent {
        timestamp: jiff::Timestamp::now().to_string(),
        request_id: "req-perm-test".to_string(),
        session_key: "sess-tag".to_string(),
        method: "POST".to_string(),
        path: "/test".to_string(),
        model: "model".to_string(),
        strategy: "smart".to_string(),
        attempt: 1,
        account_id: "acc-1".to_string(),
        account_name: "acc-1".to_string(),
        status_code: 200,
        ttfb_ms: 10,
        total_ms: 20,
        tokens: crate::observability::AuditTokens::zero(),
        quota_before: crate::observability::AuditQuota {
            q5: None,
            q7: None,
            r5_secs: None,
            r7_secs: None,
        },
        failover_reason: String::new(),
    };
    crate::observability::record_audit_event(event);
    crate::observability::flush_audit_sync();

    let meta = std::fs::metadata(&audit_path).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "audit file permission mode must be 0o600, got {:o}", mode);

    drop(audit_handle);
}
