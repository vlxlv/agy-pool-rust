pub mod core;
pub mod failover;
pub mod inflight;
pub mod model_cooldown;
pub mod observability;
pub mod stream;
pub mod tokens;
pub mod transport_failure;
pub mod verification_pending;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::http::{HeaderMap, Method, StatusCode};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tempfile::TempDir;

use crate::proxy::Handler;
use crate::storage::{Account, Pool};

pub struct ProxyTestLock {
    async_lock: tokio::sync::Mutex<()>,
}

pub struct ProxyTestGuard<'a> {
    _async_guard: tokio::sync::MutexGuard<'a, ()>,
    _sync_guard: std::sync::MutexGuard<'static, ()>,
}

impl ProxyTestLock {
    pub const fn new() -> Self {
        Self {
            async_lock: tokio::sync::Mutex::const_new(()),
        }
    }

    pub async fn lock(&self) -> ProxyTestGuard<'_> {
        let async_guard = self.async_lock.lock().await;
        let sync_guard = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        crate::proxy::model_cooldown::reset_model_cooldowns();
        crate::proxy::smooth_rate::reset_smooth_rate();
        crate::proxy::clear_all_verification_confirmation_claims();
        ProxyTestGuard {
            _async_guard: async_guard,
            _sync_guard: sync_guard,
        }
    }
}

impl Default for ProxyTestLock {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Drop for ProxyTestGuard<'a> {
    fn drop(&mut self) {
        crate::proxy::model_cooldown::reset_model_cooldowns();
        crate::proxy::smooth_rate::reset_smooth_rate();
        crate::proxy::clear_all_verification_confirmation_claims();
        crate::config::reset_data_dir();
        crate::config::set_test_mode(false);
    }
}

pub static TEST_LOCK: LazyLock<ProxyTestLock> = LazyLock::new(ProxyTestLock::new);

pub struct TestEnv {
    pub dir: TempDir,
    pub handler: Arc<Handler>,
}

pub fn setup_isolated_test_dir() -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    crate::config::configure_state_dir(tmp.path()).expect("failed to configure state dir");
    tmp
}

pub fn setup_two_account_pool() -> (TempDir, Account, Account) {
    let tmp = setup_isolated_test_dir();
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
    crate::storage::save_pool(&pool).expect("failed to save pool");
    crate::proxy::set_token_refresher(|acc| Ok(format!("token-{}", acc.id)));
    (tmp, acc1, acc2)
}

/// Runs one request end-to-end through a real hyper HTTP/1 server over a localhost socket.
pub async fn serve_once(
    handler: Arc<Handler>,
    method: Method,
    path_and_query: &str,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, Bytes) {
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

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut req_builder = hyper::http::Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path_and_query}"));
    if !headers.contains_key(hyper::header::CONNECTION) {
        req_builder = req_builder.header(hyper::header::CONNECTION, "close");
    }
    for (k, v) in headers.iter() {
        req_builder = req_builder.header(k, v);
    }
    let req = req_builder.body(http_body_util::Full::new(body)).unwrap();

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut body_bytes = bytes::BytesMut::new();
    let mut body = resp.into_body();
    while let Some(frame_res) = body.frame().await {
        match frame_res {
            Ok(frame) => {
                if let Some(chunk) = frame.data_ref() {
                    body_bytes.extend_from_slice(chunk);
                }
            }
            Err(_) => break,
        }
    }
    let body_bytes = body_bytes.freeze();

    drop(sender);
    let _ = tokio::time::timeout(Duration::from_millis(500), server_task).await;
    (status, headers, body_bytes)
}
