pub mod affinity;
pub mod body;
pub mod cloak;
pub mod confirmation;
pub mod dispatch;
pub mod failover;
pub mod keep_alive;
pub mod model_cooldown;
pub mod model_mapping;
pub mod smooth_rate;
pub mod usage;

pub use affinity::*;
pub use body::*;
pub use cloak::*;
pub use confirmation::*;
pub use dispatch::*;
pub use failover::*;
pub use keep_alive::*;
pub use model_cooldown::*;
pub use model_mapping::*;
pub use smooth_rate::*;
pub use usage::*;

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use hyper::body::Body;
use hyper::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::accounts::display_account_name;
use crate::observability::{format_prometheus_metrics, record_generation_request, record_restricted_skip};
use crate::storage::{Account, Pool};

// ---------------------------------------------------------------------------
// Audit Events and Observation
// ---------------------------------------------------------------------------

pub mod time_serde {
    use super::*;
    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let ts = jiff::Timestamp::try_from(*time).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&ts.to_string())
    }
    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let ts = s.parse::<jiff::Timestamp>().map_err(serde::de::Error::custom)?;
        let secs = ts.as_second();
        let nanos = ts.subsec_nanosecond();
        if secs >= 0 {
            Ok(SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nanos as u32))
        } else {
            Ok(SystemTime::UNIX_EPOCH - Duration::from_secs((-secs) as u64))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEvent {
    #[serde(with = "time_serde")]
    pub timestamp: SystemTime,
    pub strategy: String,
    pub account: String,
    pub account_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub classification: String,
    pub healthy_candidate_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub healthy_candidates: Vec<String>,
    pub reserve_candidate_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reserve_candidates: Vec<String>,

    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub five_hour_known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_reset_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour_pace: Option<f64>,

    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub weekly_known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_reset_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_pace: Option<f64>,

    pub known_window_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_rank: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worst_pace: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pace: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_floor: Option<f64>,

    pub attempt: usize,
    pub status: u16,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub failover_reason: String,
    pub path: String,
}

pub const DEFAULT_AUDIT_BUFFER_SIZE: usize = 1024;

struct Subscriber {
    id: usize,
    tx: tokio::sync::mpsc::Sender<RoutingEvent>,
}

pub type LegacyAuditObserver = Arc<dyn Fn(&str, i32, i32, &str, &str) + Send + Sync>;
pub type RichAuditObserver = Arc<dyn Fn(RoutingEvent) + Send + Sync>;

struct AuditState {
    next_id: usize,
    subscribers: Vec<Subscriber>,
    session_dropped: i64,
    total_dropped: i64,
    active_auditors: i32,
    legacy_observer: Option<LegacyAuditObserver>,
    rich_observer: Option<RichAuditObserver>,
}

static AUDIT_STATE: LazyLock<RwLock<AuditState>> = LazyLock::new(|| {
    RwLock::new(AuditState {
        next_id: 1,
        subscribers: Vec::new(),
        session_dropped: 0,
        total_dropped: 0,
        active_auditors: 0,
        legacy_observer: None,
        rich_observer: None,
    })
});

pub struct AuditSubscription {
    id: usize,
}

impl Drop for AuditSubscription {
    fn drop(&mut self) {
        let mut state = AUDIT_STATE.write().unwrap();
        state.subscribers.retain(|s| s.id != self.id);
        update_active_auditors_locked(&mut state);
    }
}

fn update_active_auditors_locked(state: &mut AuditState) {
    let mut count = state.subscribers.len() as i32;
    if state.legacy_observer.is_some() {
        count += 1;
    }
    if state.rich_observer.is_some() {
        count += 1;
    }
    state.active_auditors = count;
}

pub fn has_active_auditor() -> bool {
    let state = AUDIT_STATE.read().unwrap();
    state.active_auditors > 0
}

pub fn subscribe_audit(buf_size: usize) -> (tokio::sync::mpsc::Receiver<RoutingEvent>, AuditSubscription) {
    let cap = if buf_size == 0 {
        DEFAULT_AUDIT_BUFFER_SIZE
    } else {
        buf_size
    };
    let (tx, rx) = tokio::sync::mpsc::channel(cap);
    let mut state = AUDIT_STATE.write().unwrap();
    state.session_dropped = 0;
    let id = state.next_id;
    state.next_id += 1;
    state.subscribers.push(Subscriber { id, tx });
    update_active_auditors_locked(&mut state);
    (rx, AuditSubscription { id })
}

pub fn dropped_audit_events() -> i64 {
    let state = AUDIT_STATE.read().unwrap();
    state.session_dropped
}

pub fn reset_dropped_audit_events() {
    let mut state = AUDIT_STATE.write().unwrap();
    state.session_dropped = 0;
    state.total_dropped = 0;
}

pub fn audit_queue_len() -> usize {
    let state = AUDIT_STATE.read().unwrap();
    state
        .subscribers
        .iter()
        .map(|s| s.tx.max_capacity() - s.tx.capacity())
        .sum()
}

pub fn set_audit_observer<F>(f: Option<F>)
where
    F: Fn(&str, i32, i32, &str, &str) + Send + Sync + 'static,
{
    let mut state = AUDIT_STATE.write().unwrap();
    state.legacy_observer = f.map(|cb| Arc::new(cb) as LegacyAuditObserver);
    update_active_auditors_locked(&mut state);
}

pub fn set_audit_observer_event<F>(f: Option<F>)
where
    F: Fn(RoutingEvent) + Send + Sync + 'static,
{
    let mut state = AUDIT_STATE.write().unwrap();
    state.rich_observer = f.map(|cb| Arc::new(cb) as RichAuditObserver);
    update_active_auditors_locked(&mut state);
}

pub fn emit_routing_event(ev: RoutingEvent) -> bool {
    if !has_active_auditor() {
        return false;
    }
    let mut state = AUDIT_STATE.write().unwrap();
    if state.active_auditors <= 0 {
        return false;
    }

    let mut delivered = true;
    let mut dropped_count = 0;
    for sub in &state.subscribers {
        if sub.tx.try_send(ev.clone()).is_err() {
            dropped_count += 1;
            delivered = false;
        }
    }
    if dropped_count > 0 {
        state.session_dropped += dropped_count;
        state.total_dropped += dropped_count;
    }

    if let Some(obs) = &state.legacy_observer {
        let obs = obs.clone();
        let ev = ev.clone();
        tokio::spawn(async move {
            obs(
                &ev.account_name,
                ev.status as i32,
                ev.attempt as i32,
                &ev.failover_reason,
                &ev.path,
            );
        });
    }
    if let Some(obs_ev) = &state.rich_observer {
        let obs_ev = obs_ev.clone();
        tokio::spawn(async move {
            obs_ev(ev);
        });
    }

    delivered
}

pub fn notify_audit(frozen: Option<&RoutingEvent>, attempt: usize, status: u16, reason: &str) {
    let Some(base) = frozen else { return };
    if !has_active_auditor() {
        return;
    }
    let mut ev = base.clone();
    ev.timestamp = crate::clock::now();
    ev.attempt = attempt;
    ev.status = status;
    ev.failover_reason = reason.to_string();
    emit_routing_event(ev);
}

pub struct SelectionAuditContext {
    pub strategy: String,
    pub now: f64,
    pub healthy_candidates: Vec<String>,
    pub reserve_candidates: Vec<String>,
}

pub fn build_selection_audit_context(pool: &Pool, strategy: &str, now: f64) -> SelectionAuditContext {
    let mut ctx = SelectionAuditContext {
        strategy: strategy.to_string(),
        now,
        healthy_candidates: Vec::new(),
        reserve_candidates: Vec::new(),
    };
    for cand in &pool.accounts {
        if crate::scheduler::is_restricted(cand) {
            continue;
        }
        if cand.rate_limited_until.map(|u| u > now).unwrap_or(false) {
            continue;
        }
        let cand_cap = crate::quota::compute_capacity_state(cand, now);
        if cand_cap.is_depleted {
            continue;
        }
        let disp = display_account_name(Some(cand), Some(false));
        if crate::scheduler::is_reserve_capacity(&cand_cap) {
            ctx.reserve_candidates.push(disp);
        } else if cand_cap.known_window_count > 0 || cand_cap.legacy_fraction_known {
            ctx.healthy_candidates.push(disp);
        }
    }
    ctx
}

pub fn build_frozen_event(acc: &Account, path: &str, audit_ctx: &SelectionAuditContext) -> RoutingEvent {
    let cand_cap = crate::quota::compute_capacity_state(acc, audit_ctx.now);
    let freshness = crate::quota::freshness_rank(acc, audit_ctx.now);
    let disp_name = display_account_name(Some(acc), Some(false));

    let worst_pace = if cand_cap.worst_pace.is_finite() {
        Some(cand_cap.worst_pace)
    } else {
        None
    };
    let total_pace = if cand_cap.total_pace.is_finite() {
        Some(cand_cap.total_pace)
    } else {
        None
    };
    let raw_floor = Some(cand_cap.raw_floor);

    let classification = if cand_cap.is_depleted {
        "Depleted"
    } else if crate::scheduler::is_reserve_capacity(&cand_cap) {
        "Reserve"
    } else if cand_cap.known_window_count > 0 || cand_cap.legacy_fraction_known {
        "Healthy"
    } else {
        "Unknown"
    };

    let r5 = if cand_cap.q5_known { Some(cand_cap.r5) } else { None };
    let r7 = if cand_cap.q7_known { Some(cand_cap.r7) } else { None };

    RoutingEvent {
        timestamp: crate::clock::now(),
        strategy: audit_ctx.strategy.clone(),
        account: disp_name.clone(),
        account_name: disp_name,
        account_id: Some(acc.id.clone()),
        classification: classification.to_string(),
        healthy_candidate_count: audit_ctx.healthy_candidates.len(),
        healthy_candidates: audit_ctx.healthy_candidates.clone(),
        reserve_candidate_count: audit_ctx.reserve_candidates.len(),
        reserve_candidates: audit_ctx.reserve_candidates.clone(),

        five_hour_known: cand_cap.q5_known,
        five_hour_remaining: cand_cap.q5,
        five_hour_reset_at: cand_cap
            .reset5_time
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(|ts| ts.to_string()),
        five_hour_reset_ratio: r5,
        five_hour_pace: cand_cap.pace5,

        weekly_known: cand_cap.q7_known,
        weekly_remaining: cand_cap.q7,
        weekly_reset_at: cand_cap
            .reset7_time
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(|ts| ts.to_string()),
        weekly_reset_ratio: r7,
        weekly_pace: cand_cap.pace7,

        known_window_count: cand_cap.known_window_count,
        freshness_rank: Some(freshness),
        worst_pace,
        total_pace,
        raw_floor,

        attempt: 1,
        status: 200,
        failover_reason: String::new(),
        path: path.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Configurable Providers / Hooks
// ---------------------------------------------------------------------------

static BACKEND_HOST_OVERRIDE: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));
static BACKEND_URL_OVERRIDE: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));
static USER_AGENT_OVERRIDE: LazyLock<RwLock<Option<String>>> = LazyLock::new(|| RwLock::new(None));

