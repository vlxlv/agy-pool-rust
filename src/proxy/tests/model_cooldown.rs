use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::model_cooldown::{record_model_capacity_error, reset_model_cooldowns, set_model_slow_ttfb_for_test};
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::{Account, Pool, QuotaState};
use crate::testutil::TestServer;

#[tokio::test]
async fn test_proxy_model_capacity_exhausted_cools_down_only_that_model() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let server = TestServer::start(move |req| {
        let body_str = String::from_utf8_lossy(&req.body);
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("token-acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            if body_str.contains("busy-model") {
                return (
                    503,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    b"{\"error\":{\"code\":503,\"message\":\"No capacity available for model busy-model on the server\",\"status\":\"UNAVAILABLE\",\"details\":[{\"@type\":\"type.googleapis.com/google.rpc.ErrorInfo\",\"reason\":\"MODEL_CAPACITY_EXHAUSTED\"}]}}".to_vec(),
                );
            }
            return (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"acc1\"}".to_vec(),
            );
        }
        h2.fetch_add(1, Ordering::SeqCst);
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"result\":\"acc2\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let hi = 0.95;
    let lo = 0.90;
    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(hi),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(lo),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
        strategy: "max_quota".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());

    let send = |model: &str, text: &str| {
        let handler = handler.clone();
        let body = format!(
            "{{\"model\":\"{}\",\"request\":{{\"contents\":[{{\"role\":\"user\",\"parts\":[{{\"text\":\"{}\"}}]}}]}}}}",
            model, text
        );
        async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (status, _, body_bytes) = serve_once(
                handler,
                Method::POST,
                "/v1internal:generateContent",
                headers,
                Bytes::from(body),
            )
            .await;
            (status, String::from_utf8_lossy(&body_bytes).to_string())
        }
    };

    // 1. Capacity error on acc_1 fails over to acc_2
    let (status, body) = send("busy-model", "first").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc2"));

    // 2. Same model again: acc_1 is skipped while cooling down
    let (status, body) = send("busy-model", "second").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc2"));
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);

    // 3. A different model still goes to acc_1
    let (status, body) = send("other-model", "third").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    // 4. Every account paused for the model: probe the one whose pause ends soonest (acc_1)
    record_model_capacity_error("acc_2", "busy-model");
    let before1 = acc1_hits.load(Ordering::SeqCst);
    let before2 = acc2_hits.load(Ordering::SeqCst);
    let (status, body) = send("busy-model", "fourth").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("busy-model"));
    assert_eq!(acc1_hits.load(Ordering::SeqCst), before1 + 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), before2);

    // Verify no account-wide cooldown or quota restriction was recorded
    let loaded = crate::storage::load_pool().unwrap();
    let stored = loaded.accounts.iter().find(|a| a.id == acc1.id).unwrap();
    assert!(stored.rate_limited_until.is_none());
    assert_eq!(stored.status, "");
}

#[tokio::test]
async fn test_proxy_slow_start_cools_down_only_that_model() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();

    set_model_slow_ttfb_for_test(Duration::from_millis(50));

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let slow = Arc::new(AtomicBool::new(true));
    let h1 = acc1_hits.clone();
    let is_slow = slow.clone();

    let server = TestServer::start(move |req| {
        let body_str = String::from_utf8_lossy(&req.body);
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        if auth.contains("token-acc_1") {
            h1.fetch_add(1, Ordering::SeqCst);
            if is_slow.load(Ordering::SeqCst) && body_str.contains("slow-model") {
                std::thread::sleep(Duration::from_millis(150));
            }
            return (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"result\":\"acc1\"}".to_vec(),
            );
        }
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"result\":\"acc2\"}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));

    let hi = 0.95;
    let lo = 0.90;
    let acc1 = Account {
        id: "acc_1".to_string(),
        email: "1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(hi),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_2".to_string(),
        email: "2@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(lo),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1.clone(), acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
        strategy: "max_quota".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let handler = Arc::new(Handler::new());

    let send = |model: &str, text: &str| {
        let handler = handler.clone();
        let body = format!(
            "{{\"model\":\"{}\",\"request\":{{\"contents\":[{{\"role\":\"user\",\"parts\":[{{\"text\":\"slow-{}\"}}]}}]}}}}",
            model, text
        );
        async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (status, _, body_bytes) = serve_once(
                handler,
                Method::POST,
                "/v1internal:generateContent",
                headers,
                Bytes::from(body),
            )
            .await;
            (status, String::from_utf8_lossy(&body_bytes).to_string())
        }
    };

    // 1. Slow but successful answer is still delivered, and one slow answer alone does not pause acc_1
    let (status, body) = send("slow-model", "first").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    // Fast answer in between resets the strike
    slow.store(false, Ordering::SeqCst);
    let (status, body) = send("slow-model", "fast").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    slow.store(true, Ordering::SeqCst);
    let (status, body) = send("slow-model", "strike1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    let (status, body) = send("slow-model", "strike2").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    // 2. After two slow answers in a row, the same model skips acc_1 and goes to acc_2
    let (status, body) = send("slow-model", "second").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc2"));

    // 3. Other models still use acc_1
    let (status, body) = send("other-model", "third").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("acc1"));

    // Restore slow ttfb default
    set_model_slow_ttfb_for_test(Duration::from_secs(12));
}
