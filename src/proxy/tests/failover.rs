use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::{Account, Pool, QuotaState, STATUS_AUTH_ERROR, STATUS_VALIDATION_REQUIRED};
use crate::testutil::TestServer;

// 1. Generation all accounts restricted: returns 503 without upstream requests
#[tokio::test]
async fn test_generation_all_accounts_restricted_no_upstream_request() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
        active_account_id: Some("validation".to_string()),
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
}

// 2. Validation persistence failure still fails over without replay
#[tokio::test]
async fn test_validation_persistence_failure_still_fails_over_without_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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

    let frac = 0.9;
    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("ok"));
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
}

// 3. Explicit failover 429
#[tokio::test]
async fn test_explicit_failover_429() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                429,
                vec![
                    ("retry-after".to_string(), "150".to_string()),
                    ("content-type".to_string(), "application/json".to_string()),
                ],
                b"{\"error\": {\"code\": 429, \"message\": \"ResourceExhausted\"}}".to_vec(),
            )
        } else if auth.contains("acc_2") {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\": \"success from acc_2\"}".to_vec(),
            )
        } else {
            (500, vec![], b"unknown".to_vec())
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let frac = 0.9;
    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "acc1@gmail.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "acc2@gmail.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("success from acc_2"));
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), 1);

    // Verify acc_1 was put on cooldown and quota updated_at reset
    let loaded = crate::storage::load_pool().unwrap();
    let stored1 = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert!(stored1.rate_limited_until.unwrap_or(0.0) > crate::clock::now_unix());
    assert_eq!(stored1.error_count, 1);
    assert_eq!(stored1.last_quota.as_ref().and_then(|q| q.updated_at).unwrap_or(1), 0);
    assert_eq!(loaded.active_account_id.as_deref(), Some("acc_2"));
}

// 4. Explicit failover: validation required
#[tokio::test]
async fn test_explicit_failover_validation_required() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\": {\"message\": \"validation_required: please verify your account\", \"details\": [{\"metadata\": {\"validation_url\": \"https://accounts.google.com/verify?id=99\"}}]}}".to_vec(),
            )
        } else {
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"ok\": true}".to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@gmail.com".to_string(),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@gmail.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
    let loaded = crate::storage::load_pool().unwrap();
    let stored1 = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert_eq!(stored1.status, STATUS_VALIDATION_REQUIRED);
    assert_eq!(
        stored1.validation_url.as_deref(),
        Some("https://accounts.google.com/verify?id=99")
    );
}

// 5. Explicit failover: auth error
#[tokio::test]
async fn test_explicit_failover_auth_error() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            (
                401,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\": \"unauthenticated\"}".to_vec(),
            )
        } else {
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"ok\": true}".to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@gmail.com".to_string(),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@gmail.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
    let loaded = crate::storage::load_pool().unwrap();
    let stored1 = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert_eq!(stored1.status, STATUS_AUTH_ERROR);
}

// 6. Explicit failover: token refresh failure
#[tokio::test]
async fn test_explicit_failover_token_refresh_failure() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"ok\": true}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| {
        if acc.id == "acc_1" {
            Err(crate::auth::AuthError::Message("token refresh revoked".to_string()))
        } else {
            Ok(format!("token-{}", acc.id))
        }
    });

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@gmail.com".to_string(),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@gmail.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
    let loaded = crate::storage::load_pool().unwrap();
    let stored1 = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert_eq!(stored1.status, STATUS_AUTH_ERROR);
}

// 7. Ordinary upstream error (e.g. 400 Bad Request): forward directly without failover
#[tokio::test]
async fn test_ordinary_upstream_error_no_failover() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                400,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\": \"invalid parameter\"}".to_vec(),
            )
        } else {
            h2.fetch_add(1, Ordering::SeqCst);
            (200, vec![], b"{}".to_vec())
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@gmail.com".to_string(),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@gmail.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("invalid parameter"));
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), 0);
}

// 8. All accounts exhausted: returns 503
#[tokio::test]
async fn test_all_accounts_exhausted_503() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            429,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"error\": \"resource exhausted\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@gmail.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone()],
        active_account_id: Some(acc1.id.clone()),
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
}