pub type TokenRefresherFn = Arc<dyn Fn(&Account, bool) -> Result<String, crate::auth::AuthError> + Send + Sync>;
static TOKEN_REFRESHER: LazyLock<RwLock<Option<TokenRefresherFn>>> = LazyLock::new(|| RwLock::new(None));

pub type QuotaRefresherFn = Arc<dyn Fn(&Account) + Send + Sync>;
static QUOTA_REFRESHER: LazyLock<RwLock<Option<QuotaRefresherFn>>> = LazyLock::new(|| RwLock::new(None));

// Installed once by the daemon, like Go's wireLogRotator sync.Once: tests run several daemons in one process.
static LOG_ROTATOR: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

pub fn set_log_rotator(f: fn()) {
    let _ = LOG_ROTATOR.set(f);
}

#[cfg(test)]
pub fn log_rotator_installed() -> bool {
    LOG_ROTATOR.get().is_some()
}

pub fn get_backend_host() -> String {
    if let Some(h) = BACKEND_HOST_OVERRIDE.read().unwrap().as_ref() {
        return h.clone();
    }
    crate::quota::BACKEND_HOST.to_string()
}

pub fn backend_host() -> String {
    get_backend_host()
}

pub fn set_backend_host(host: &str) {
    *BACKEND_HOST_OVERRIDE.write().unwrap() = Some(host.to_string());
}

