use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};

use crate::observability::{in_flight_generation, reset_in_flight};
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_isolated_test_dir};
use crate::storage::{Account, Pool, QuotaState};
use crate::testutil::TestServer;

// 1. In-flight lifecycle
#[tokio::test]
async fn test_in_flight_lifecycle() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let frac = 0.9;
    let acc1 = Account {
        id: "acc_flight_1".to_string(),
        email: "flight1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_flight_2".to_string(),
        email: "flight2@example.com".to_string(),
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

    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("mock_token_{}", acc.id)));

    // Part 1: Successful generation in-flight observation
    {
        let (in_flight_tx, mut in_flight_rx) = tokio::sync::mpsc::channel::<i64>(1);
        let (release_tx, release_rx) = tokio::sync::mpsc::channel::<()>(1);
        let if_tx = Arc::new(tokio::sync::Mutex::new(Some(in_flight_tx)));
        let rel_rx = Arc::new(tokio::sync::Mutex::new(release_rx));

        let server = TestServer::start(move |_| {
            let tx = if_tx.clone();
            let rx = rel_rx.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    if let Some(s) = tx.lock().await.take() {
                        let _ = s.send(in_flight_generation("acc_flight_1")).await;
                    }
                    let _ = rx.lock().await.recv().await;
                });
            });
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                b"{\"candidates\":[]}".to_vec(),
            )
        });

        let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
        let handler = Arc::new(Handler::new());

        let h = handler.clone();
        let task = tokio::spawn(async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (status, _, _) = serve_once(
                h,
                Method::POST,
                "/v1/models/gemini:generateContent",
                headers,
                Bytes::from_static(b"{}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        });

        let count = in_flight_rx.recv().await.unwrap();
        assert_eq!(count, 1, "expected in-flight count 1 during active dispatch");

        let _ = release_tx.send(()).await;
        task.await.unwrap();
        assert_eq!(in_flight_generation("acc_flight_1"), 0);
    }

    // Part 2: Failover returns in-flight to 0
    {
        let server = TestServer::start(|req| {
            let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
            if auth.contains("mock_token_acc_flight_1") {
                (
                    429,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    b"{\"error\":{\"code\":429,\"message\":\"Resource exhausted\"}}".to_vec(),
                )
            } else {
                (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    b"{\"candidates\":[]}".to_vec(),
                )
            }
        });
        let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
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
        assert_eq!(in_flight_generation("acc_flight_1"), 0);
        assert_eq!(in_flight_generation("acc_flight_2"), 0);
    }

    // Part 3: Non-generation request bypasses in-flight tracking
    {
        let server = TestServer::start(|_| (200, vec![], b"[]".to_vec()));
        let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
        let handler = Arc::new(Handler::new());
        let (status, _, _) = serve_once(handler, Method::GET, "/v1/models", HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(in_flight_generation("acc_flight_1"), 0);
    }
}

// 2. Concurrent dispatch distribution
#[tokio::test]
async fn test_in_flight_concurrent_dispatch_distribution() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    let frac = 0.95;
    let acc1 = Account {
        id: "acc_flight_1".to_string(),
        email: "f1@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let acc2 = Account {
        id: "acc_flight_2".to_string(),
        email: "f2@example.com".to_string(),
        last_quota: Some(QuotaState {
            remaining_fraction: Some(frac),
            ..Default::default()
        }),
        ..Default::default()
    };
    let pool = Pool {
        accounts: vec![acc1, acc2],
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("mock_token_{}", acc.id)));

    let (req1_started_tx, mut req1_started_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (req2_started_tx, mut req2_started_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (release_tx, _) = tokio::sync::broadcast::channel::<()>(2);

    let r1_tx = Arc::new(tokio::sync::Mutex::new(Some(req1_started_tx)));
    let r2_tx = Arc::new(tokio::sync::Mutex::new(Some(req2_started_tx)));
    let rel_tx = release_tx.clone();

    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").map(|s| s.as_str()).unwrap_or("");
        let r1 = r1_tx.clone();
        let r2 = r2_tx.clone();
        let mut rel_rx = rel_tx.subscribe();

        if auth.contains("mock_token_acc_flight_1") {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    if let Some(tx) = r1.lock().await.take() {
                        let _ = tx.send(()).await;
                    }
                    let _ = rel_rx.recv().await;
                });
            });
        } else if auth.contains("mock_token_acc_flight_2") {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    if let Some(tx) = r2.lock().await.take() {
                        let _ = tx.send(()).await;
                    }
                    let _ = rel_rx.recv().await;
                });
            });
        }
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\":[]}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let handler = Arc::new(Handler::new());

    // Launch request 1
    let h1 = handler.clone();
    let t1 = tokio::spawn(async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let (status, _, _) = serve_once(
            h1,
            Method::POST,
            "/v1/models/gemini:generateContent",
            headers,
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    });

    // Wait for req 1 to start on acc_flight_1
    req1_started_rx.recv().await.unwrap();

    // While req 1 is in-flight, launch req 2: must route to acc_flight_2!
    let h2 = handler.clone();
    let t2 = tokio::spawn(async move {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let (status, _, _) = serve_once(
            h2,
            Method::POST,
            "/v1/models/gemini:generateContent",
            headers,
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    });

    // Wait for req 2 to start on acc_flight_2
    req2_started_rx.recv().await.unwrap();

    // Release both
    let _ = release_tx.send(());
    t1.await.unwrap();
    t2.await.unwrap();

    assert_eq!(in_flight_generation("acc_flight_1"), 0);
    assert_eq!(in_flight_generation("acc_flight_2"), 0);
}

