use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::confirmation::{clear_all_verification_confirmation_claims, is_verification_confirmation_claimed};
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::{Account, Pool, QuotaState, STATUS_VALIDATION_REQUIRED, STATUS_VERIFICATION_PENDING};
use crate::testutil::TestServer;

// 1. Only one confirmation in flight per process
#[tokio::test]
async fn test_verification_pending_only_one_confirmation_in_flight() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    clear_all_verification_confirmation_claims();
    let _dir = setup_isolated_test_dir();

    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (proceed_tx, proceed_rx) = tokio::sync::mpsc::channel::<()>(1);
    let entered_tx = Arc::new(tokio::sync::Mutex::new(Some(entered_tx)));
    let proceed_rx = Arc::new(tokio::sync::Mutex::new(proceed_rx));

    let pending_hits = Arc::new(AtomicI32::new(0));
    let healthy_hits = Arc::new(AtomicI32::new(0));
    let ph = pending_hits.clone();
    let hh = healthy_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("pending_1") {
            ph.fetch_add(1, Ordering::SeqCst);
            let etx = entered_tx.clone();
            let prx = proceed_rx.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    if let Some(tx) = etx.lock().await.take() {
                        let _ = tx.send(()).await;
                    }
                    let _ = prx.lock().await.recv().await;
                });
            });
            return (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"pending_success\"}".to_vec(),
            );
        }
        if auth.contains("healthy_1") {
            hh.fetch_add(1, Ordering::SeqCst);
            return (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"healthy_success\"}".to_vec(),
            );
        }
        (400, vec![], b"unknown".to_vec())
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let fraction = 0.9;
    let p_acc = Account {
        id: "pending_1".to_string(),
        email: "pending@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let h_acc = Account {
        id: "healthy_1".to_string(),
        email: "healthy@example.com".to_string(),
        status: "".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![p_acc.clone(), h_acc],
        active_account_id: Some(p_acc.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());

    // Launch req1 in background
    let h1 = handler.clone();
    let req1_task = tokio::spawn(async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let (status, _, body) = serve_once(
            h1,
            Method::POST,
            "/v1/models/gemini:generateContent",
            headers,
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&body).contains("pending_success"));
    });

    // Wait until req1 reaches upstream and claims pending_1
    entered_rx.recv().await.unwrap();
    assert!(is_verification_confirmation_claimed("pending_1"));

    // Launch req2 while req1 is still in flight: must bypass pending_1 and route to healthy_1
    let mut headers2 = HeaderMap::new();
    headers2.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status2, _, body2) = serve_once(
        handler.clone(),
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers2,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status2, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body2).contains("healthy_success"));
    assert_eq!(healthy_hits.load(Ordering::SeqCst), 1);

    // Release req1 to finish
    let _ = proceed_tx.send(()).await;
    req1_task.await.unwrap();
    assert_eq!(pending_hits.load(Ordering::SeqCst), 1);

    // Verify claim is released
    assert!(!is_verification_confirmation_claimed("pending_1"));

    // Verify persisted status transitioned to ready ("")
    let loaded = crate::storage::load_pool().unwrap();
    let stored = loaded.accounts.iter().find(|a| a.id == p_acc.id).unwrap();
    assert_eq!(stored.status, "");
}

