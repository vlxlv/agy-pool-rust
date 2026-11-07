use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Frame};
use hyper::http::{HeaderMap, StatusCode};

use crate::observability::{
    InFlightGuard, record_failover_success, record_generation_success, record_no_replay_prevented,
};
use crate::proxy::usage::UsageTap;
use crate::proxy::{affinity_manager, filter_response_headers, fmt_dur, log_message, session_tag};
use crate::storage::LeaseGuard;

/// Downstream proxy body type: boxed stream of Bytes with std::io::Error.
pub type ProxyBody = BoxBody<Bytes, std::io::Error>;

/// Creates a buffered ProxyBody from Bytes.
pub fn full_body(data: Bytes) -> ProxyBody {
    http_body_util::Full::new(data).map_err(|never| match never {}).boxed()
}

/// Creates an empty ProxyBody.
pub fn empty_body() -> ProxyBody {
    http_body_util::Empty::new().map_err(|never| match never {}).boxed()
}

/// SendBuffered constructs a buffered HTTP response matching Go's SendBuffered.
pub fn send_buffered(status: StatusCode, headers: &HeaderMap, body: Bytes) -> hyper::http::Response<ProxyBody> {
    let mut builder = hyper::http::Response::builder().status(status);
    let filtered = filter_response_headers(headers);
    for (k, v) in filtered.iter() {
        builder = builder.header(k, v);
    }
    builder = builder
        .header(hyper::header::CONTENT_LENGTH, body.len().to_string())
        .header(hyper::header::CONNECTION, "close");
    builder.body(full_body(body)).unwrap()
}

/// SendStream constructs a chunked streaming HTTP response matching Go's SendStream.
pub fn send_stream(status: StatusCode, headers: &HeaderMap, body: ProxyBody) -> hyper::http::Response<ProxyBody> {
    let mut builder = hyper::http::Response::builder().status(status);
    let filtered = filter_response_headers(headers);
    for (k, v) in filtered.iter() {
        builder = builder.header(k, v);
    }
    builder = builder
        .header(hyper::header::TRANSFER_ENCODING, "chunked")
        .header(hyper::header::CONNECTION, "close");
    builder.body(body).unwrap()
}

/// Parameters configuring a StreamBody.
pub struct StreamBodyParams {
    pub is_generation: bool,
    pub method: String,
    pub path_base: String,
    pub disp_name: String,
    pub status: u16,
    pub ttfb: Duration,
    pub start_time: Instant,
    pub audit_suffix: String,
    pub session_key: String,
    pub account_id: String,
    pub model: String,
    pub has_failed_over: bool,
    pub affinity_status: String,
    pub expected_len: Option<usize>,
    pub in_flight_guard: Option<InFlightGuard>,
    pub lease_guard: Option<LeaseGuard>,
    pub request_id: String,
    pub strategy: String,
    pub path: String,
    pub attempt: usize,
    pub quota_before: crate::observability::AuditQuota,
}

/// StreamBody wraps an incoming upstream streaming body, tapping usage, enforcing Content-Length,
/// logging end-of-stream timing lines, updating success metrics, and holding RAII guards across the stream.
pub struct StreamBody<B> {
    inner: B,
    usage: UsageTap<()>,
    params: StreamBodyParams,
    sent_bytes: usize,
    completed: bool,
}

impl<B> StreamBody<B> {
    pub fn new(inner: B, params: StreamBodyParams) -> Self {
        Self {
            inner,
            usage: UsageTap::new(()),
            params,
            sent_bytes: 0,
            completed: false,
        }
    }
}