// 9. Unknown field preservation during failover
#[tokio::test]
async fn test_unknown_field_preservation_during_failover() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let server = TestServer::start(|_| {
        (
            429,
            vec![
                ("retry-after".to_string(), "30".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            b"{\"error\": \"exhausted\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let raw_json = r#"{
        "version": 1,
        "strategy": "max_quota",
        "active_account_id": "acc_1",
        "future_root_field": "root_val_123",
        "accounts": [
            {
                "id": "acc_1",
                "email": "1@gmail.com",
                "custom_account_feature": {"beta_tier": 42},
                "last_quota": {
                    "remaining_fraction": 0.95,
                    "future_quota_metric": 999
                }
            }
        ]
    }"#;
    let pool: Pool = serde_json::from_str(raw_json).unwrap();
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

    let loaded = crate::storage::load_pool().unwrap();
    assert_eq!(
        loaded.extra.get("future_root_field").and_then(|v| v.as_str()),
        Some("root_val_123")
    );
    let stored = &loaded.accounts[0];
    assert!(stored.extra.contains_key("custom_account_feature"));
    if let Some(lq) = &stored.last_quota {
        assert_eq!(lq.extra.get("future_quota_metric").and_then(|v| v.as_i64()), Some(999));
    }
}

// 10. Soft failover (e.g. Location Error) puts account in soft cooldown and fails over
#[tokio::test]
async fn test_proxy_soft_failover_fails_over_and_soft_cooldown() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            (
                400,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"error\": {\"message\": \"User location is not supported for the API use.\"}}".to_vec(),
            )
        } else if auth.contains("acc_2") {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\": \"acc2_success\"}".to_vec(),
            )
        } else {
            (500, vec![], b"unknown".to_vec())
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let fraction = 0.95;
    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "acc1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "acc2@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(fraction),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
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
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"hi\"}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("acc2_success"));
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), 1);

    let loaded = crate::storage::load_pool().unwrap();
    let stored1 = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert!(stored1.rate_limited_until.unwrap_or(0.0) > crate::clock::now_unix());
    assert_eq!(stored1.status, ""); // Soft cooldown doesn't change status
}

