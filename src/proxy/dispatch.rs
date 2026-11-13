use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::http::{HeaderMap, Method, Response, StatusCode, Uri};

use crate::observability::{
    AuditEvent, AuditQuota, AuditTokens, InFlightGuard, record_audit_event, record_no_replay_prevented,
};
use crate::proxy::body::{ProxyBody, StreamBody, StreamBodyParams, send_buffered, send_stream};
use crate::proxy::usage::UsageTap;
use crate::proxy::{
    RoutingEvent, attach_debug_headers, do_refresh_token, filepath_base, filter_request_headers, fmt_dur,
    get_backend_url, is_location_error, log_message, notify_audit, record_auth_error, record_model_capacity_error,
    record_model_latency, record_quota_error, record_soft_cooldown, record_success, record_validation_error,
    upstream_error_reason,
};
use crate::storage::{Account, LeaseGuard, STATUS_VERIFICATION_PENDING};

/// Result of an attempt on a candidate account.
pub enum Attempt {
    Failover { reason: &'static str, err: Option<String> },
    Done(Response<ProxyBody>),
}

/// Token usage metadata returned on successful generation requests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub prompt: i64,
    pub cached: i64,
    pub output: i64,
    pub thinking: i64,
}

impl Tokens {
    pub const fn zero() -> Self {
        Self {
            prompt: 0,
            cached: 0,
            output: 0,
            thinking: 0,
        }
    }

    pub fn effective_output(&self) -> i64 {
        self.output.max(self.thinking)
    }
}

/// Parameters for dispatching an HTTP attempt to a specific account.
pub struct DispatchParams<'a> {
    pub req_method: &'a Method,
    pub req_uri: &'a Uri,
    pub req_headers: &'a HeaderMap,
    pub path: &'a str,
    pub body: &'a [u8],
    pub model: &'a str,
    pub is_generation: bool,
    pub is_sse: bool,
    pub acc: &'a Account,
    pub client_ua: &'a str,
    pub attempt: usize,
    pub strategy: &'a str,
    pub affinity_status: &'a str,
    pub session_key: &'a str,
    pub has_failed_over: bool,
    pub frozen_event: Option<RoutingEvent>,
    pub in_flight_guard: Option<InFlightGuard>,
    pub lease_guard: Option<LeaseGuard>,
    pub request_id: &'a str,
}

fn audit_quota_from_routing_event(ev: &RoutingEvent) -> AuditQuota {
    let now = jiff::Timestamp::now();
    let r5_secs = ev.five_hour_reset_at.as_deref().and_then(|s| {
        s.parse::<jiff::Timestamp>().ok().map(|t| {
            let diff = t.duration_since(now);
            let secs = diff.as_secs();
            if secs > 0 { secs as u64 } else { 0 }
        })
    });
    let r7_secs = ev.weekly_reset_at.as_deref().and_then(|s| {
        s.parse::<jiff::Timestamp>().ok().map(|t| {
            let diff = t.duration_since(now);
            let secs = diff.as_secs();
            if secs > 0 { secs as u64 } else { 0 }
        })
    });
    AuditQuota {
        q5: ev.five_hour_remaining,
        q7: ev.weekly_remaining,
        r5_secs,
        r7_secs,
    }
}