impl<B> Body for StreamBody<B>
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, std::io::Error>>> {
        if self.completed {
            return Poll::Ready(None);
        }

        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if frame.is_data() {
                    let data = frame.into_data().unwrap();
                    self.sent_bytes += data.len();
                    self.usage.scan(&data);
                    Poll::Ready(Some(Ok(Frame::data(data))))
                } else {
                    Poll::Ready(Some(Ok(frame)))
                }
            }
            Poll::Ready(Some(Err(err))) => {
                self.completed = true;
                log_message("STREAM TRUNCATED", &err.to_string());
                record_no_replay_prevented();
                if crate::observability::is_audit_worker_active() {
                    crate::observability::record_audit_event(crate::observability::AuditEvent {
                        timestamp: jiff::Timestamp::now().to_string(),
                        request_id: self.params.request_id.clone(),
                        session_key: self.params.session_key.clone(),
                        method: self.params.method.clone(),
                        path: self.params.path.clone(),
                        model: self.params.model.clone(),
                        strategy: self.params.strategy.clone(),
                        attempt: self.params.attempt,
                        account_id: self.params.account_id.clone(),
                        account_name: self.params.disp_name.clone(),
                        status_code: self.params.status,
                        ttfb_ms: self.params.ttfb.as_millis() as u64,
                        total_ms: self.params.start_time.elapsed().as_millis() as u64,
                        tokens: crate::observability::AuditTokens {
                            prompt: self.usage.prompt,
                            cached: self.usage.cached,
                        },
                        quota_before: self.params.quota_before.clone(),
                        failover_reason: "StreamTruncated".to_string(),
                    });
                }
                Poll::Ready(Some(Err(std::io::Error::other(err.to_string()))))
            }
            Poll::Ready(None) => {
                self.completed = true;
                if let Some(expected) = self.params.expected_len
                    && self.sent_bytes != expected
                {
                    let msg = format!("expected {} bytes, sent {}", expected, self.sent_bytes);
                    log_message("STREAM TRUNCATED", &msg);
                    record_no_replay_prevented();
                    if crate::observability::is_audit_worker_active() {
                        crate::observability::record_audit_event(crate::observability::AuditEvent {
                            timestamp: jiff::Timestamp::now().to_string(),
                            request_id: self.params.request_id.clone(),
                            session_key: self.params.session_key.clone(),
                            method: self.params.method.clone(),
                            path: self.params.path.clone(),
                            model: self.params.model.clone(),
                            strategy: self.params.strategy.clone(),
                            attempt: self.params.attempt,
                            account_id: self.params.account_id.clone(),
                            account_name: self.params.disp_name.clone(),
                            status_code: self.params.status,
                            ttfb_ms: self.params.ttfb.as_millis() as u64,
                            total_ms: self.params.start_time.elapsed().as_millis() as u64,
                            tokens: crate::observability::AuditTokens {
                                prompt: self.usage.prompt,
                                cached: self.usage.cached,
                            },
                            quota_before: self.params.quota_before.clone(),
                            failover_reason: "StreamTruncatedEof".to_string(),
                        });
                    }
                    return Poll::Ready(Some(Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, msg))));
                }

                let total = self.params.start_time.elapsed();
                log_message(
                    "PROXY",
                    &format!(
                        "{} {} -> {} (Status: {}, ttfb {}, total {}){}",
                        self.params.method,
                        self.params.path_base,
                        self.params.disp_name,
                        self.params.status,
                        fmt_dur(self.params.ttfb),
                        fmt_dur(total),
                        self.params.audit_suffix
                    ),
                );

                if crate::observability::is_audit_worker_active() {
                    crate::observability::record_audit_event(crate::observability::AuditEvent {
                        timestamp: jiff::Timestamp::now().to_string(),
                        request_id: self.params.request_id.clone(),
                        session_key: self.params.session_key.clone(),
                        method: self.params.method.clone(),
                        path: self.params.path.clone(),
                        model: self.params.model.clone(),
                        strategy: self.params.strategy.clone(),
                        attempt: self.params.attempt,
                        account_id: self.params.account_id.clone(),
                        account_name: self.params.disp_name.clone(),
                        status_code: self.params.status,
                        ttfb_ms: self.params.ttfb.as_millis() as u64,
                        total_ms: total.as_millis() as u64,
                        tokens: crate::observability::AuditTokens {
                            prompt: self.usage.prompt,
                            cached: self.usage.cached,
                        },
                        quota_before: self.params.quota_before.clone(),
                        failover_reason: String::new(),
                    });
                }

                if self.params.is_generation {
                    record_generation_success();
                    if self.params.has_failed_over {
                        record_failover_success();
                    }
                    if !self.params.session_key.is_empty()
                        && !self.params.account_id.is_empty()
                        && self.params.affinity_status != "spill"
                    {
                        affinity_manager().set(
                            &self.params.session_key,
                            &self.params.account_id,
                            std::time::SystemTime::now(),
                        );
                    }
                    if self.usage.prompt > 0 {
                        let cached = if self.usage.cached >= 0 {
                            format!(
                                "{} ({}%)",
                                self.usage.cached,
                                self.usage.cached * 100 / self.usage.prompt
                            )
                        } else {
                            "not reported".to_string()
                        };
                        log_message(
                            "CACHE",
                            &format!(
                                "{} session {} -> {}: prompt {}, cached {}",
                                self.params.model,
                                session_tag(&self.params.session_key),
                                self.params.disp_name,
                                self.usage.prompt,
                                cached
                            ),
                        );
                    }
                }

                Poll::Ready(None)
            }
        }
    }
}
