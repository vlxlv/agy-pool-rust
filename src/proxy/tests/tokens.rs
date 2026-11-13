use std::sync::Arc;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::tokens::{TokenCounts, TokenRange, clear_pending, flush_token_usage, usage_by_account};
use crate::storage::{Account, Pool};
use crate::testutil::TestServer;

fn save_two_accounts() {
    let pool = Pool {
        accounts: vec![
            Account {
                id: "acc_1".to_string(),
                email: "1@example.com".to_string(),
                ..Default::default()
            },
            Account {
                id: "acc_2".to_string(),
                email: "2@example.com".to_string(),
                ..Default::default()
            },
        ],
        active_account_id: Some("acc_1".to_string()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

fn usage_now() -> std::collections::BTreeMap<String, TokenCounts> {
    flush_token_usage().unwrap();
    usage_by_account(TokenRange::Total, crate::clock::now_unix() as i64).unwrap()
}

#[tokio::test]
async fn test_generation_usage_recorded_once_for_successful_attempt_only() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    // acc_1 is rate limited and fails over; acc_2 answers with usage.
    let server = TestServer::start(move |req| {
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        if auth.contains("token-acc_1") {
            (
                429,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"error":{"code":429,"message":"rate limit","status":"RESOURCE_EXHAUSTED"}}"#.to_vec(),
            )
        } else {
            (
                200,
                vec![("Content-Type".to_string(), "application/json".to_string())],
                br#"{"candidates":[],"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":80,"candidatesTokenCount":45,"thoughtsTokenCount":30}}"#.to_vec(),
            )
        }
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1/models/gemini-2.5-pro:generateContent",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let usage = usage_now();
    assert!(
        !usage.contains_key("acc_1"),
        "a failed attempt records no usage: {usage:?}"
    );
    assert_eq!(
        usage["acc_2"],
        TokenCounts {
            prompt: 120,
            cached: 80,
            output: 45,
            thinking: 30,
            requests: 1,
            aborted: 0
        }
    );
    clear_pending();
}

#[tokio::test]
async fn test_streamed_generation_usage_uses_last_usage_metadata() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    // Gemini streams cumulative usageMetadata; the last value is the total.
    let server = TestServer::start(move |_req| {
        (
            200,
            vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            concat!(
                "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":500,\"candidatesTokenCount\":10}}\n\n",
                "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":500,\"candidatesTokenCount\":90,\"thoughtsTokenCount\":40}}\n\n"
            )
            .as_bytes()
            .to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let usage = usage_now();
    let total = crate::storage::tokens::sum(&usage);
    assert_eq!(total.requests, 1, "recorded exactly once: {usage:?}");
    assert_eq!(total.prompt, 500);
    assert_eq!(total.output, 90);
    assert_eq!(total.thinking, 40);
    assert_eq!(total.cached, 0, "cached not reported (-1) is stored as 0");
    assert_eq!(total.aborted, 0);
    clear_pending();
}

#[tokio::test]
async fn test_non_generation_requests_record_no_usage() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();
    clear_pending();

    let server = TestServer::start(move |_req| {
        (
            200,
            vec![("Content-Type".to_string(), "application/json".to_string())],
            br#"{"usageMetadata":{"promptTokenCount":999}}"#.to_vec(),
        )
    });
    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("token-{}", acc.id)));
    save_two_accounts();

    let (status, _, _) = serve_once(
        Arc::new(Handler::new()),
        Method::POST,
        "/v1internal:loadCodeAssist",
        json_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(usage_now().is_empty());
    clear_pending();
}