pub struct BackendHostGuard(Option<String>);
impl Drop for BackendHostGuard {
    fn drop(&mut self) {
        *BACKEND_HOST_OVERRIDE.write().unwrap() = self.0.take();
    }
}

pub fn set_backend_host_for_test(host: &str) -> BackendHostGuard {
    let mut w = BACKEND_HOST_OVERRIDE.write().unwrap();
    let old = w.clone();
    *w = Some(host.to_string());
    BackendHostGuard(old)
}

pub fn get_backend_url() -> String {
    if let Some(u) = BACKEND_URL_OVERRIDE.read().unwrap().as_ref() {
        return u.clone();
    }
    crate::quota::BACKEND_URL_BASE.to_string()
}

pub fn backend_url() -> String {
    get_backend_url()
}

pub fn set_backend_url(url: &str) {
    *BACKEND_URL_OVERRIDE.write().unwrap() = Some(url.to_string());
}

pub struct BackendUrlGuard(Option<String>, Option<String>);
impl Drop for BackendUrlGuard {
    fn drop(&mut self) {
        *BACKEND_URL_OVERRIDE.write().unwrap() = self.0.take();
        *BACKEND_HOST_OVERRIDE.write().unwrap() = self.1.take();
    }
}

pub fn set_backend_url_for_test(backend_url: &str) -> BackendUrlGuard {
    let old_url = BACKEND_URL_OVERRIDE.write().unwrap().clone();
    let old_host = BACKEND_HOST_OVERRIDE.write().unwrap().clone();
    set_backend_url(backend_url);
    if let Ok(u) = url::Url::parse(backend_url) {
        let host = u.host_str().unwrap_or("").to_string();
        let port_suffix = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        set_backend_host(&format!("{host}{port_suffix}"));
    }
    BackendUrlGuard(old_url, old_host)
}

static USER_AGENT_PROVIDER: std::sync::OnceLock<fn() -> String> = std::sync::OnceLock::new();
static PROVIDED_USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Installs Go's proxy.UserAgentProvider; like Go's sync.OnceValue it is evaluated once, on first use.
pub fn set_user_agent_provider(f: fn() -> String) {
    let _ = USER_AGENT_PROVIDER.set(f);
}

pub fn get_user_agent() -> String {
    if let Some(ua) = USER_AGENT_OVERRIDE.read().unwrap().as_ref() {
        return ua.clone();
    }
    if let Some(f) = USER_AGENT_PROVIDER.get() {
        return PROVIDED_USER_AGENT.get_or_init(f).clone();
    }
    crate::quota::DEFAULT_UA.to_string()
}

pub fn user_agent() -> String {
    get_user_agent()
}

pub struct UserAgentGuard(Option<String>);
impl Drop for UserAgentGuard {
    fn drop(&mut self) {
        *USER_AGENT_OVERRIDE.write().unwrap() = self.0.take();
    }
}

pub fn set_user_agent_for_test(ua: &str) -> UserAgentGuard {
    let mut w = USER_AGENT_OVERRIDE.write().unwrap();
    let old = w.clone();
    *w = Some(ua.to_string());
    UserAgentGuard(old)
}