// 3. Burst spreads across accounts
#[tokio::test]
async fn test_in_flight_burst_spreads_across_accounts() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    const N: usize = 12;
    let frac = 0.95;
    let mut accounts = Vec::new();
    for i in 0..N {
        let id = format!("acc_burst_{i}");
        accounts.push(Account {
            id: id.clone(),
            email: format!("{id}@example.com"),
            last_quota: Some(QuotaState {
                remaining_fraction: Some(frac),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    let pool = Pool {
        accounts,
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("mock_token_{}", acc.id)));

    let seen = Arc::new(Mutex::new(HashMap::new()));
    let (arrived_tx, mut arrived_rx) = tokio::sync::mpsc::channel::<()>(N);
    let (release_tx, _) = tokio::sync::broadcast::channel::<()>(N);

    let s = seen.clone();
    let rel_tx = release_tx.clone();
    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").cloned().unwrap_or_default();
        s.lock().unwrap().entry(auth).and_modify(|c| *c += 1).or_insert(1);
        // Subscribe before signalling arrival: a release broadcast sent in between would be lost.
        let mut rel_rx = rel_tx.subscribe();
        let _ = arrived_tx.try_send(());
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let _ = rel_rx.recv().await;
            });
        });
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\":[]}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let handler = Arc::new(Handler::new());

    let mut tasks = Vec::new();
    for _ in 0..N {
        let h = handler.clone();
        tasks.push(tokio::spawn(async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (status, _, _) = serve_once(
                h,
                Method::POST,
                "/v1/models/gemini:generateContent",
                headers,
                Bytes::from_static(b"{}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }));
    }

    for _ in 0..N {
        tokio::time::timeout(Duration::from_secs(5), arrived_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    let _ = release_tx.send(());
    for t in tasks {
        t.await.unwrap();
    }

    let seen_map = seen.lock().unwrap().clone();
    assert_eq!(
        seen_map.len(),
        N,
        "expected {N} distinct accounts used, got {:?}",
        seen_map
    );
    for i in 0..N {
        assert_eq!(in_flight_generation(&format!("acc_burst_{i}")), 0);
    }
}

// 4. Hundred concurrent unknown quota
#[tokio::test]
async fn test_in_flight_hundred_concurrent_unknown_quota() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let _dir = setup_isolated_test_dir();

    const ACCOUNTS_N: usize = 5;
    const REQUESTS_N: usize = 100;

    let mut accounts = Vec::new();
    for i in 0..ACCOUNTS_N {
        let id = format!("acc_cold_{i}");
        accounts.push(Account {
            id: id.clone(),
            email: format!("{id}@example.com"),
            ..Default::default()
        });
    }
    let pool = Pool {
        accounts,
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let _tok_guard = crate::proxy::set_token_refresher_for_test(|acc| Ok(format!("mock_token_{}", acc.id)));
    let _quota_guard = crate::proxy::set_quota_refresher_for_test(|_| {});

    let seen = Arc::new(Mutex::new(HashMap::new()));
    let (arrived_tx, mut arrived_rx) = tokio::sync::mpsc::channel::<()>(REQUESTS_N);
    let (release_tx, _) = tokio::sync::broadcast::channel::<()>(REQUESTS_N);

    let s = seen.clone();
    let rel_tx = release_tx.clone();
    let server = TestServer::start(move |req| {
        let auth = req.headers.get("authorization").cloned().unwrap_or_default();
        s.lock().unwrap().entry(auth).and_modify(|c| *c += 1).or_insert(1);
        // Subscribe before signalling arrival: a release broadcast sent in between would be lost.
        let mut rel_rx = rel_tx.subscribe();
        let _ = arrived_tx.try_send(());
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let _ = rel_rx.recv().await;
            });
        });
        (
            200,
            vec![("content-type".to_string(), "application/json".to_string())],
            b"{\"candidates\":[]}".to_vec(),
        )
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&server.url());
    let handler = Arc::new(Handler::new());

    let mut tasks = Vec::new();
    for _ in 0..REQUESTS_N {
        let h = handler.clone();
        tasks.push(tokio::spawn(async move {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            let (status, _, _) = serve_once(
                h,
                Method::POST,
                "/v1/models/gemini:generateContent",
                headers,
                Bytes::from_static(b"{}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }));
    }

    for _ in 0..REQUESTS_N {
        tokio::time::timeout(Duration::from_secs(10), arrived_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    let _ = release_tx.send(());
    for t in tasks {
        t.await.unwrap();
    }

    let seen_map = seen.lock().unwrap().clone();
    assert_eq!(seen_map.len(), ACCOUNTS_N, "expected all {ACCOUNTS_N} accounts used");
    let expected_per_acc = REQUESTS_N / ACCOUNTS_N;
    for (auth, count) in &seen_map {
        assert_eq!(
            *count, expected_per_acc,
            "{auth} got {count} requests, want {expected_per_acc}"
        );
    }
    for i in 0..ACCOUNTS_N {
        assert_eq!(in_flight_generation(&format!("acc_cold_{i}")), 0);
    }
}
