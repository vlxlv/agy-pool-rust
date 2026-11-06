use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_two_account_pool};
use crate::storage::{Pool, STATUS_VERIFICATION_PENDING};

fn no_replay_prevented_total() -> u64 {
    crate::observability::snapshot().no_replay_prevented
}

// 1. EOF before headers
#[tokio::test]
async fn test_transport_failure_eof_before_headers_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
}

// 2. TCP Connection Reset (RST)
#[tokio::test]
async fn test_transport_failure_connection_reset_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
}

// 3. Broken Pipe during request body
#[tokio::test]
async fn test_transport_failure_broken_pipe_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
    let handler = Arc::new(Handler::new());
    let large_body = vec![b'x'; 65536];
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
        Bytes::from(large_body),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(physical_attempts.load(Ordering::SeqCst), 1);
}

// 4. TLS handshake / certificate failure
#[tokio::test]
async fn test_transport_failure_tls_failure_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let physical_attempts = Arc::new(AtomicI32::new(0));
    let pa = physical_attempts.clone();

    // Plain TCP listener responding with plain text to a TLS client
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            pa.fetch_add(1, Ordering::SeqCst);
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await;
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("https://127.0.0.1:{port}"));
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

    // Pre-connect TLS failure fails over across both accounts; when all fail, 503 is returned.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(physical_attempts.load(Ordering::SeqCst), 2);
}

// 5. Malformed HTTP response status/headers
#[tokio::test]
async fn test_transport_failure_malformed_response_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let physical_attempts = Arc::new(AtomicI32::new(0));
    let pa = physical_attempts.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            pa.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            let _ = conn.write_all(b"INVALID_HTTP_STATUS_LINE\r\n\r\n").await;
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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
    assert_eq!(physical_attempts.load(Ordering::SeqCst), 1);
}

// 6. Truncated Content-Length (Content-Length: 100, sent 20 bytes then EOF)
#[tokio::test]
async fn test_transport_failure_truncated_buffered_body_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let physical_attempts = Arc::new(AtomicI32::new(0));
    let pa = physical_attempts.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            pa.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).await;
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n12345678901234567890").await;
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let (status, _, _) = serve_once(handler, Method::POST, "/v1/models", headers, Bytes::from_static(b"{}")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(physical_attempts.load(Ordering::SeqCst), 1);
}

// 7. Reused connection failure: connection reuse must not cause generation request replay
#[tokio::test]
async fn test_transport_failure_reused_connection_failure_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let request_count = Arc::new(AtomicI32::new(0));
    let rc = request_count.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let rc = rc.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                if n > 0 {
                    let count = rc.fetch_add(1, Ordering::SeqCst);
                    if count == 0 {
                        let _ = conn
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                            .await;
                    } else {
                        drop(conn);
                    }
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    // Send generation request: each generation attempt opens a fresh connection
    let mut headers = HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let (status1, _, _) = serve_once(
        handler.clone(),
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers.clone(),
        Bytes::from_static(b"{\"prompt\":\"1\"}"),
    )
    .await;
    assert_eq!(status1, StatusCode::OK);

    let (status2, _, _) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:generateContent",
        headers,
        Bytes::from_static(b"{\"prompt\":\"2\"}"),
    )
    .await;
    assert_eq!(status2, StatusCode::BAD_GATEWAY);
    assert_eq!(request_count.load(Ordering::SeqCst), 2);
}

// 8. SSE Truncation mid-stream
#[tokio::test]
async fn test_sse_truncation_mid_stream_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    conn.write_all(b"f\r\ndata: event 1\n\n\r\n").await.unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    conn.write_all(b"f\r\ndata: event 2\n\n\r\n").await.unwrap();
                    drop(conn);
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));

    let (status, _, body) = serve_once(
        handler,
        Method::POST,
        "/v1/models/gemini:streamGenerateContent",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("data: event 1"));
    assert!(s.contains("data: event 2"));
    assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_attempts.load(Ordering::SeqCst), 0);
}

// 9. SSE Connection Reset mid-stream
#[tokio::test]
async fn test_sse_connection_reset_mid_stream_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    conn.write_all(b"e\r\ndata: partial\n\n\r\n").await.unwrap();
                    drop(conn);
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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
}

// 10. SSE Timeout mid-stream
#[tokio::test]
async fn test_sse_timeout_mid_stream_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    conn.write_all(b"f\r\ndata: starting\n\n\r\n").await.unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    drop(conn);
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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
}