pub fn set_token_refresher<F>(f: F)
where
    F: Fn(&Account) -> Result<String, crate::auth::AuthError> + Send + Sync + 'static,
{
    *TOKEN_REFRESHER.write().unwrap() = Some(Arc::new(move |acc, _force| f(acc)));
}

pub struct TokenRefresherGuard(Option<TokenRefresherFn>);
impl Drop for TokenRefresherGuard {
    fn drop(&mut self) {
        *TOKEN_REFRESHER.write().unwrap() = self.0.take();
    }
}

pub fn set_token_refresher_for_test<F>(f: F) -> TokenRefresherGuard
where
    F: Fn(&Account) -> Result<String, crate::auth::AuthError> + Send + Sync + 'static,
{
    let mut w = TOKEN_REFRESHER.write().unwrap();
    let old = w.clone();
    *w = Some(Arc::new(move |acc, _force| f(acc)));
    TokenRefresherGuard(old)
}

pub fn set_token_refresher_with_force_for_test<F>(f: F) -> TokenRefresherGuard
where
    F: Fn(&Account, bool) -> Result<String, crate::auth::AuthError> + Send + Sync + 'static,
{
    let mut w = TOKEN_REFRESHER.write().unwrap();
    let old = w.clone();
    *w = Some(Arc::new(f));
    TokenRefresherGuard(old)
}

pub fn do_refresh_token(account: &Account, force: bool) -> Result<String, crate::auth::AuthError> {
    if let Some(f) = TOKEN_REFRESHER.read().unwrap().as_ref() {
        return f(account, force);
    }
    let mut acc_clone = account.clone();
    let rt = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| rt.block_on(async { crate::auth::refresh_token_force(&mut acc_clone, force).await }))
}

pub struct QuotaRefresherGuard(Option<QuotaRefresherFn>);
impl Drop for QuotaRefresherGuard {
    fn drop(&mut self) {
        *QUOTA_REFRESHER.write().unwrap() = self.0.take();
    }
}

pub fn set_quota_refresher_for_test<F>(f: F) -> QuotaRefresherGuard
where
    F: Fn(&Account) + Send + Sync + 'static,
{
    let mut w = QUOTA_REFRESHER.write().unwrap();
    let old = w.clone();
    *w = Some(Arc::new(f));
    QuotaRefresherGuard(old)
}

pub fn do_schedule_quota_refresh(account: &Account) {
    if let Some(f) = QUOTA_REFRESHER.read().unwrap().as_ref() {
        f(account);
        return;
    }
    crate::quota::schedule_quota_refresh_simple(account);
}

pub use crate::observability::{LogWriterGuard, set_log_writer_for_test};

pub fn log_message(prefix: &str, msg: &str) {
    let level = if prefix.contains("ERROR") || prefix.contains("EXCEPTION") || prefix.contains("ALL EXHAUSTED") {
        crate::observability::LogLevel::Error
    } else if prefix.contains("WARN") || prefix.contains("FAILOVER") || prefix.contains("STREAM TRUNCATED") {
        crate::observability::LogLevel::Warn
    } else if prefix.contains("DEBUG") {
        crate::observability::LogLevel::Debug
    } else {
        crate::observability::LogLevel::Info
    };
    crate::observability::log_message_level(level, prefix, msg);
}

pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        let secs = d.as_secs_f64();
        format!("{secs:.2}s")
    }
}

pub fn filepath_base(path: &str) -> String {
    let p = if let Some(idx) = path.find('?') {
        &path[..idx]
    } else {
        path
    };
    let trimmed = p.trim_end_matches('/');
    if let Some(idx) = trimmed.rfind('/') {
        let part = &trimmed[idx + 1..];
        if !part.is_empty() {
            return part.to_string();
        }
    } else if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    "/".to_string()
}

// ---------------------------------------------------------------------------
// HTTP Header Rewriting & Request Inspection
// ---------------------------------------------------------------------------

pub fn hop_by_hop_names(headers: &HeaderMap) -> HashSet<String> {
    let mut names = HashSet::new();
    for name in &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        names.insert(name.to_string());
    }
    for val in headers.get_all(hyper::header::CONNECTION) {
        if let Ok(s) = val.to_str() {
            for part in s.split(',') {
                let trimmed = part.trim().to_ascii_lowercase();
                if !trimmed.is_empty() {
                    names.insert(trimmed);
                }
            }
        }
    }
    names
}

