pub mod dto;
pub mod telemetry;

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::{Method, Request, Response, StatusCode};

use crate::proxy::body::{ProxyBody, full_body};

pub fn dashboard_html() -> &'static str {
    include_str!("assets/index.html")
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
    resp.headers_mut()
        .insert(hyper::header::ALLOW, hyper::header::HeaderValue::from_static("GET"));
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn forbidden() -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(b"Forbidden\n")));
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn internal_error() -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(b"Internal Server Error\n")));
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

fn parse_authority(authority: &str) -> Option<(&str, Option<u16>)> {
    if authority.is_empty()
        || authority
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'@' | b'/' | b'\\' | b'?' | b'#'))
    {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let close_idx = rest.find(']')?;
        let host = &rest[..close_idx];
        host.parse::<std::net::Ipv6Addr>().ok()?;
        let after = &rest[close_idx + 1..];
        if after.is_empty() {
            return Some((host, None));
        }
        let port_str = after.strip_prefix(':')?;
        if port_str.is_empty() {
            return None;
        }
        let port = port_str.parse::<u16>().ok()?;
        return Some((host, Some(port)));
    }

    if authority.parse::<std::net::IpAddr>().is_ok() {
        return Some((authority, None));
    }

    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if host.contains(':') || host.is_empty() || port_str.is_empty() {
            return None;
        }
        let port = port_str.parse::<u16>().ok()?;
        Some((host, Some(port)))
    } else {
        Some((authority, None))
    }
}

fn is_allowed_hostname(host: &str) -> bool {
    // Numeric IP literals (127.0.0.1, ::1, Tailscale 100.x.y.z / fd7a::, LAN IPs) are immune to DNS rebinding.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return true;
    }
    if normalized.ends_with(".ts.net") && normalized.len() > ".ts.net".len() {
        return true;
    }
    let (listen, _) = dto::get_runtime_meta();
    if let Some((listen_host, _)) = parse_authority(&listen) {
        let listen_norm = listen_host.trim_end_matches('.');
        if !listen_norm.is_empty() && normalized.eq_ignore_ascii_case(listen_norm) {
            return true;
        }
    }
    false
}

fn is_valid_host_and_origin(req: &Request<hyper::body::Incoming>, is_html_document: bool) -> bool {
    let host_headers: Vec<_> = req.headers().get_all(hyper::header::HOST).iter().collect();
    if host_headers.len() > 1 {
        return false;
    }
    let authority_str = if let Some(h) = host_headers.first() {
        let Ok(s) = h.to_str() else {
            return false;
        };
        s
    } else if let Some(auth) = req.uri().authority() {
        auth.as_str()
    } else {
        return false;
    };

    let Some((req_host, req_port)) = parse_authority(authority_str) else {
        return false;
    };
    if !is_allowed_hostname(req_host) {
        return false;
    }

    let origin_headers: Vec<_> = req.headers().get_all(hyper::header::ORIGIN).iter().collect();
    if origin_headers.len() > 1 {
        return false;
    }
    if let Some(origin_val) = origin_headers.first() {
        let Ok(origin_str) = origin_val.to_str() else {
            return false;
        };
        let Ok(origin_url) = url::Url::parse(origin_str) else {
            return false;
        };
        if !matches!(origin_url.scheme(), "http" | "https") {
            return false;
        }
        let Some(origin_host_raw) = origin_url.host_str() else {
            return false;
        };
        let origin_host = origin_host_raw.trim_start_matches('[').trim_end_matches(']');
        let hosts_match = match (
            origin_host.parse::<std::net::IpAddr>(),
            req_host.parse::<std::net::IpAddr>(),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => origin_host
                .trim_end_matches('.')
                .eq_ignore_ascii_case(req_host.trim_end_matches('.')),
        };
        if !hosts_match {
            return false;
        }
        let port_matches = match req_port {
            Some(p) => origin_url.port_or_known_default() == Some(p),
            None => origin_url.port().is_none(),
        };
        if !port_matches {
            return false;
        }
    }

    if let Some(fetch_site) = req.headers().get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && fetch_site.trim().eq_ignore_ascii_case("cross-site")
    {
        let is_top_level_navigate = is_html_document
            && req
                .headers()
                .get("sec-fetch-mode")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|m| m.trim().eq_ignore_ascii_case("navigate"))
            && req
                .headers()
                .get("sec-fetch-dest")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|d| d.trim().eq_ignore_ascii_case("document"));
        if !is_top_level_navigate {
            return false;
        }
    }

    true
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

pub async fn try_handle_request(req: &Request<hyper::body::Incoming>) -> Option<Response<ProxyBody>> {
    let path = req.uri().path();
    let is_root = path == "/";
    let is_dashboard_path = matches!(path, "/dashboard" | "/dashboard/");
    let is_status = matches!(path, "/api/v1/status" | "/api/v1/status/");
    let is_accounts = matches!(path, "/api/v1/accounts" | "/api/v1/accounts/");
    let is_forecast = matches!(path, "/api/v1/forecast" | "/api/v1/forecast/");
    let is_events = matches!(path, "/api/v1/events" | "/api/v1/events/");

    let is_dashboard_only = is_dashboard_path || is_status || is_accounts || is_forecast || is_events;
    let is_dashboard = (is_root && req.method() == Method::GET) || is_dashboard_path;

    if !is_dashboard && !is_dashboard_only {
        return None;
    }

    if !is_valid_host_and_origin(req, is_dashboard) {
        return Some(forbidden());
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
        let Ok(status) = tokio::task::spawn_blocking(dto::build_status_response).await else {
            return Some(internal_error());
        };
        let bytes = serde_json::to_vec(&status).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_accounts {
        let request_show_email = req
            .uri()
            .query()
            .map(|q| {
                url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "show_email" && (v == "true" || v == "1"))
            })
            .unwrap_or(false);
        let Ok(accounts) = tokio::task::spawn_blocking(move || {
            let pool = crate::storage::load_pool().unwrap_or_default();
            let now_ts = jiff::Timestamp::now().as_second() as f64;
            dto::build_accounts_response(&pool, request_show_email, now_ts)
        })
        .await
        else {
            return Some(internal_error());
        };
        let bytes = serde_json::to_vec(&accounts).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_forecast {
        let Ok(forecast) = tokio::task::spawn_blocking(|| {
            let pool = crate::storage::load_pool().unwrap_or_default();
            let now_ts = jiff::Timestamp::now().as_second() as f64;
            dto::build_forecast_response(&pool, now_ts)
        })
        .await
        else {
            return Some(internal_error());
        };
        let bytes = serde_json::to_vec(&forecast).unwrap_or_default();
        return Some(json_response(bytes));
    }

    if is_events {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
        let (recent, mut audit_rx) = telemetry::subscribe_with_replay(20);

        tokio::spawn(async move {
            let mut last_replayed_id = 0u64;
            for ev in recent {
                last_replayed_id = last_replayed_id.max(ev.id);
                if let Ok(data) = serde_json::to_string(&ev) {
                    let msg = format!("id: {}\nevent: audit\ndata: {data}\n\n", ev.id);
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
                                if ev.id <= last_replayed_id {
                                    continue;
                                }
                                last_replayed_id = ev.id;
                                if let Ok(data) = serde_json::to_string(&ev) {
                                    let msg = format!("id: {}\nevent: audit\ndata: {data}\n\n", ev.id);
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