#[allow(clippy::too_many_arguments)]
fn write_attempt_audit(
    params: &DispatchParams<'_>,
    disp_name: &str,
    status_code: u16,
    reason: &str,
    is_failover: bool,
    record_latency: bool,
    ttfb: Duration,
    total: Duration,
    tokens: AuditTokens,
) {
    let now = jiff::Timestamp::now();
    crate::dashboard::telemetry::record_attempt(
        crate::dashboard::telemetry::DashboardAuditEvent {
            id: 0,
            timestamp: now,
            model: params.model.to_string(),
            account: disp_name.to_string(),
            ttfb_ms: ttfb.as_millis() as u64,
            total_ms: total.as_millis() as u64,
            tokens: tokens.prompt.max(0),
            output_tokens: tokens.output.max(0),
            thinking_tokens: tokens.thinking.max(0),
            status: status_code,
            failover: is_failover,
        },
        record_latency,
    );

    if crate::observability::is_audit_worker_active() {
        let quota_before = params
            .frozen_event
            .as_ref()
            .map(audit_quota_from_routing_event)
            .unwrap_or(AuditQuota {
                q5: None,
                q7: None,
                r5_secs: None,
                r7_secs: None,
            });
        let path = params.path.split('?').next().unwrap_or(params.path);
        let event = AuditEvent {
            timestamp: now.to_string(),
            request_id: params.request_id.to_string(),
            session_key: crate::proxy::usage::session_tag(params.session_key),
            method: params.req_method.to_string(),
            path: path.to_string(),
            model: params.model.to_string(),
            strategy: params.strategy.to_string(),
            attempt: params.attempt,
            account_id: params.acc.id.clone(),
            account_name: disp_name.to_string(),
            status_code,
            ttfb_ms: ttfb.as_millis() as u64,
            total_ms: total.as_millis() as u64,
            tokens,
            quota_before,
            failover_reason: reason.to_string(),
        };
        record_audit_event(event);
    }
}

#[allow(clippy::too_many_arguments)]
fn record_attempt_audit(
    params: &DispatchParams<'_>,
    disp_name: &str,
    status_code: u16,
    reason: &str,
    is_failover: bool,
    ttfb: Duration,
    total: Duration,
    tokens: AuditTokens,
) {
    notify_audit(params.frozen_event.as_ref(), params.attempt, status_code, reason);
    write_attempt_audit(
        params,
        disp_name,
        status_code,
        reason,
        is_failover,
        false,
        ttfb,
        total,
        tokens,
    );
}