// 2. Multiple pending: only one tried per request
#[tokio::test]
async fn test_verification_pending_multiple_pending_only_one_tried_per_request() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    clear_all_verification_confirmation_claims();
    let _dir = setup_isolated_test_dir();

    let p1_hits = Arc::new(AtomicI32::new(0));
    let p2_hits = Arc::new(AtomicI32::new(0));
    let h_hits = Arc::new(AtomicI32::new(0));
    let p1h = p1_hits.clone();
    let p2h = p2_hits.clone();
    let hh = h_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("pending_1") {
            p1h.fetch_add(1, Ordering::SeqCst);
            return (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\":{\"message\":\"validation_required\",\"details\":[{\"url\":\"https://accounts.google.com/verify?id=fresh\"}]}}".to_vec(),
            );
        }
        if auth.contains("pending_2") {
            p2h.fetch_add(1, Ordering::SeqCst);
            panic!("pending_2 was dispatched! Multiple pending accounts must not be swept in one request");
        }
        if auth.contains("healthy_1") {
            hh.fetch_add(1, Ordering::SeqCst);
            return (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"ok\"}".to_vec(),
            );
        }
        (400, vec![], b"unknown".to_vec())
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let fraction = 0.9;
    let p1 = Account {
        id: "pending_1".to_string(),
        email: "p1@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let p2 = Account {
        id: "pending_2".to_string(),
        email: "p2@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let h1 = Account {
        id: "healthy_1".to_string(),
        email: "h1@example.com".to_string(),
        status: "".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![p1.clone(), p2.clone(), h1],
        active_account_id: Some(p1.id.clone()),
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

    assert_eq!(status, StatusCode::OK);
    assert_eq!(p1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(p2_hits.load(Ordering::SeqCst), 0);
    assert_eq!(h_hits.load(Ordering::SeqCst), 1);

    let loaded = crate::storage::load_pool().unwrap();
    let stored_p1 = loaded.accounts.iter().find(|a| a.id == p1.id).unwrap();
    assert_eq!(stored_p1.status, STATUS_VALIDATION_REQUIRED);
    let stored_p2 = loaded.accounts.iter().find(|a| a.id == p2.id).unwrap();
    assert_eq!(stored_p2.status, STATUS_VERIFICATION_PENDING);

    assert!(!is_verification_confirmation_claimed("pending_1"));
    assert!(!is_verification_confirmation_claimed("pending_2"));
}

// 3. Transport failure remains pending
#[tokio::test]
async fn test_verification_pending_transport_failure_remains_pending() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    clear_all_verification_confirmation_claims();
    let _dir = setup_isolated_test_dir();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let fraction = 0.9;
    let p1 = Account {
        id: "pending_1".to_string(),
        email: "p1@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![p1.clone()],
        active_account_id: Some(p1.id.clone()),
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

    assert_ne!(status, StatusCode::OK);

    let loaded = crate::storage::load_pool().unwrap();
    let stored = loaded.accounts.iter().find(|a| a.id == p1.id).unwrap();
    assert_eq!(stored.status, STATUS_VERIFICATION_PENDING);
    assert!(!is_verification_confirmation_claimed("pending_1"));
}

// 4. Claim is released on every exit path
#[tokio::test]
async fn test_verification_pending_claim_released_on_every_exit_path() {
    let cases = vec![
        ("2xx_success", 200, r#"{"result":"ok"}"#),
        (
            "403_validation_required",
            403,
            r#"{"error":{"message":"validation_required"}}"#,
        ),
        ("401_auth_error", 401, r#"{"error":{"message":"invalid_grant"}}"#),
    ];

    for (name, status_code, body) in cases {
        let _lock = TEST_LOCK.lock().await;
        reset_in_flight();
        clear_all_verification_confirmation_claims();
        let _dir = setup_isolated_test_dir();

        let resp_body = body.as_bytes().to_vec();
        let server = TestServer::start(move |_| {
            (
                status_code,
                vec![("content-type".to_string(), "application/json".to_string())],
                resp_body.clone(),
            )
        });

        let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
        let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

        let p1 = Account {
            id: "pending_exit_test".to_string(),
            email: "exit@example.com".to_string(),
            status: STATUS_VERIFICATION_PENDING.to_string(),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![p1.clone()],
            active_account_id: Some(p1.id.clone()),
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

        let _ = serve_once(
            handler,
            Method::POST,
            "/v1/models/gemini:generateContent",
            headers,
            Bytes::from_static(b"{}"),
        )
        .await;

        assert!(
            !is_verification_confirmation_claimed("pending_exit_test"),
            "[{}] confirmation claim was not released!",
            name
        );
    }
}

// 5. No replay after commit
#[tokio::test]
async fn test_verification_pending_no_replay_after_commit() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    clear_all_verification_confirmation_claims();
    let _dir = setup_isolated_test_dir();

    let p1_hits = Arc::new(AtomicI32::new(0));
    let h1_hits = Arc::new(AtomicI32::new(0));
    let p1h = p1_hits.clone();
    let h1h = h1_hits.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let p1h = p1h.clone();
            let h1h = h1h.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.contains("token-pending_1") {
                    p1h.fetch_add(1, Ordering::SeqCst);
                    let resp =
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
                    let _ = conn.write_all(resp.as_bytes()).await;
                    let _ = conn.write_all(b"13\r\ndata: partial-chunk\n\n\r\n").await;
                    drop(conn);
                } else if req_str.contains("token-healthy_1") {
                    h1h.fetch_add(1, Ordering::SeqCst);
                    let _ = conn
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let fraction = 0.9;
    let p1 = Account {
        id: "pending_1".to_string(),
        email: "pending@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let h1 = Account {
        id: "healthy_1".to_string(),
        email: "healthy@example.com".to_string(),
        status: "".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![p1, h1],
        active_account_id: Some("pending_1".to_string()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

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

    assert_eq!(p1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(h1_hits.load(Ordering::SeqCst), 0);
    assert!(!is_verification_confirmation_claimed("pending_1"));
}

// 6. Force token refresh on verification pending confirmation
#[tokio::test]
async fn test_verification_pending_forces_token_refresh_on_confirmation() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    clear_all_verification_confirmation_claims();
    let _dir = setup_isolated_test_dir();

    let force_received = Arc::new(AtomicBool::new(false));
    let fr = force_received.clone();

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"result\":\"ok\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_with_force_for_test(move |acc, force| {
        if acc.id == "pending_cand" && force {
            fr.store(true, Ordering::SeqCst);
        }
        Ok(format!("fresh-token-{}", acc.id))
    });

    let fraction = 0.9;
    let cand = Account {
        id: "pending_cand".to_string(),
        email: "cand@example.com".to_string(),
        status: STATUS_VERIFICATION_PENDING.to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![cand.clone()],
        active_account_id: Some(cand.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:streamGenerateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(force_received.load(Ordering::SeqCst));

    let loaded = crate::storage::load_pool().unwrap();
    let stored = loaded.accounts.iter().find(|a| a.id == cand.id).unwrap();
    assert_eq!(stored.status, "");
}
