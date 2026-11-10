use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hyper::http::{HeaderMap, HeaderValue, Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::observability::reset_in_flight;
use crate::proxy::Handler;
use crate::proxy::tests::{TEST_LOCK, serve_once, setup_two_account_pool};

#[tokio::test]
async fn test_sse_forwarding_and_truncation() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(res) => res,
                Err(_) => break,
            };
            let h1 = h1.clone();
            let h2 = h2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.contains("Bearer token-acc_1") {
                    h1.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    socket.write_all(resp.as_bytes()).await.unwrap();
                    socket.write_all(b"f\r\ndata: event 1\n\n\r\n").await.unwrap();
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    socket.write_all(b"f\r\ndata: event 2\n\n\r\n").await.unwrap();
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                } else {
                    h2.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            });
        }
    });

    let _url_guard = crate::proxy::set_backend_url_for_test(&format!("http://127.0.0.1:{port}"));
    let log_buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let lb = log_buf.clone();
    struct LogBufWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let _log_guard = crate::proxy::set_log_writer_for_test(Box::new(LogBufWriter(lb)));

    let handler = Arc::new(Handler::new());
    let mut headers = HeaderMap::new();
    headers.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/event-stream"));

    let (status, _, body) = serve_once(handler, Method::GET, "/v1/streamGenerateContent", headers, Bytes::new()).await;

    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("data: event 1"));
    assert!(body_str.contains("data: event 2"));

    // CRITICAL: acc_2 must never be contacted
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), 0);

    let logged = String::from_utf8_lossy(&log_buf.lock().unwrap().clone()).to_string();
    assert!(logged.contains("Status: 200, ttfb"));
    assert!(logged.contains("total"));
}

#[tokio::test]
async fn test_sse_commit_point_no_replay() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let acc1_hits = Arc::new(AtomicI32::new(0));
    let acc2_hits = Arc::new(AtomicI32::new(0));
    let h1 = acc1_hits.clone();
    let h2 = acc2_hits.clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(res) => res,
                Err(_) => break,
            };
            let h1 = h1.clone();
            let h2 = h2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);

                if req_str.contains("Bearer token-acc_1") {
                    h1.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    socket.write_all(resp.as_bytes()).await.unwrap();
                    socket.write_all(b"e\r\ndata: partial\n\n\r\n").await.unwrap();
                    // Abruptly terminate connection mid-stream (reset/close socket)
                    drop(socket);
                } else {
                    h2.fetch_add(1, Ordering::SeqCst);
                    let resp = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(resp.as_bytes()).await;
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

    // Invariant: Once stream is committed downstream, NEVER replay to acc_2!
    assert_eq!(acc1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(acc2_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_sse_keep_alive_slow_upstream() {
    let _lock = TEST_LOCK.lock().await;
    reset_in_flight();
    let (_dir, _acc1, _acc2) = setup_two_account_pool();

    let old_cfg = crate::config::get_static_config();
    let mut cfg = old_cfg.clone();
    cfg.server.keep_alive_secs = 1;
    crate::config::set_static_config(cfg);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(res) => res,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // Sleep past 1s keep-alive duration
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.write_all(b"15\r\ndata: delayed chunk\n\n\r\n").await;
                let _ = socket.write_all(b"0\r\n\r\n").await;
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
        "/v1/models/gemini:streamGenerateContent?alt=sse",
        headers,
        Bytes::from_static(b"{}"),
    )
    .await;

    crate::config::set_static_config(old_cfg);

    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains(": keep-alive\n\n"));
    assert!(body_str.contains("data: delayed chunk\n\n"));
}