/// Dispatches a request to one specific account.
/// Implements the exact dispatch decision table from internal/proxy/proxy.go:
/// - Token refresh failure -> failover
/// - Upstream generation connect failure (ConnectError) -> failover "PreConnectTransportFailure"
/// - Upstream generation send failure (SendError) -> terminal 502/504 + record_no_replay_prevented
/// - Explicit retryable response status (capacity, validation, quota, auth, location, 503) -> failover
/// - Ordinary status code or read error -> terminal
pub async fn dispatch_account(params: DispatchParams<'_>) -> (Attempt, Option<Tokens>) {
    let disp_name = crate::accounts::display_account_name(Some(params.acc), Some(false));
    let path_base = filepath_base(params.path);

    // 1. Refresh token
    let force_token = params.is_generation && params.acc.status == STATUS_VERIFICATION_PENDING;
    let access_token = match do_refresh_token(params.acc, force_token) {
        Ok(tok) => tok,
        Err(err) => {
            log_message(
                "PROXY WARN",
                &format!("Token refresh failed for {disp_name}: {err}. Switching to next account..."),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                401,
                "TokenRefreshFailed",
                true,
                Duration::ZERO,
                Duration::ZERO,
                AuditTokens::zero(),
            );
            if let Err(persist_err) = record_auth_error(params.acc) {
                log_message(
                    "PROXY WARN",
                    &format!("failed to persist auth restriction: {persist_err}; continuing failover"),
                );
            }
            return (
                Attempt::Failover {
                    reason: "TokenRefreshFailed",
                    err: Some(err.to_string()),
                },
                None,
            );
        }
    };

    let target_url = format!("{}{}", get_backend_url(), params.path);
    let filtered_headers = filter_request_headers(
        params.req_headers,
        &access_token,
        params.client_ua,
        params.is_generation,
    );

    let uri: Uri = match target_url.parse() {
        Ok(u) => u,
        Err(e) => {
            let mut h = HeaderMap::new();
            attach_debug_headers(&mut h, Some(params.acc), params.strategy, params.affinity_status);
            let resp = send_buffered(StatusCode::BAD_REQUEST, &h, Bytes::from(format!("Invalid URL: {e}\n")));
            return (Attempt::Done(resp), None);
        }
    };

    let (parts, incoming_body, ttfb, start) = if params.is_generation {
        // Generation requests MUST go through upstream::connect + Connected::send (never pooled_client)
        let connected = match crate::upstream::connect(&uri).await {
            Ok(c) => c,
            Err(connect_err) => {
                log_message(
                    "PROXY EXCEPTION",
                    &format!(
                        "{} {} -> {disp_name}: failed before connecting ({connect_err}); failing over",
                        params.req_method, path_base
                    ),
                );
                record_attempt_audit(
                    &params,
                    &disp_name,
                    502,
                    "PreConnectTransportFailure",
                    true,
                    Duration::ZERO,
                    Duration::ZERO,
                    AuditTokens::zero(),
                );
                return (
                    Attempt::Failover {
                        reason: "PreConnectTransportFailure",
                        err: Some(connect_err.to_string()),
                    },
                    None,
                );
            }
        };

        let mut up_req = hyper::http::Request::builder()
            .method(params.req_method.clone())
            .uri(&uri);
        for (k, v) in filtered_headers.iter() {
            up_req = up_req.header(k, v);
        }
        up_req = up_req.header(hyper::header::CONNECTION, "close");

        let body_full = http_body_util::Full::new(Bytes::copy_from_slice(params.body));
        let start = Instant::now();
        let send_res = connected.send(up_req.body(body_full).unwrap()).await;
        let ttfb = start.elapsed();

        match send_res {
            Ok(resp) => {
                let (p, b) = resp.into_parts();
                (p, b, ttfb, start)
            }
            Err(send_err) => {
                log_message(
                    "PROXY EXCEPTION",
                    &format!("{} {} -> {disp_name}: {send_err}", params.req_method, path_base),
                );
                record_attempt_audit(
                    &params,
                    &disp_name,
                    502,
                    "AmbiguousTransportFailure",
                    false,
                    ttfb,
                    start.elapsed(),
                    AuditTokens::zero(),
                );
                record_no_replay_prevented();

                let status = if crate::upstream::is_timeout(&send_err) {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                };
                let mut h = HeaderMap::new();
                attach_debug_headers(&mut h, Some(params.acc), params.strategy, params.affinity_status);
                let resp = send_buffered(status, &h, Bytes::from_static(b"Upstream request failed\n"));
                return (Attempt::Done(resp), None);
            }
        }
    } else {
        // Non-generation: pooled metadata client
        let mut up_req = hyper::http::Request::builder()
            .method(params.req_method.clone())
            .uri(&uri);
        for (k, v) in filtered_headers.iter() {
            up_req = up_req.header(k, v);
        }

        let body_full = http_body_util::Full::new(Bytes::copy_from_slice(params.body));
        let start = Instant::now();
        let send_res = crate::upstream::pooled_client()
            .request(up_req.body(body_full).unwrap())
            .await;
        let ttfb = start.elapsed();

        match send_res {
            Ok(resp) => {
                let (p, b) = resp.into_parts();
                (p, b, ttfb, start)
            }
            Err(err) => {
                if err.is_connect() {
                    log_message(
                        "PROXY EXCEPTION",
                        &format!(
                            "{} {} -> {disp_name}: failed before connecting ({err}); failing over",
                            params.req_method, path_base
                        ),
                    );
                    record_attempt_audit(
                        &params,
                        &disp_name,
                        502,
                        "PreConnectTransportFailure",
                        true,
                        Duration::ZERO,
                        Duration::ZERO,
                        AuditTokens::zero(),
                    );
                    return (
                        Attempt::Failover {
                            reason: "PreConnectTransportFailure",
                            err: Some(err.to_string()),
                        },
                        None,
                    );
                }
                log_message(
                    "PROXY EXCEPTION",
                    &format!("{} {} -> {disp_name}: {err}", params.req_method, path_base),
                );
                record_attempt_audit(
                    &params,
                    &disp_name,
                    502,
                    "AmbiguousTransportFailure",
                    false,
                    ttfb,
                    start.elapsed(),
                    AuditTokens::zero(),
                );
                record_no_replay_prevented();
                let err_str = err.to_string().to_ascii_lowercase();
                let status = if err_str.contains("timeout") || err_str.contains("deadline") {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                };
                let mut h = HeaderMap::new();
                attach_debug_headers(&mut h, Some(params.acc), params.strategy, params.affinity_status);
                let resp = send_buffered(status, &h, Bytes::from_static(b"Upstream request failed\n"));
                return (Attempt::Done(resp), None);
            }
        }
    };

    let status = parts.status;
    let status_code = status.as_u16();
    let audit_suffix = match &params.frozen_event {
        Some(ev) => format!(
            " [audit: strategy={} tier={} healthy={} reserve={}]",
            ev.strategy, ev.classification, ev.healthy_candidate_count, ev.reserve_candidate_count
        ),
        None => String::new(),
    };

    if status_code >= 400 {
        let err_body = incoming_body.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
        let reason = upstream_error_reason(&err_body);
        let reason_suffix = if !reason.is_empty() {
            format!(" {reason}")
        } else {
            String::new()
        };
        log_message(
            "PROXY ERROR",
            &format!(
                "{} {} -> {disp_name} HTTP {status_code}{reason_suffix} (ttfb {}){audit_suffix}",
                params.req_method,
                path_base,
                fmt_dur(ttfb)
            ),
        );

        // Failover on 503 or model capacity exhaustion when model is specified
        if (reason == "MODEL_CAPACITY_EXHAUSTED" || status == StatusCode::SERVICE_UNAVAILABLE)
            && !params.model.is_empty()
        {
            let cooldown = record_model_capacity_error(&params.acc.id, params.model);
            log_message(
                "FAILOVER",
                &format!(
                    "Account {disp_name} has no capacity for {} (HTTP {status_code})! Paused for this model for {:?}. Auto-switching to next account...",
                    params.model, cooldown
                ),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "ModelCapacityExhausted",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:ModelCapacityExhausted",
                    err: None,
                },
                None,
            );
        }

        // Failover on Google Security Validation Required
        if crate::auth::is_validation_error(status_code, &err_body) {
            let persist_err = record_validation_error(params.acc, Some(&err_body));
            if persist_err.is_err() {
                log_message(
                    "PROXY WARN",
                    "failed to persist validation restriction; continuing failover",
                );
            }
            log_message(
                "FAILOVER",
                &format!("Account {disp_name} requires account verification! Auto-switching to next account..."),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "ValidationError",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:ValidationError",
                    err: persist_err.err().map(|e| e.to_string()),
                },
                None,
            );
        }

        // Failover on upstream model capacity exhaustion (Google cluster overload)
        if crate::quota::is_model_capacity_error(status_code, &err_body) {
            let backoff = crate::proxy::model_cooldown::record_model_capacity_error(&params.acc.id, params.model);
            let backoff_secs = backoff.as_secs();
            log_message(
                "FAILOVER",
                &format!(
                    "Account {disp_name} hit upstream model capacity error for {}! Model paused for {backoff_secs}s. Auto-switching to next account...",
                    params.model
                ),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "ModelCapacityExhausted",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:ModelCapacityExhausted",
                    err: None,
                },
                None,
            );
        }

        // Failover on 429 or 403 quota exhaustion
        if crate::quota::is_quota_error(status_code, &err_body) {
            let (delay, persist_err) = match record_quota_error(params.acc, Some(&parts.headers), Some(&err_body)) {
                Ok(d) => (d, None),
                Err(e) => (300, Some(e)),
            };
            if persist_err.is_some() {
                log_message("PROXY WARN", "failed to persist quota cooldown; continuing failover");
            }
            log_message(
                "FAILOVER",
                &format!(
                    "Account {disp_name} hit rate limit/quota error! Cooldown {delay}s. Auto-switching to next account..."
                ),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "QuotaExhausted",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:QuotaExhausted",
                    err: persist_err.map(|e| e.to_string()),
                },
                None,
            );
        }

        // Failover on 401 or recognized auth failure
        if crate::auth::is_auth_error(status_code, &err_body) {
            let persist_err = record_auth_error(params.acc);
            if persist_err.is_err() {
                log_message("PROXY WARN", "failed to persist auth restriction; continuing failover");
            }
            log_message(
                "FAILOVER",
                &format!("Account {disp_name} auth/permission failure! Auto-switching to next account..."),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "AuthFailure",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:AuthFailure",
                    err: persist_err.err().map(|e| e.to_string()),
                },
                None,
            );
        }

        // Failover on Location / GeoIP Not Supported
        if is_location_error(status_code, &err_body) {
            let persist_err = record_soft_cooldown(params.acc);
            if persist_err.is_err() {
                log_message("PROXY WARN", "failed to persist location cooldown; continuing failover");
            }
            log_message(
                "FAILOVER",
                &format!(
                    "Account {disp_name} hit temporary location restriction (HTTP {status_code})! Auto-switching to next account (soft cooldown 30s)..."
                ),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "LocationError",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:LocationError",
                    err: persist_err.err().map(|e| e.to_string()),
                },
                None,
            );
        }

        // Failover on 503 for requests without a model
        if status == StatusCode::SERVICE_UNAVAILABLE {
            let persist_err = record_soft_cooldown(params.acc);
            if persist_err.is_err() {
                log_message(
                    "PROXY WARN",
                    "failed to persist unavailable cooldown; continuing failover",
                );
            }
            log_message(
                "FAILOVER",
                &format!(
                    "Account {disp_name} upstream unavailable (HTTP 503)! Auto-switching to next account (soft cooldown 30s)..."
                ),
            );
            record_attempt_audit(
                &params,
                &disp_name,
                status_code,
                "UpstreamUnavailable",
                true,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            return (
                Attempt::Failover {
                    reason: "ExplicitRetryableUpstreamResponse:UpstreamUnavailable",
                    err: persist_err.err().map(|e| e.to_string()),
                },
                None,
            );
        }

        // Ordinary upstream error (400, 404, 500, etc.): Forward directly, DO NOT REPLAY!
        record_attempt_audit(
            &params,
            &disp_name,
            status_code,
            "NonRetryableError",
            false,
            ttfb,
            start.elapsed(),
            AuditTokens::zero(),
        );
        let mut out_headers = parts.headers;
        attach_debug_headers(
            &mut out_headers,
            Some(params.acc),
            params.strategy,
            params.affinity_status,
        );
        let resp = send_buffered(status, &out_headers, err_body);
        return (Attempt::Done(resp), None);
    }

    // 2xx response
    notify_audit(params.frozen_event.as_ref(), params.attempt, status_code, "");
    if params.is_generation {
        record_model_latency(Some(params.acc), params.model, ttfb);
    }

    let resp_content_type = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_stream = params.is_sse || resp_content_type.contains("text/event-stream");

    if is_stream {
        if let Err(persist_err) = record_success(params.acc, params.is_generation) {
            log_message(
                "PROXY WARN",
                &format!("failed to persist success bookkeeping: {persist_err}"),
            );
        }
        let mut out_headers = parts.headers;
        attach_debug_headers(
            &mut out_headers,
            Some(params.acc),
            params.strategy,
            params.affinity_status,
        );
        let expected_len = out_headers
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());

        let quota_before = params
            .frozen_event
            .as_ref()
            .map(audit_quota_from_routing_event)
            .unwrap_or(AuditQuota {
                q5: None,
                q7: None,
                r5_secs: None,
                r7_secs: None,
            });

        let stream_params = StreamBodyParams {
            is_generation: params.is_generation,
            method: params.req_method.to_string(),
            path_base,
            disp_name,
            status: status_code,
            ttfb,
            start_time: start,
            audit_suffix,
            session_key: params.session_key.to_string(),
            account_id: params.acc.id.clone(),
            model: params.model.to_string(),
            has_failed_over: params.has_failed_over,
            affinity_status: params.affinity_status.to_string(),
            expected_len,
            in_flight_guard: params.in_flight_guard,
            lease_guard: params.lease_guard,
            request_id: params.request_id.to_string(),
            strategy: params.strategy.to_string(),
            path: params.path.to_string(),
            attempt: params.attempt,
            quota_before,
        };
        let stream_body = StreamBody::new(incoming_body, stream_params);
        let resp = send_stream(status, &out_headers, stream_body.boxed());
        return (Attempt::Done(resp), None);
    }

    // Buffered 2xx response
    log_message(
        "PROXY",
        &format!(
            "{} {} -> {disp_name} (Status: {status_code}, ttfb {}){audit_suffix}",
            params.req_method,
            path_base,
            fmt_dur(ttfb)
        ),
    );

    let expected_len = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());

    let resp_body = match incoming_body.collect().await {
        Ok(c) => {
            let b = c.to_bytes();
            if let Some(expected) = expected_len
                && b.len() < expected
            {
                log_message(
                    "PROXY EXCEPTION",
                    &format!(
                        "{} {} -> {disp_name}: read error: unexpected EOF (got {} bytes, want {expected})",
                        params.req_method,
                        path_base,
                        b.len()
                    ),
                );
                record_no_replay_prevented();
                write_attempt_audit(
                    &params,
                    &disp_name,
                    StatusCode::BAD_GATEWAY.as_u16(),
                    "UnexpectedEof",
                    false,
                    false,
                    ttfb,
                    start.elapsed(),
                    AuditTokens::zero(),
                );
                let mut out_headers = HeaderMap::new();
                attach_debug_headers(
                    &mut out_headers,
                    Some(params.acc),
                    params.strategy,
                    params.affinity_status,
                );
                let resp = send_buffered(
                    StatusCode::BAD_GATEWAY,
                    &out_headers,
                    Bytes::from_static(b"Upstream request failed\n"),
                );
                return (Attempt::Done(resp), None);
            }
            b
        }
        Err(read_err) => {
            log_message(
                "PROXY EXCEPTION",
                &format!(
                    "{} {} -> {disp_name}: read error: {read_err}",
                    params.req_method, path_base
                ),
            );
            record_no_replay_prevented();
            let mut out_headers = HeaderMap::new();
            attach_debug_headers(
                &mut out_headers,
                Some(params.acc),
                params.strategy,
                params.affinity_status,
            );
            let err_status = if read_err.to_string().to_ascii_lowercase().contains("timeout") {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            write_attempt_audit(
                &params,
                &disp_name,
                err_status.as_u16(),
                "ReadError",
                false,
                false,
                ttfb,
                start.elapsed(),
                AuditTokens::zero(),
            );
            let resp = send_buffered(
                err_status,
                &out_headers,
                Bytes::from_static(b"Upstream request failed\n"),
            );
            return (Attempt::Done(resp), None);
        }
    };

    if let Err(persist_err) = record_success(params.acc, params.is_generation) {
        log_message(
            "PROXY WARN",
            &format!("failed to persist success bookkeeping: {persist_err}"),
        );
    }
    let mut out_headers = parts.headers;
    attach_debug_headers(
        &mut out_headers,
        Some(params.acc),
        params.strategy,
        params.affinity_status,
    );
    let resp = send_buffered(status, &out_headers, resp_body.clone());

    let mut usage = UsageTap::new(());
    usage.scan(&resp_body);
    let tokens = Tokens {
        prompt: usage.prompt,
        cached: usage.cached,
        output: usage.output,
        thinking: usage.thinking,
    };
    write_attempt_audit(
        &params,
        &disp_name,
        status_code,
        "",
        false,
        params.is_generation,
        ttfb,
        start.elapsed(),
        AuditTokens {
            prompt: tokens.prompt,
            cached: tokens.cached,
            output: tokens.output,
            thinking: tokens.thinking,
        },
    );
    (Attempt::Done(resp), Some(tokens))
}