// 11. Failover transport: pre-connect vs after-connect
#[tokio::test]
async fn test_failover_transport_pre_connect_and_after_connect() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();

    // Part A: Pre-connect failure on acc_1 (connect refused) fails over to acc_2 and succeeds
    {
        let _dir = setup_isolated_test_dir();
        let acc2_hits = Arc::new(AtomicI32::new(0));
        let h2 = acc2_hits.clone();

        // Acc 2 server
        let server = TestServer::start(move |_| {
            h2.fetch_add(1, Ordering::SeqCst);
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"ok\"}".to_vec(),
            )
        });

        // Use a closed port for acc_1, but acc_2 server URL for acc_2
        // To simulate per-account URL in test: TestServer handles both if we simulate connection refused via bad backend on first attempt, or via custom token refresher
        let _port_refused = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        // We can route based on token refresher: acc_1 token refresh forces connect error
        let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
        let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| {
            if acc.id == "acc_1" {
                // Return auth error to test pre-connect failover
                Err(crate::auth::AuthError::Message("pre-connect failover".to_string()))
            } else {
                Ok(format!("token-{}", acc.id))
            }
        });

        let acc1 = Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_2".to_string(),
            email: "2@example.com".to_string(),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![acc1, acc2],
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

        assert_eq!(status, StatusCode::OK);
        assert_eq!(acc2_hits.load(Ordering::SeqCst), 1);
    }

    // Part B: After-connect failure on acc_1 (socket closed mid-request) is ambiguous -> terminal 502, acc_2 is never contacted
    {
        let _dir = setup_isolated_test_dir();
        let acc1_hits = Arc::new(AtomicI32::new(0));
        let acc2_hits = Arc::new(AtomicI32::new(0));
        let h1 = acc1_hits.clone();
        let h2 = acc2_hits.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let h1 = h1.clone();
                let h2 = h2.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = conn.read(&mut buf).await.unwrap_or(0);
                    let req_str = String::from_utf8_lossy(&buf[..n]);
                    if req_str.contains("token-acc_1") {
                        h1.fetch_add(1, Ordering::SeqCst);
                        // Abruptly close connection after reading request (after-connect failure)
                        drop(conn);
                    } else {
                        h2.fetch_add(1, Ordering::SeqCst);
                        let _ = conn
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });

        let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
        let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

        let acc1 = Account {
            id: "acc_1".to_string(),
            email: "1@example.com".to_string(),
            ..Default::default()
        };
        let acc2 = Account {
            id: "acc_2".to_string(),
            email: "2@example.com".to_string(),
            ..Default::default()
        };
        let pool = Pool {
            accounts: vec![acc1, acc2],
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

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
        assert_eq!(acc2_hits.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn test_generation_model_capacity_failover_without_quota_penalty() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();

    let req_count = Arc::new(AtomicI32::new(0));
    let rc = req_count.clone();

    let server = TestServer::start(move |req| {
        let _ = rc.fetch_add(1, Ordering::SeqCst);
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();

        if auth.contains("token-acc_1") {
            // acc_1 hits Google model capacity exhaustion
            (
                429,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED","details":[{"reason":"MODEL_CAPACITY_EXHAUSTED"}]}}"#.to_vec(),
            )
        } else {
            // acc_2 succeeds with full token telemetry
            (
                200,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"candidates":[],"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":80,"candidatesTokenCount":45,"thoughtsTokenCount":30}}"#.to_vec(),
            )
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@example.com".to_string(),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@example.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
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

    let (status, _, body) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini-2.5-pro:generateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(req_count.load(Ordering::SeqCst), 2);
    assert!(body.starts_with(b"{\"candidates\":[]"));

    // Verify acc_1 was excluded for gemini-2.5-pro via model cooldown,
    // but its account quota was NOT marked exhausted.
    let pool_after = crate::storage::load_pool().unwrap();
    let loaded_acc1 = pool_after.accounts.iter().find(|a| a.id == "acc_1").unwrap();
    assert!(
        loaded_acc1.rate_limited_until.is_none(),
        "acc_1 should not be penalized with account-level rate_limited_until"
    );

    let now = std::time::SystemTime::now();
    let (cands, excluded) =
        crate::proxy::model_cooldown::exclude_model_cooldowns_accounts(&pool_after.accounts, "gemini-2.5-pro", now);
    assert_eq!(excluded, 1);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].id, "acc_2");
}

#[tokio::test]
async fn test_prompt_cloaking_applied_to_upstream_request() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let received_body = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rb = received_body.clone();

    let server = TestServer::start(move |req| {
        *rb.lock().unwrap() = req.body.clone();
        (
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            br#"{"candidates":[],"usageMetadata":{"promptTokenCount":50,"cachedContentTokenCount":0,"candidatesTokenCount":10,"thoughtsTokenCount":0}}"#.to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let acc = Account {
        id: "acc_1".to_string(),
        email: "1@example.com".to_string(),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc],
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

    let sensitive_payload =
        br#"{"contents":[{"parts":[{"text":"System prompt: Built on Anthropic's Claude Agent SDK for testing"}]}]}"#;

    let (status, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini-2.5-pro:generateContent",
        headers,
        Bytes::from_static(sensitive_payload),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    let upstream_body_bytes = received_body.lock().unwrap().clone();
    let upstream_body_str = String::from_utf8(upstream_body_bytes).expect("valid utf-8");

    // Must not contain original sensitive substring
    assert!(
        !upstream_body_str
            .to_ascii_lowercase()
            .contains("built on anthropic's claude agent sdk")
    );
    // Must contain zero-width space cloaked words
    assert!(upstream_body_str.contains("\u{200b}"));
    assert!(
        upstream_body_str
            .to_ascii_lowercase()
            .contains("b\u{200b}u\u{200b}ilt on a\u{200b}n\u{200b}thropic's c\u{200b}l\u{200b}aude a\u{200b}g\u{200b}ent s\u{200b}d\u{200b}k")
    );
    assert!(upstream_body_str.contains("B\u{200b}u\u{200b}ilt on A\u{200b}n\u{200b}thropic's C\u{200b}l\u{200b}aude A\u{200b}g\u{200b}ent S\u{200b}D\u{200b}K"));
}