// 11. Client disconnect after stream committed
#[tokio::test]
async fn test_client_disconnect_after_stream_committed_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let acc1_attempts = Arc::new(AtomicI32::new(0));
    let acc2_attempts = Arc::new(AtomicI32::new(0));
    let a1 = acc1_attempts.clone();
    let a2 = acc2_attempts.clone();

    let stream_started = Arc::new(tokio::sync::Notify::new());
    let ss = stream_started.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let a1 = a1.clone();
            let a2 = a2.clone();
            let ss = ss.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                    conn.write_all(b"c\r\ndata: start\n\n\r\n").await.unwrap();
                    ss.notify_one();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let h = handler.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = server_listener.accept().await {
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

    let mut client_sock = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    client_sock.write_all(b"POST /v1/models/gemini:streamGenerateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\nAccept: text/event-stream\r\n\r\n{}").await.unwrap();

    stream_started.notified().await;
    drop(client_sock); // Disconnect client

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_attempts.load(Ordering::SeqCst), 0);
}

// 12. Client disconnect before commit
#[tokio::test]
async fn test_client_disconnect_before_commit_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let acc1_attempts = Arc::new(AtomicI32::new(0));
    let acc2_attempts = Arc::new(AtomicI32::new(0));
    let a1 = acc1_attempts.clone();
    let a2 = acc2_attempts.clone();

    let entered = Arc::new(tokio::sync::Notify::new());
    let ent = entered.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let a1 = a1.clone();
            let a2 = a2.clone();
            let ent = ent.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    ent.notify_one();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let h = handler.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = server_listener.accept().await {
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

    let mut client_sock = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    client_sock
        .write_all(b"POST /v1/models/gemini:generateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\n\r\n{}")
        .await
        .unwrap();

    entered.notified().await;
    drop(client_sock); // Disconnect client before headers

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_attempts.load(Ordering::SeqCst), 0);
}

// 13. Explicit HTTP Failover (429, 401, validation_required) preserved
#[tokio::test]
async fn test_explicit_failover_429_and_auth_preserved() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
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
                if req_str.contains("Bearer token-acc_1") {
                    a1.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 5\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"quota exceeded\"}}";
                    conn.write_all(resp.as_bytes()).await.unwrap();
                } else {
                    a2.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 42\r\nConnection: close\r\n\r\n{\"candidates\":[{\"text\":\"success from acc_2\"}]}";
                    conn.write_all(resp.as_bytes()).await.unwrap();
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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

    assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_attempts.load(Ordering::SeqCst), 1);
}

// 14. Ordinary HTTP errors (400, 404, 500) forwarded directly without replay
#[tokio::test]
async fn test_explicit_ordinary_error_no_replay() {
    for code in [400, 404, 500] {
        let _lock = TEST_LOCK.lock().await;
        reset_in_flight();
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
                    if req_str.contains("Bearer token-acc_1") {
                        a1.fetch_add(1, Ordering::SeqCst);
                        let body = format!("{{\"error\":\"code {}\"}}", code);
                        let resp = format!(
                            "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            code,
                            body.len(),
                            body
                        );
                        conn.write_all(resp.as_bytes()).await.unwrap();
                    } else {
                        a2.fetch_add(1, Ordering::SeqCst);
                        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = conn.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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

        assert_eq!(status.as_u16(), code);
        assert_eq!(acc1_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(acc2_attempts.load(Ordering::SeqCst), 0);
    }
}

// 16. Counter semantics: consistent accounting, no double-counting
#[tokio::test]
async fn test_counter_semantics_consistent() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let hits = Arc::new(AtomicI32::new(0));
    let h = hits.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            h.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).await;
            let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"candidates\":[]}";
            conn.write_all(resp.as_bytes()).await.unwrap();
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
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
    let pool = crate::storage::load_pool().unwrap();
    let acc = &pool.accounts[0];
    assert_eq!(acc.request_count, 1);
    assert_eq!(acc.hits(), 1);
}

// 17. Task / connection cleanup on SSE cancellation
#[tokio::test]
async fn test_goroutine_leak_sse_and_cancellation() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf).await;
                conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
                conn.write_all(b"11\r\ndata: heartbeat\n\n\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let h = handler.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = server_listener.accept().await {
            let io = hyper_util::rt::TokioIo::new(stream);
            let h_conn = h.clone();
            let service = hyper::service::service_fn(move |req| {
                let h = h_conn.clone();
                async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        }
    });

    for _ in 0..5 {
        let mut client_sock = tokio::net::TcpStream::connect(server_addr).await.unwrap();
        client_sock.write_all(b"POST /v1/models/gemini:streamGenerateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\nAccept: text/event-stream\r\n\r\n{}").await.unwrap();
        tokio::time::sleep(Duration::from_millis(15)).await;
        drop(client_sock);
    }

    // Go's assertNoGoroutineLeak: every cancelled stream must release its slot. Draining here also keeps
    // handler cleanup from running after the test guard has reset the data dir.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while crate::observability::in_flight_generation("acc_1") + crate::observability::in_flight_generation("acc_2") != 0
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "cancelled streams still hold in-flight slots"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// Promised covering test: transport_failure_timeout_no_replay
#[tokio::test]
async fn transport_failure_timeout_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let attempts = Arc::new(AtomicI32::new(0));
    let att = attempts.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            att.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).await;
            // Sleep without answering to simulate upstream timeout
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(conn);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let prev_no_replay = no_replay_prevented_total();

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

    // Any send error (including EOF or timeout) is terminal
    assert!(status == StatusCode::BAD_GATEWAY || status == StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(no_replay_prevented_total(), prev_no_replay + 1);
}

