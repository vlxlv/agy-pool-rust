pub mod dto;
pub mod telemetry;

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::{Method, Request, Response, StatusCode};

use crate::proxy::body::{full_body, ProxyBody};

pub fn dashboard_html() -> &'static str {
    "<!doctype html><html><body>agy-pool dashboard</body></html>"
}

pub struct SseBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
}

impl SseBody {
    pub fn new(rx: tokio::sync::mpsc::Receiver<Bytes>) -> Self {
        Self { rx }
    }
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => Poll::Ready(Some(Ok(Frame::data(data)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn method_not_allowed() -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(b"Method Not Allowed\n")));
    *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    resp.headers_mut().insert(
        hyper::header::ALLOW,
        hyper::header::HeaderValue::from_static("GET"),
    );
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn json_response(bytes: Vec<u8>) -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from(bytes)));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    resp.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    resp
}

pub fn try_handle_request(req: &Request<hyper::body::Incoming>) -> Option<Response<ProxyBody>> {
    let path = req.uri().path();
    let is_dashboard = matches!(path, "/" | "/dashboard" | "/dashboard/");
    let is_status = matches!(path, "/api/v1/status" | "/api/v1/status/");
    let is_accounts = matches!(path, "/api/v1/accounts" | "/api/v1/accounts/");
    let is_forecast = matches!(path, "/api/v1/forecast" | "/api/v1/forecast/");
    let is_events = matches!(path, "/api/v1/events" | "/api/v1/events/");

    if !is_dashboard && !is_status && !is_accounts && !is_forecast && !is_events {
        return None;
    }

    if req.method() != Method::GET {
        return Some(method_not_allowed());
    }

    if is_dashboard {
        let mut resp = Response::new(full_body(Bytes::from_static(dashboard_html().as_bytes())));
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("text/html; charset=utf-8"),
        );
        return Some(resp);
    }

    if is_status {
        let status = dto::build_status_response();
        let bytes = serde_json::to_vec(&status).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_accounts {
        let show_email = req.uri().query().map(|q| {
            url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "show_email" && (v == "true" || v == "1"))
        }).unwrap_or(false);
        let pool = crate::storage::load_pool().unwrap_or_default();
        let now_ts = jiff::Timestamp::now().as_second() as f64;
        let accounts = dto::build_accounts_response(&pool, show_email, now_ts);
        let bytes = serde_json::to_vec(&accounts).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_forecast {
        let pool = crate::storage::load_pool().unwrap_or_default();
        let now_ts = jiff::Timestamp::now().as_second() as f64;
        let forecast = dto::build_forecast_response(&pool, now_ts);
        let bytes = serde_json::to_vec(&forecast).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_events {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
        let mut audit_rx = telemetry::subscribe_events();
        let recent = telemetry::recent_events(20);

        tokio::spawn(async move {
            for ev in recent {
                if let Ok(data) = serde_json::to_string(&ev) {
                    let msg = format!("event: audit\ndata: {data}\n\n");
                    if tx.send(Bytes::from(msg)).await.is_err() {
                        return;
                    }
                }
            }

            let mut ticker = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(2),
                Duration::from_secs(2),
            );

            loop {
                tokio::select! {
                    _ = tx.closed() => {
                        break;
                    }
                    recv_res = audit_rx.recv() => {
                        match recv_res {
                            Ok(ev) => {
                                if let Ok(data) = serde_json::to_string(&ev) {
                                    let msg = format!("event: audit\ndata: {data}\n\n");
                                    if tx.send(Bytes::from(msg)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        let stats = dto::build_sse_stats_event();
                        if let Ok(data) = serde_json::to_string(&stats) {
                            let msg = format!("event: stats\ndata: {data}\n\n");
                            if tx.send(Bytes::from(msg)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        let mut resp = Response::new(SseBody::new(rx).boxed());
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        resp.headers_mut().insert(
            hyper::header::CACHE_CONTROL,
            hyper::header::HeaderValue::from_static("no-cache"),
        );
        resp.headers_mut().insert(
            hyper::header::CONNECTION,
            hyper::header::HeaderValue::from_static("keep-alive"),
        );
        return Some(resp);
    }

    None
}