pub fn filter_request_headers(
    headers: &HeaderMap,
    access_token: &str,
    client_ua: &str,
    is_generation: bool,
) -> HeaderMap {
    let mut excluded = hop_by_hop_names(headers);
    excluded.insert("host".to_string());
    excluded.insert("content-length".to_string());
    excluded.insert("authorization".to_string());
    excluded.insert("accept-encoding".to_string());
    excluded.insert("expect".to_string());
    if is_generation {
        excluded.insert("idempotency-key".to_string());
        excluded.insert("x-idempotency-key".to_string());
    }

    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        if !excluded.contains(k.as_str()) {
            out.append(k.clone(), v.clone());
        }
    }
    if let Ok(v) = HeaderValue::from_str(&get_backend_host()) {
        out.insert(hyper::header::HOST, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {access_token}")) {
        out.insert(hyper::header::AUTHORIZATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(client_ua) {
        out.insert(hyper::header::USER_AGENT, v);
    }
    out.insert(hyper::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    out
}

pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut excluded = hop_by_hop_names(headers);
    excluded.insert("content-length".to_string());
    excluded.insert("transfer-encoding".to_string());

    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        if !excluded.contains(k.as_str()) {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

pub fn attach_debug_headers(headers: &mut HeaderMap, account: Option<&Account>, strategy: &str, affinity_status: &str) {
    let Some(acc) = account else { return };
    let disp = display_account_name(Some(acc), Some(false));
    if let Ok(val) = HeaderValue::from_str(&disp) {
        headers.insert("X-Agy-Pool-Account", val);
    }
    if let Ok(val) = HeaderValue::from_str(&acc.id) {
        headers.insert("X-Agy-Pool-Account-Id", val);
    }
    if !strategy.is_empty()
        && let Ok(val) = HeaderValue::from_str(strategy)
    {
        headers.insert("X-Agy-Pool-Strategy", val);
    }
    if !affinity_status.is_empty()
        && let Ok(val) = HeaderValue::from_str(affinity_status)
    {
        headers.insert("X-Agy-Pool-Affinity", val);
    }
}

pub fn is_generation_request_path(path: &str) -> bool {
    let p = if let Some(idx) = path.find('?') {
        &path[..idx]
    } else {
        path
    };
    p.to_ascii_lowercase().contains("generatecontent")
}

pub fn is_generation_request<B>(r: &Request<B>) -> bool {
    let path = r.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or(r.uri().path());
    is_generation_request_path(path)
}

pub fn is_stream_request_path(path: &str, accept: Option<&str>) -> bool {
    if is_generation_request_path(path) {
        return true;
    }
    let p = if let Some(idx) = path.find('?') {
        &path[..idx]
    } else {
        path
    };
    p.to_ascii_lowercase().contains("stream")
        || accept
            .map(|a| a.to_ascii_lowercase().contains("text/event-stream"))
            .unwrap_or(false)
}

pub fn is_stream_request<B>(r: &Request<B>) -> bool {
    if is_generation_request(r) {
        return true;
    }
    let path = r.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or(r.uri().path());
    let accept = r.headers().get(hyper::header::ACCEPT).and_then(|v| v.to_str().ok());
    is_stream_request_path(path, accept)
}

pub const MAX_REQUEST_BODY_BYTES: usize = 64 << 20;

pub async fn read_request_body<B>(headers: &HeaderMap, body: B) -> Result<Bytes, String>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let content_lengths: Vec<&str> = headers
        .get_all(hyper::header::CONTENT_LENGTH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    let te = headers
        .get(hyper::header::TRANSFER_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !content_lengths.is_empty() && !te.is_empty() {
        return Err("both Content-Length and Transfer-Encoding are present".to_string());
    }

    if content_lengths.len() > 1 {
        let first = content_lengths[0].trim();
        for cl in &content_lengths[1..] {
            if cl.trim() != first {
                return Err("conflicting Content-Length headers".to_string());
            }
        }
    }

    if !content_lengths.is_empty() {
        let cl_str = content_lengths[0].trim();
        let cl_val = cl_str
            .parse::<i64>()
            .map_err(|_| "invalid Content-Length".to_string())?;
        if cl_val < 0 {
            return Err("negative Content-Length".to_string());
        }
        if cl_val as usize > MAX_REQUEST_BODY_BYTES {
            return Err("request body exceeds maximum allowed size".to_string());
        }

        let collected = http_body_util::BodyExt::collect(body)
            .await
            .map_err(|e| format!("read error: {e}"))?
            .to_bytes();
        if (collected.len() as i64) < cl_val {
            return Err("unexpected EOF in request body".to_string());
        }
        return Ok(collected.slice(..cl_val as usize));
    }

    if !te.is_empty() {
        let codings: Vec<&str> = te.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        if codings.len() != 1 || !codings[0].eq_ignore_ascii_case("chunked") {
            return Err("unsupported Transfer-Encoding".to_string());
        }
    }

    let collected = http_body_util::BodyExt::collect(body)
        .await
        .map_err(|e| format!("read error: {e}"))?
        .to_bytes();
    if collected.len() > MAX_REQUEST_BODY_BYTES {
        return Err("request body exceeds maximum allowed size".to_string());
    }
    Ok(collected)
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

static SELECTION_MUTEX: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

#[derive(Clone, Default)]
pub struct Handler;

impl Handler {
    pub fn new() -> Self {
        Self
    }

    pub async fn serve(&self, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
        if crate::dashboard::owns(req.method(), req.uri().path()) {
            return crate::dashboard::serve(req).await;
        }
        self.handle_proxy(req).await
    }

    pub async fn handle_proxy(&self, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
        if req.method() == Method::GET {
            let path = req.uri().path();
            if path == "/metrics" || path == "/metrics/" {
                let mut resp = Response::new(full_body(Bytes::from(format_prometheus_metrics())));
                resp.headers_mut().insert(
                    hyper::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
                );
                return resp;
            }
            if path == "/healthz" || path == "/healthz/" {
                let mut resp = Response::new(full_body(Bytes::from_static(b"{\"status\":\"ok\"}")));
                resp.headers_mut().insert(
                    hyper::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                return resp;
            }
        }

        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or(req.uri().path())
            .to_string();
        let path = if path_and_query.is_empty() {
            "/".to_string()
        } else {
            path_and_query
        };

        let (parts, incoming_body) = req.into_parts();
        let body_bytes = match read_request_body(&parts.headers, incoming_body).await {
            Ok(b) => b,
            Err(err) => {
                let h = HeaderMap::new();
                return send_buffered(StatusCode::BAD_REQUEST, &h, Bytes::from(format!("{err}\n")));
            }
        };

        let static_cfg = crate::config::get_static_config();
        let (path, mut body_bytes, remapped) = apply_model_mapping(&path, &body_bytes, &static_cfg.model_mapping);
        if let Some((ref src, ref target)) = remapped {
            log_message("MODEL MAPPING", &format!("Remapped request model: {src} -> {target}"));
        }

        if let Some(cloaked) = cloak_payload_if_needed(&body_bytes) {
            log_message(
                "PROMPT CLOAK",
                "Applied zero-width cloaking to sensitive agent system prompt",
            );
            body_bytes = cloaked;
        }

        let is_generation = is_generation_request_path(&path);
        let accept = parts.headers.get(hyper::header::ACCEPT).and_then(|v| v.to_str().ok());
        let is_sse = is_stream_request_path(&path, accept);
        // Narrower than is_sse: heartbeats would corrupt JSON and JSON-array (no alt=sse) streams.
        let wants_sse =
            path.contains("alt=sse") || accept.is_some_and(|a| a.to_ascii_lowercase().contains("text/event-stream"));
        let keep_alive_secs = static_cfg.server.keep_alive_secs;
        let keep_alive_duration = Duration::from_secs(keep_alive_secs);

        if wants_sse && keep_alive_secs > 0 {
            struct AbortOnDrop(Option<tokio::task::AbortHandle>);
            impl Drop for AbortOnDrop {
                fn drop(&mut self) {
                    if let Some(ref handle) = self.0 {
                        handle.abort();
                    }
                }
            }

            let (tx, mut rx) = tokio::sync::oneshot::channel();
            let join_handle = tokio::spawn(async move {
                let res = Self::execute_request(parts, path, body_bytes, is_generation, is_sse).await;
                let _ = tx.send(res);
            });
            let mut abort_guard = AbortOnDrop(Some(join_handle.abort_handle()));

            match tokio::time::timeout(keep_alive_duration, &mut rx).await {
                Ok(Ok(resp)) => {
                    abort_guard.0 = None;
                    resp
                }
                Ok(Err(_)) => {
                    abort_guard.0 = None;
                    send_buffered(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &HeaderMap::new(),
                        Bytes::from_static(b"Request task aborted\n"),
                    )
                }
                Err(_) => {
                    log_message(
                        "KEEP-ALIVE",
                        &format!(
                            "SSE stream in progress after {keep_alive_duration:?}; committing 200 OK headers and sending heartbeats"
                        ),
                    );
                    let handle = abort_guard.0.take();
                    let body = KeepAliveBody::new(rx, keep_alive_duration, handle);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(hyper::header::CONTENT_TYPE, "text/event-stream")
                        .header(hyper::header::CACHE_CONTROL, "no-cache")
                        .body(http_body_util::BodyExt::boxed(body))
                        .unwrap()
                }
            }
        } else {
            Self::execute_request(parts, path, body_bytes, is_generation, is_sse).await
        }
    }

    pub async fn execute_request(
        parts: hyper::http::request::Parts,
        path: String,
        body_bytes: Bytes,
        is_generation: bool,
        is_sse: bool,
    ) -> Response<ProxyBody> {
        if let Some(rotate) = LOG_ROTATOR.get() {
            rotate();
        }

        let pool = match crate::storage::load_pool() {
            Ok(p) if !p.accounts.is_empty() => p,
            _ => {
                let h = HeaderMap::new();
                return send_buffered(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &h,
                    Bytes::from_static(b"No accounts configured in agy-pool\n"),
                );
            }
        };

        if is_generation {
            record_generation_request();
            crate::dashboard::telemetry::record_client_request();
            for a in &pool.accounts {
                if crate::scheduler::is_restricted(a) {
                    record_restricted_skip();
                }
            }
        }

        let now_sys = crate::clock::now();
        let now = crate::clock::now_unix();
        let strategy = if pool.strategy.is_empty() {
            crate::config::DEFAULT_STRATEGY
        } else {
            &pool.strategy
        };

        let mut candidates = Vec::new();
        let mut pending_claim: Option<ConfirmationClaim> = None;
        let mut session_key = String::new();
        let mut affinity_status = "none".to_string();
        let mut reserved_id = String::new();
        let mut reserved_in_flight = None;
        let mut model = String::new();
        let mut model_excluded = 0;
        let mut lease_guard = None;

        if is_generation {
            let (m, sk) = inspect_request(&parts.headers, Some(&parts.uri), &body_bytes);
            model = m;
            session_key = sk;
            if model.is_empty()
                && let Some(idx) = path.find("/models/")
            {
                let after = &path[idx + 8..];
                let end = after.find(':').unwrap_or(after.len());
                let clean = after[..end].strip_prefix("models/").unwrap_or(&after[..end]);
                model = clean.to_string();
            }
            if !session_key.is_empty() && !model.is_empty() {
                session_key.push('\0');
                session_key.push_str(&model);
            }

            let _sel_guard = SELECTION_MUTEX.lock().await;
            let mut pick = |remote: &std::collections::HashMap<String, i64>| {
                crate::observability::set_remote_in_flight(remote.clone());
                let ordered_indices = crate::scheduler::order_candidates(&pool.accounts, strategy, &pool, now);
                candidates = ordered_indices
                    .into_iter()
                    .filter_map(|i| pool.accounts.get(i).cloned())
                    .collect();

                let mut held_by_load = false;
                if !session_key.is_empty() {
                    let (aff_cands, matched) =
                        affinity_manager().apply_affinity_accounts(&candidates, &session_key, now_sys);
                    candidates = aff_cands;
                    if matched {
                        affinity_status = "hit".to_string();
                    } else {
                        affinity_status = "new".to_string();
                        held_by_load = affinity_manager().held_by_load_accounts(&candidates, &session_key, now_sys);
                        if strategy == crate::config::STRATEGY_SMART {
                            candidates = crate::scheduler::rotate_top_tier(
                                &candidates,
                                crate::scheduler::get_smart_rr_cursor(),
                                now,
                            );
                            crate::scheduler::advance_smart_rr_cursor();
                        }
                    }
                }

                let (cands_after_cool, excluded) = exclude_model_cooldowns_accounts(&candidates, &model, now_sys);
                candidates = cands_after_cool;
                model_excluded = excluded;

                if strategy == crate::config::STRATEGY_SMART {
                    if affinity_status != "hit" {
                        candidates = smooth_model_rate_accounts(&candidates, &model, now_sys);
                    } else if candidates.len() > 2 {
                        let first = candidates[0].clone();
                        let mut rest = smooth_model_rate_accounts(&candidates[1..], &model, now_sys);
                        rest.insert(0, first);
                        candidates = rest;
                    }
                }

                if let Some(bound_id) = affinity_manager().get(&session_key, now_sys)
                    && held_by_load
                    && candidates.iter().any(|a| a.id == bound_id)
                {
                    affinity_status = "spill".to_string();
                }

                if let Some(conf) = select_and_claim_confirmation_candidate(&pool.accounts) {
                    candidates.insert(0, conf.account.clone());
                    pending_claim = Some(conf);
                }

                let pending_id = pending_claim.as_ref().map(|c| c.account_id.clone());
                for acc in &candidates {
                    let is_pending = pending_id.as_deref() == Some(&acc.id);
                    if crate::scheduler::is_restricted(acc) && !is_pending {
                        continue;
                    }
                    if !acc.id.is_empty() {
                        reserved_id = acc.id.clone();
                        reserved_in_flight = Some(crate::observability::start_in_flight_generation(&acc.id));
                    }
                    break;
                }

                reserved_id.clone()
            };
            let mut picked = false;
            let reserve_res = crate::storage::reserve_in_flight(|remote| {
                picked = true;
                pick(remote)
            });
            // reserve_in_flight gives up without calling pick when the write lock stays busy; select on the last
            // known remote counts instead of leaving candidates empty (a spurious 503 with a healthy pool).
            if reserve_res.is_err() && !picked {
                pick(&crate::observability::remote_in_flight());
            }

            match reserve_res {
                Ok((_, lg)) => {
                    lease_guard = Some(lg);
                }
                Err(err) => {
                    if !reserved_id.is_empty() {
                        crate::observability::record_lease_fallback();
                        log_message(
                            "PROXY WARN",
                            &format!(
                                "in-flight lease not recorded for {reserved_id}, balancing on local counts: {err}"
                            ),
                        );
                    }
                }
            }

            if !candidates.is_empty() && crate::quota::refresh_needed(&candidates[0], now) {
                do_schedule_quota_refresh(&candidates[0]);
            }
        } else {
            // Non-generation: prefer active account, deprioritize restricted/cooldown
            let active_id = pool.active_account_id.clone().unwrap_or_default();
            candidates = pool.accounts.clone();
            candidates.sort_by_key(|a| {
                if crate::scheduler::is_restricted(a) {
                    3
                } else if a.rate_limited_until.map(|u| u > now).unwrap_or(false) {
                    2
                } else if a.id == active_id {
                    0
                } else {
                    1
                }
            });
        }

        let raw_ua = parts
            .headers
            .get(hyper::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let client_ua = if raw_ua.starts_with("antigravity/") {
            raw_ua.to_string()
        } else {
            get_user_agent()
        };

        let request_id = parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                let mut b = [0u8; 8];
                let _ = getrandom::fill(&mut b);
                format!("req-{}", hex::encode(b))
            });

        let audit_ctx = if is_generation && (has_active_auditor() || crate::observability::is_audit_worker_active()) {
            Some(build_selection_audit_context(&pool, strategy, now))
        } else {
            None
        };

        let mut attempt = 0;
        let mut has_failed_over = false;
        let mut consecutive_quota_failovers = 0;
        let mut last_error: Option<String> = None;

        for acc in &candidates {
            let is_pending = pending_claim.as_ref().map(|c| c.account_id == acc.id).unwrap_or(false);
            if is_generation && crate::scheduler::is_restricted(acc) && !is_pending {
                continue;
            }

            attempt += 1;
            let current_affinity = if attempt > 1 { "failover" } else { &affinity_status };
            if is_generation {
                crate::observability::record_routing_decision();
                if attempt > 1 {
                    crate::observability::record_failover_attempt();
                    has_failed_over = true;
                }
            }

            let (cur_ifg, cur_lg) = if acc.id == reserved_id {
                (reserved_in_flight.take(), lease_guard.take())
            } else if is_generation && !acc.id.is_empty() {
                let ifg = crate::observability::start_in_flight_generation(&acc.id);
                let lg = match crate::storage::lease_in_flight(&acc.id) {
                    Ok(g) => Some(g),
                    Err(e) => {
                        crate::observability::record_lease_fallback();
                        log_message(
                            "PROXY WARN",
                            &format!(
                                "in-flight lease not recorded on failover for {}, balancing on local counts: {e}",
                                acc.id
                            ),
                        );
                        None
                    }
                };
                (Some(ifg), lg)
            } else {
                (None, None)
            };

            if is_generation && !acc.id.is_empty() && !model.is_empty() {
                record_model_dispatch(&acc.id, &model, now_sys);
            }

            let frozen_event = audit_ctx.as_ref().map(|ctx| build_frozen_event(acc, &path, ctx));

            let dispatch_params = DispatchParams {
                req_method: &parts.method,
                req_uri: &parts.uri,
                req_headers: &parts.headers,
                path: &path,
                body: &body_bytes,
                model: &model,
                is_generation,
                is_sse,
                acc,
                client_ua: &client_ua,
                attempt,
                strategy,
                affinity_status: current_affinity,
                session_key: &session_key,
                has_failed_over,
                frozen_event,
                in_flight_guard: cur_ifg,
                lease_guard: cur_lg,
                request_id: &request_id,
            };

            let (outcome, tokens) = dispatch_account(dispatch_params).await;

            if is_pending {
                pending_claim = None;
            }

            match outcome {
                Attempt::Done(resp) => {
                    if is_generation && let Some(toks) = tokens {
                        crate::observability::record_generation_success();
                        if has_failed_over {
                            crate::observability::record_failover_success();
                        }
                        if !session_key.is_empty() && affinity_status != "spill" {
                            affinity_manager().set(&session_key, &acc.id, now_sys);
                        }
                        if toks.prompt > 0 {
                            let cached_str = if toks.cached >= 0 {
                                format!("{} ({}%)", toks.cached, toks.cached * 100 / toks.prompt)
                            } else {
                                "not reported".to_string()
                            };
                            let disp = display_account_name(Some(acc), Some(false));
                            let output_info = if toks.output > 0 || toks.thinking > 0 {
                                format!(", output {} (thinking {})", toks.output, toks.thinking)
                            } else {
                                String::new()
                            };
                            log_message(
                                "CACHE",
                                &format!(
                                    "{model} session {} -> {disp}: prompt {}, cached {cached_str}{output_info}",
                                    session_tag(&session_key),
                                    toks.prompt
                                ),
                            );
                        }
                    }
                    return resp;
                }
                Attempt::Failover { reason, err } => {
                    if let Some(e) = err {
                        last_error = Some(e);
                    }
                    if let Some(bound) = affinity_manager().get(&session_key, now_sys)
                        && bound == acc.id
                    {
                        affinity_manager().delete(&session_key);
                    }
                    if reason.contains("QuotaExhausted") {
                        consecutive_quota_failovers += 1;
                        if consecutive_quota_failovers >= 2 {
                            log_message(
                                "CIRCUIT BREAKER",
                                &format!(
                                    "Consecutive 429 quota failovers ({consecutive_quota_failovers}) reached for request {request_id}. Halting failover to protect remaining pool accounts."
                                ),
                            );
                            break;
                        }
                    } else {
                        consecutive_quota_failovers = 0;
                    }
                    continue;
                }
            }
        }

        // All accounts exhausted or circuit breaker tripped
        let (status, err_msg) = if consecutive_quota_failovers >= 2 {
            (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Circuit breaker tripped: consecutive 429 quota exhaustion detected across {consecutive_quota_failovers} accounts for request {request_id}. Halting failover to protect remaining pool accounts."
                ),
            )
        } else if let Some(err) = last_error {
            (StatusCode::SERVICE_UNAVAILABLE, err)
        } else if model_excluded > 0 {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{model_excluded} account(s) paused for {model} after 503/capacity errors"),
            )
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "all candidate accounts restricted or exhausted".to_string(),
            )
        };
        log_message(
            "ALL EXHAUSTED",
            &format!(
                "{} {} failed across candidate accounts: {err_msg}",
                parts.method,
                filepath_base(&path)
            ),
        );

        let mut out_headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(strategy) {
            out_headers.insert("X-Agy-Pool-Strategy", v);
        }
        if consecutive_quota_failovers >= 2 {
            out_headers.insert(
                "X-Agy-Pool-Circuit-Breaker",
                HeaderValue::from_static("consecutive-429"),
            );
        }
        send_buffered(
            status,
            &out_headers,
            Bytes::from(format!("All accounts in pool exhausted or unavailable: {err_msg}\n")),
        )
    }
}

#[cfg(test)]
pub mod tests;