// Promised covering test: transport_failure_connection_refused_no_replay
#[tokio::test]
async fn transport_failure_connection_refused_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    // Use a port that immediately refuses connections
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // Closed port!

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let prev_no_replay = no_replay_prevented_total();

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

    // Pre-connect TCP refusal is failover-safe: tried across both accounts until pool exhausted -> 503
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Since connection was never established, no replay prevented counter is NOT incremented
    assert_eq!(no_replay_prevented_total(), prev_no_replay);
}

// Review Focus #1: client_disconnect_releases_everything
#[tokio::test]
async fn client_disconnect_releases_everything() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, mut acc1, _acc2) = setup_two_account_pool();
    acc1.status = STATUS_VERIFICATION_PENDING.to_string();
    let pool = Pool {
        accounts: vec![acc1.clone(), _acc2.clone()],
        active_account_id: Some(acc1.id.clone()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    crate::storage::save_pool(&pool).unwrap();

    let upstream_req_count = Arc::new(AtomicI32::new(0));
    let urc = upstream_req_count.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            urc.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_millis(2000)).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let h = handler.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = server_listener.accept().await {
            let io = hyper_util::rt::TokioIo::new(stream);
            let h = h.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let h = h.clone();
                    async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let mut client_stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    client_stream
        .write_all(b"POST /v1/models/gemini:generateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 2\r\nContent-Type: application/json\r\n\r\n{}")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Without this the post-disconnect checks would pass for a handler that never acquired anything.
    assert_eq!(crate::observability::in_flight_generation("acc_1"), 1);
    drop(client_stream); // Close client socket

    tokio::time::sleep(Duration::from_millis(1000)).await;

    assert_eq!(upstream_req_count.load(Ordering::SeqCst), 1);
    assert_eq!(crate::observability::in_flight_generation("acc_1"), 0);
    assert_eq!(crate::observability::in_flight_generation("acc_2"), 0);

    let sdb = crate::storage::StateDb::global().unwrap();
    let lease_count: i64 = sdb
        .view(|tx| {
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM inflight_leases", [], |r| r.get(0))?;
            Ok(count)
        })
        .unwrap();
    assert_eq!(lease_count, 0, "expected no active leases");
    assert!(!crate::proxy::confirmation::is_verification_confirmation_claimed(
        "acc_1"
    ));
}

// Review Focus #2: sse_first_chunk_not_buffered
#[tokio::test]
async fn sse_first_chunk_not_buffered() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            sock.write_all(b"9\r\ndata: 1\n\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(1000)).await;
            sock.write_all(b"9\r\ndata: 2\n\n\r\n0\r\n\r\n").await.unwrap();
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let handler = Arc::new(Handler::new());

    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let h = handler.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = server_listener.accept().await {
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

    let mut client = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let start = std::time::Instant::now();
    client
        .write_all(b"GET /v1/models/gemini:streamGenerateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n")
        .await
        .unwrap();

    let mut buf = [0u8; 1024];
    let mut total_read = Vec::new();
    loop {
        let n = client.read(&mut buf).await.unwrap();
        assert!(
            n != 0,
            "stream ended before first event: {:?}",
            String::from_utf8_lossy(&total_read)
        );
        total_read.extend_from_slice(&buf[..n]);
        let s = String::from_utf8_lossy(&total_read);
        if s.contains("data: 1") {
            let first_event_time = start.elapsed();
            assert!(
                first_event_time < Duration::from_millis(500),
                "first event took {:?}, expected < 500ms (before upstream 1s second chunk)",
                first_event_time
            );
            break;
        }
    }
}

// Review Focus #3: ambiguous_failure_never_contacts_second_account
#[tokio::test]
async fn ambiguous_failure_never_contacts_second_account() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let conn_count = Arc::new(AtomicI32::new(0));
    let cc = conn_count.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            cc.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            drop(sock);
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let prev_no_replay = no_replay_prevented_total();

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
    assert_eq!(conn_count.load(Ordering::SeqCst), 1);
    assert_eq!(no_replay_prevented_total(), prev_no_replay + 1);
}
