use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::config;
use crate::observability;
use crate::storage::{Account, Pool, QuotaState, QuotaWindow, StorageError};

#[derive(Debug)]
pub enum QuotaError {
    AccountRequired,
    AccountDisabled,
    TestGuard(String),
    TokenRefresh(String),
    Http(crate::http::HttpError),
    Persistence(String),
    Upstream(String),
    NoGeminiBuckets,
    NoQuotaInfo,
    Json(serde_json::Error),
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::AccountRequired => write!(f, "account is required"),
            QuotaError::AccountDisabled => write!(f, "cannot refresh quota for disabled account"),
            QuotaError::TestGuard(msg) => write!(f, "{}", msg),
            QuotaError::TokenRefresh(msg) => write!(f, "{}", msg),
            QuotaError::Http(e) => write!(f, "http error: {}", e),
            QuotaError::Persistence(msg) => write!(f, "{}", msg),
            QuotaError::Upstream(msg) => write!(f, "{}", msg),
            QuotaError::NoGeminiBuckets => write!(f, "quota response contained no Gemini quota buckets"),
            QuotaError::NoQuotaInfo => write!(f, "model response contained no quota information"),
            QuotaError::Json(e) => write!(f, "json decode error: {}", e),
        }
    }
}

impl std::error::Error for QuotaError {}

pub type BackendUrlProviderFunc = Box<dyn Fn() -> String + Send + Sync>;
static BACKEND_URL_BASE_PROVIDER: RwLock<Option<Arc<BackendUrlProviderFunc>>> = RwLock::new(None);

pub struct BackendUrlBaseGuard(Option<Arc<BackendUrlProviderFunc>>);
impl Drop for BackendUrlBaseGuard {
    fn drop(&mut self) {
        *BACKEND_URL_BASE_PROVIDER.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_backend_url_base_provider_for_test(f: Option<BackendUrlProviderFunc>) -> BackendUrlBaseGuard {
    let mut w = BACKEND_URL_BASE_PROVIDER.write().unwrap_or_else(|e| e.into_inner());
    let old = w.take();
    *w = f.map(Arc::new);
    BackendUrlBaseGuard(old)
}

pub fn get_backend_url_base() -> String {
    let hook = BACKEND_URL_BASE_PROVIDER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(f) = hook {
        f().trim_end_matches('/').to_string()
    } else {
        format!("https://{}", super::BACKEND_HOST)
    }
}

pub type UserAgentProviderFunc = Box<dyn Fn() -> String + Send + Sync>;
static USER_AGENT_PROVIDER: RwLock<Option<Arc<UserAgentProviderFunc>>> = RwLock::new(None);

pub struct UserAgentGuard(Option<Arc<UserAgentProviderFunc>>);
impl Drop for UserAgentGuard {
    fn drop(&mut self) {
        *USER_AGENT_PROVIDER.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_user_agent_provider_for_test(f: Option<UserAgentProviderFunc>) -> UserAgentGuard {
    let mut w = USER_AGENT_PROVIDER.write().unwrap_or_else(|e| e.into_inner());
    let old = w.take();
    *w = f.map(Arc::new);
    UserAgentGuard(old)
}

/// Installs the process-wide User-Agent provider (Go's quota.UserAgentProvider); called each refresh.
pub fn set_user_agent_provider(f: UserAgentProviderFunc) {
    *USER_AGENT_PROVIDER.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

pub fn get_user_agent() -> String {
    let hook = USER_AGENT_PROVIDER.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(f) = hook {
        f()
    } else {
        super::DEFAULT_UA.to_string()
    }
}

pub type TokenRefresherFunc = Box<dyn Fn(&mut Account) -> Result<String, crate::auth::AuthError> + Send + Sync>;
static TOKEN_REFRESHER: RwLock<Option<Arc<TokenRefresherFunc>>> = RwLock::new(None);

pub struct TokenRefresherGuard(Option<Arc<TokenRefresherFunc>>);
impl Drop for TokenRefresherGuard {
    fn drop(&mut self) {
        *TOKEN_REFRESHER.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_token_refresher_for_test(f: Option<TokenRefresherFunc>) -> TokenRefresherGuard {
    let mut w = TOKEN_REFRESHER.write().unwrap_or_else(|e| e.into_inner());
    let old = w.take();
    *w = f.map(Arc::new);
    TokenRefresherGuard(old)
}

async fn do_refresh_token(account: &mut Account, force: bool) -> Result<String, crate::auth::AuthError> {
    let hook = TOKEN_REFRESHER.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(f) = hook {
        f(account)
    } else {
        crate::auth::refresh_token_force(account, force).await
    }
}

pub type WarnHookFunc = Box<dyn Fn(&str) + Send + Sync>;
static WARN_HOOK: RwLock<Option<Arc<WarnHookFunc>>> = RwLock::new(None);

pub struct WarnHookGuard(Option<Arc<WarnHookFunc>>);
impl Drop for WarnHookGuard {
    fn drop(&mut self) {
        *WARN_HOOK.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_warn_hook_for_test(f: Option<WarnHookFunc>) -> WarnHookGuard {
    let mut w = WARN_HOOK.write().unwrap_or_else(|e| e.into_inner());
    let old = w.take();
    *w = f.map(Arc::new);
    WarnHookGuard(old)
}

fn call_warn_hook(msg: &str) {
    let hook = WARN_HOOK.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(f) = hook {
        f(msg);
    }
}

pub type PoolTxFunc = Box<
    dyn Fn(Box<dyn FnOnce(&mut Pool) -> Result<(), StorageError> + Send>) -> Result<(), StorageError> + Send + Sync,
>;
static POOL_TRANSACTION_HOOK: RwLock<Option<Arc<PoolTxFunc>>> = RwLock::new(None);

pub struct PoolTxGuard(Option<Arc<PoolTxFunc>>);
impl Drop for PoolTxGuard {
    fn drop(&mut self) {
        *POOL_TRANSACTION_HOOK.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

pub fn set_pool_transaction_for_test(f: Option<PoolTxFunc>) -> PoolTxGuard {
    let mut w = POOL_TRANSACTION_HOOK.write().unwrap_or_else(|e| e.into_inner());
    let old = w.take();
    *w = f.map(Arc::new);
    PoolTxGuard(old)
}

fn is_pool_transaction_replaced() -> bool {
    POOL_TRANSACTION_HOOK
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

fn execute_pool_transaction<F>(f: F) -> Result<(), StorageError>
where
    F: FnOnce(&mut Pool) -> Result<(), StorageError> + Send + 'static,
{
    let hook = POOL_TRANSACTION_HOOK.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(h) = hook {
        h(Box::new(f))
    } else {
        crate::storage::pool_transaction(f)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RetryEntry {
    pub attempt: usize,
    pub next_at: f64,
}

struct RefreshState {
    in_flight: HashMap<String, bool>,
    retry: HashMap<String, RetryEntry>,
}

static REFRESH_STATE: std::sync::LazyLock<Mutex<RefreshState>> = std::sync::LazyLock::new(|| {
    Mutex::new(RefreshState {
        in_flight: HashMap::new(),
        retry: HashMap::new(),
    })
});

/// ResetRefreshState resets in-flight and retry backoff state (useful for tests).
pub fn reset_refresh_state() {
    let mut state = REFRESH_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.in_flight.clear();
    state.retry.clear();
}

/// GetRefreshRetryEntry returns the retry attempt, next_at timestamp, and existence for an account key.
pub fn get_refresh_retry_entry(key: &str) -> (usize, f64, bool) {
    let state = REFRESH_STATE.lock().unwrap_or_else(|e| e.into_inner());
    match state.retry.get(key) {
        Some(e) => (e.attempt, e.next_at, true),
        None => (0, 0.0, false),
    }
}

/// IsRefreshInFlight reports whether a refresh is currently running for an account key.
pub fn is_refresh_in_flight(key: &str) -> bool {
    let state = REFRESH_STATE.lock().unwrap_or_else(|e| e.into_inner());
    state.in_flight.get(key).copied().unwrap_or(false)
}

struct InFlightQuery {
    sender: tokio::sync::watch::Sender<Option<Result<QuotaState, String>>>,
    receiver: tokio::sync::watch::Receiver<Option<Result<QuotaState, String>>>,
}

static FLIGHT_MAP: std::sync::LazyLock<Mutex<HashMap<String, InFlightQuery>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

async fn do_http_post(
    url: &str,
    token: &str,
    ua: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<(u16, Vec<u8>), crate::http::HttpError> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::AUTHORIZATION, format!("Bearer {}", token))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::USER_AGENT, ua)
        .header(hyper::header::ACCEPT, "application/json")
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| crate::http::HttpError::Build(e.to_string()))?;

    let fut = async {
        let resp = crate::http::client()
            .request(req)
            .await
            .map_err(|e| crate::http::HttpError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body_bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| crate::http::HttpError::Body(e.to_string()))?
            .to_bytes()
            .to_vec();
        Ok::<_, crate::http::HttpError>((status, body_bytes))
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(crate::http::HttpError::Timeout),
    }
}

#[derive(serde::Deserialize)]
struct Bucket {
    #[serde(default, rename = "bucketId")]
    bucket_id: String,
    #[serde(default, rename = "remainingFraction")]
    remaining_fraction: Option<serde_json::Value>,
    #[serde(default, rename = "resetTime")]
    reset_time: Option<serde_json::Value>,
    #[serde(default)]
    window: String,
}

#[derive(serde::Deserialize)]
struct Group {
    #[serde(default)]
    buckets: Vec<Bucket>,
}

#[derive(serde::Deserialize)]
struct QuotaSummaryResponse {
    #[serde(default)]
    groups: Vec<Group>,
}

fn remaining_fraction(fraction: Option<&serde_json::Value>, reset: Option<&serde_json::Value>) -> Option<f64> {
    if fraction.is_none() && super::parse_iso_or_timestamp(reset).is_some() {
        return Some(0.0);
    }
    super::parse_quota_fraction(fraction)
}

fn recompute_compat_quota(q: &mut QuotaState) {
    let mut known: Vec<(f64, Option<serde_json::Value>)> = Vec::new();
    for w in [&q.gemini_5h, &q.gemini_weekly] {
        if let Some(w) = w
            && let Some(f) = w.fraction
        {
            known.push((f, w.reset_time.clone()));
        }
    }

    if !known.is_empty() {
        let mut min_item = known[0].clone();
        for item in &known[1..] {
            if item.0 < min_item.0 {
                min_item = item.clone();
            }
        }
        q.remaining_fraction = Some(min_item.0);
        q.reset_time = min_item.1;
    } else {
        q.remaining_fraction = None;
        q.reset_time = None;
    }
}

fn apply_validation_required(account: &mut Account, v_url: &str) {
    account.status = crate::storage::STATUS_VALIDATION_REQUIRED.to_string();
    if !v_url.is_empty() {
        account.validation_url = Some(v_url.to_string());
    }
    let now = crate::clock::now_unix();
    let cooldown = now + 86400.0;
    account.rate_limited_until = Some(cooldown);

    let id = account.id.clone();
    let status = account.status.clone();
    let val_url = account.validation_url.clone();

    let _ = execute_pool_transaction(move |pool| {
        if let Some(stored) = pool.accounts.iter_mut().find(|a| a.id == id) {
            stored.status = status;
            stored.validation_url = val_url;
            stored.rate_limited_until = Some(cooldown);
        }
        Ok(())
    });
}

async fn probe_and_apply_validation_challenge(token: &str, ua: &str, account: &mut Account) {
    if account.validation_url.as_deref().unwrap_or("").is_empty()
        && account.status != crate::storage::STATUS_VALIDATION_REQUIRED
    {
        return;
    }

    let load_url = format!("{}/v1internal:loadCodeAssist", get_backend_url_base());
    let (status, body) = match do_http_post(&load_url, token, ua, b"{}", Duration::from_secs(10)).await {
        Ok(res) => res,
        Err(_) => {
            if account.validation_url.as_deref().is_some_and(|u| !u.is_empty()) {
                account.status = crate::storage::STATUS_VALIDATION_REQUIRED.to_string();
            }
            return;
        }
    };

    if crate::auth::is_validation_error(status, &body) {
        let mut v_url = crate::auth::extract_validation_url(&body);
        if v_url.is_empty()
            && let Some(orig) = &account.validation_url
        {
            v_url = orig.clone();
        }
        apply_validation_required(account, &v_url);
        return;
    }

    if (200..300).contains(&status) {
        #[derive(serde::Deserialize)]
        struct Tier {
            #[serde(default, rename = "reasonCode")]
            reason_code: String,
            #[serde(default, rename = "validationUrl")]
            validation_url: String,
        }
        #[derive(serde::Deserialize)]
        struct LoadResponse {
            #[serde(default, rename = "ineligibleTiers")]
            ineligible_tiers: Vec<Tier>,
        }

        if let Ok(res) = serde_json::from_slice::<LoadResponse>(&body) {
            for tier in &res.ineligible_tiers {
                if tier.reason_code.eq_ignore_ascii_case("VALIDATION_REQUIRED") {
                    let mut v_url = tier.validation_url.clone();
                    if v_url.is_empty()
                        && let Some(orig) = &account.validation_url
                    {
                        v_url = orig.clone();
                    }
                    apply_validation_required(account, &v_url);
                    return;
                }
            }
        }

        account.status.clear();
        account.validation_url = None;
        account.rate_limited_until = None;
        return;
    }

    if account.validation_url.as_deref().is_some_and(|u| !u.is_empty()) {
        account.status = crate::storage::STATUS_VALIDATION_REQUIRED.to_string();
    }
}

fn finalize_and_persist_quota(account: &mut Account, mut quota_data: QuotaState) -> Result<QuotaState, QuotaError> {
    if let Some(gw) = &quota_data.gemini_weekly
        && gw.reset_time.is_none()
        && gw.fraction == Some(0.0)
    {
        quota_data.gemini_weekly = None;
    }
    recompute_compat_quota(&mut quota_data);
    let now_sec = crate::clock::now_unix() as i64;
    quota_data.updated_at = Some(now_sec);

    let id = account.id.clone();
    let q_clone = quota_data.clone();

    execute_pool_transaction(move |pool| {
        if let Some(stored) = pool.accounts.iter_mut().find(|a| a.id == id) {
            stored.last_quota = Some(q_clone);
        }
        Ok(())
    })
    .map_err(|e| QuotaError::Persistence(format!("failed to persist quota update: {}", e)))?;

    account.last_quota = Some(quota_data.clone());
    Ok(quota_data)
}

async fn fetch_available_models_quota(
    token: &str,
    ua: &str,
    account: &mut Account,
    quota_data: &mut QuotaState,
) -> Result<(), QuotaError> {
    let models_url = format!("{}/v1internal:fetchAvailableModels", get_backend_url_base());
    let (status, body) = do_http_post(&models_url, token, ua, b"{}", Duration::from_secs(10))
        .await
        .map_err(QuotaError::Http)?;

    if status >= 400 {
        if crate::auth::is_validation_error(status, &body) || crate::auth::is_auth_error(status, &body) {
            let is_val = crate::auth::is_validation_error(status, &body);
            let status_str = if is_val {
                crate::storage::STATUS_VALIDATION_REQUIRED
            } else {
                crate::storage::STATUS_AUTH_ERROR
            };
            let delay = if is_val { 86400.0 } else { 3600.0 };
            let v_url = crate::auth::extract_validation_url(&body);
            let now = crate::clock::now_unix();
            let cooldown = now + delay;

            quota_data.gemini_5h = Some(QuotaWindow {
                fraction: Some(0.0),
                reset_time: None,
                ..Default::default()
            });
            quota_data.gemini_weekly = Some(QuotaWindow {
                fraction: Some(0.0),
                reset_time: None,
                ..Default::default()
            });
            recompute_compat_quota(quota_data);
            let now_sec = crate::clock::now_unix() as i64;
            quota_data.updated_at = Some(now_sec);

            let id = account.id.clone();
            let s_str = status_str.to_string();
            let v_opt = if !v_url.is_empty() { Some(v_url.clone()) } else { None };
            let q_clone = quota_data.clone();

            let _ = execute_pool_transaction(move |pool| {
                if let Some(stored) = pool.accounts.iter_mut().find(|a| a.id == id) {
                    stored.status = s_str;
                    if let Some(vu) = v_opt {
                        stored.validation_url = Some(vu);
                    }
                    stored.rate_limited_until = Some(cooldown);
                    stored.error_count += 1;
                    stored.last_quota = Some(q_clone);
                }
                Ok(())
            });

            account.status = status_str.to_string();
            if !v_url.is_empty() {
                account.validation_url = Some(v_url);
            }
            account.rate_limited_until = Some(cooldown);
            account.last_quota = Some(quota_data.clone());
            return Ok(());
        }
        return Err(QuotaError::Upstream(format!(
            "fetchAvailableModels HTTP {}: {}",
            status,
            String::from_utf8_lossy(&body)
        )));
    }

    #[derive(serde::Deserialize)]
    struct QuotaInfo {
        #[serde(default, rename = "remainingFraction")]
        remaining_fraction: Option<serde_json::Value>,
        #[serde(default, rename = "resetTime")]
        reset_time: Option<serde_json::Value>,
    }

    #[derive(serde::Deserialize)]
    struct ModelEntry {
        #[serde(default, rename = "quotaInfo")]
        quota_info: Option<QuotaInfo>,
    }

    #[derive(serde::Deserialize)]
    struct AvailableModelsResponse {
        #[serde(default, rename = "defaultAgentModelId")]
        default_agent_model_id: String,
        #[serde(default)]
        models: HashMap<String, ModelEntry>,
    }

    let res: AvailableModelsResponse = serde_json::from_slice(&body).map_err(QuotaError::Json)?;

    let mut candidates = Vec::new();
    if !res.default_agent_model_id.is_empty() {
        candidates.push(res.default_agent_model_id);
    }
    candidates.push("gemini-3.8-flash-high".to_string());
    candidates.push("gemini-3.6-flash-high".to_string());
    candidates.push("gemini-3.5-flash-medium".to_string());

    let mut rest = Vec::new();
    for name in res.models.keys() {
        if !candidates.contains(name) {
            rest.push(name.clone());
        }
    }
    rest.sort();
    candidates.extend(rest);

    for name in candidates {
        if !name.starts_with("gemini") {
            continue;
        }
        if let Some(model) = res.models.get(&name)
            && let Some(qi) = &model.quota_info
            && let Some(f) = remaining_fraction(qi.remaining_fraction.as_ref(), qi.reset_time.as_ref())
        {
            quota_data.gemini_5h = Some(QuotaWindow {
                fraction: Some(f),
                reset_time: qi.reset_time.clone(),
                ..Default::default()
            });
            quota_data.gemini_weekly = None;
            if let Some(ts) = super::parse_iso_or_timestamp(&qi.reset_time)
                && f == 0.0
                && (ts - crate::clock::now_unix()) > config::WINDOW_5H_SECS
            {
                quota_data.gemini_weekly = Some(QuotaWindow {
                    fraction: Some(f),
                    reset_time: qi.reset_time.clone(),
                    ..Default::default()
                });
            }
            return Ok(());
        }
    }

    Err(QuotaError::NoQuotaInfo)
}

/// QueryQuota refreshes cached quota for the given account against the Google quota endpoint.
pub async fn query_quota(account: &mut Account) -> Result<QuotaState, QuotaError> {
    if account.status == crate::storage::STATUS_DISABLED {
        return Err(QuotaError::AccountDisabled);
    }
    let key = if !account.id.is_empty() {
        account.id.clone()
    } else if !account.email.is_empty() {
        account.email.clone()
    } else {
        return Err(QuotaError::AccountRequired);
    };

    let (is_leader, mut rx) = {
        let mut map = FLIGHT_MAP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flight) = map.get(&key) {
            (false, flight.receiver.clone())
        } else {
            let (tx, rx) = tokio::sync::watch::channel(None);
            map.insert(
                key.clone(),
                InFlightQuery {
                    sender: tx,
                    receiver: rx.clone(),
                },
            );
            (true, rx)
        }
    };

    if !is_leader {
        loop {
            if let Some(res) = rx.borrow().clone() {
                match res {
                    Ok(q) => {
                        account.last_quota = Some(q.clone());
                        return Ok(q);
                    }
                    Err(e) => return Err(QuotaError::Upstream(e)),
                }
            }
            if rx.changed().await.is_err() {
                return Err(QuotaError::Upstream("flight sender dropped".to_string()));
            }
        }
    }

    let res = query_quota_inner(account).await;

    let flight = {
        let mut map = FLIGHT_MAP.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&key)
    };

    if let Some(flight) = flight {
        let flight_res = match &res {
            Ok(q) => Ok(q.clone()),
            Err(e) => Err(e.to_string()),
        };
        let _ = flight.sender.send(Some(flight_res));
    }

    res
}

async fn query_quota_inner(account: &mut Account) -> Result<QuotaState, QuotaError> {
    observability::record_quota_refresh_attempt();

    let res = query_quota_exec(account).await;
    if res.is_err() {
        observability::record_quota_refresh_failure();
    } else {
        observability::record_quota_refresh_success();
    }
    res
}

async fn query_quota_exec(account: &mut Account) -> Result<QuotaState, QuotaError> {
    if BACKEND_URL_BASE_PROVIDER
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
        && config::is_test_mode()
    {
        return Err(QuotaError::TestGuard(
            "[FAIL-CLOSED TEST GUARD] refusing external quota query in test mode without mock backend".to_string(),
        ));
    }

    let force_refresh = account.status == crate::storage::STATUS_VALIDATION_REQUIRED
        || account.status == crate::storage::STATUS_AUTH_ERROR
        || account.validation_url.as_deref().is_some_and(|u| !u.is_empty());

    let at = match do_refresh_token(account, force_refresh).await {
        Ok(t) => t,
        Err(e) => {
            if force_refresh && !account.access_token.is_empty() {
                account.access_token.clone()
            } else {
                return Err(QuotaError::TokenRefresh(format!(
                    "token refresh failed for account {}: {}",
                    account.id, e
                )));
            }
        }
    };

    let mut quota_data = QuotaState::default();
    if let Some(lq) = &account.last_quota {
        if let Some(w) = &lq.gemini_5h {
            quota_data.gemini_5h = Some(QuotaWindow {
                fraction: w.fraction,
                reset_time: w.reset_time.clone(),
                ..Default::default()
            });
        }
        if let Some(w) = &lq.gemini_weekly {
            quota_data.gemini_weekly = Some(QuotaWindow {
                fraction: w.fraction,
                reset_time: w.reset_time.clone(),
                ..Default::default()
            });
        }
        if let Some(w) = &lq.third_party_5h {
            quota_data.third_party_5h = Some(QuotaWindow {
                fraction: w.fraction,
                reset_time: w.reset_time.clone(),
                ..Default::default()
            });
        }
        if let Some(w) = &lq.third_party_weekly {
            quota_data.third_party_weekly = Some(QuotaWindow {
                fraction: w.fraction,
                reset_time: w.reset_time.clone(),
                ..Default::default()
            });
        }
    }

    let ua = get_user_agent();
    let quota_url = format!("{}/v1internal:retrieveUserQuotaSummary", get_backend_url_base());

    let summary_res = do_http_post(&quota_url, &at, &ua, b"{}", Duration::from_secs(10)).await;
    let quota_err: Option<String>;

    match summary_res {
        Err(e) => {
            quota_err = Some(e.to_string());
        }
        Ok((status, body)) => {
            if status >= 400 {
                if crate::auth::is_validation_error(status, &body) || crate::auth::is_auth_error(status, &body) {
                    let is_val = crate::auth::is_validation_error(status, &body);
                    let status_str = if is_val {
                        crate::storage::STATUS_VALIDATION_REQUIRED
                    } else {
                        crate::storage::STATUS_AUTH_ERROR
                    };
                    let delay = if is_val { 86400.0 } else { 3600.0 };
                    let v_url = crate::auth::extract_validation_url(&body);
                    let now = crate::clock::now_unix();
                    let cooldown = now + delay;

                    quota_data.gemini_5h = Some(QuotaWindow {
                        fraction: Some(0.0),
                        reset_time: None,
                        ..Default::default()
                    });
                    quota_data.gemini_weekly = Some(QuotaWindow {
                        fraction: Some(0.0),
                        reset_time: None,
                        ..Default::default()
                    });
                    recompute_compat_quota(&mut quota_data);
                    let now_sec = crate::clock::now_unix() as i64;
                    quota_data.updated_at = Some(now_sec);

                    let id = account.id.clone();
                    let s_str = status_str.to_string();
                    let v_opt = if !v_url.is_empty() { Some(v_url.clone()) } else { None };
                    let q_clone = quota_data.clone();

                    if let Err(e) = execute_pool_transaction(move |pool| {
                        if let Some(stored) = pool.accounts.iter_mut().find(|a| a.id == id) {
                            stored.status = s_str;
                            if let Some(vu) = v_opt {
                                stored.validation_url = Some(vu);
                            }
                            stored.rate_limited_until = Some(cooldown);
                            stored.error_count += 1;
                            stored.last_quota = Some(q_clone);
                        }
                        Ok(())
                    }) {
                        if is_pool_transaction_replaced() {
                            observability::record_persistence_error();
                        }
                        return Err(QuotaError::Persistence(format!(
                            "failed to persist {} state for account {}: {}",
                            status_str, account.id, e
                        )));
                    }

                    account.status = status_str.to_string();
                    if !v_url.is_empty() {
                        account.validation_url = Some(v_url);
                    }
                    account.rate_limited_until = Some(cooldown);
                    account.last_quota = Some(quota_data.clone());
                    return Ok(quota_data);
                }
                quota_err = Some(format!(
                    "retrieveUserQuotaSummary returned HTTP {}: {}",
                    status,
                    String::from_utf8_lossy(&body)
                ));
            } else if let Ok(summary) = serde_json::from_slice::<QuotaSummaryResponse>(&body) {
                let mut found_quota = false;
                let mut exact_5h = false;
                let mut exact_weekly = false;

                for group in summary.groups {
                    for bucket in group.buckets {
                        let fraction =
                            remaining_fraction(bucket.remaining_fraction.as_ref(), bucket.reset_time.as_ref());
                        let Some(f) = fraction else {
                            continue;
                        };
                        let bid = bucket.bucket_id.to_lowercase();
                        let w = bucket.window.to_lowercase();

                        if bid.contains("gemini") {
                            if bid.contains("5h") || w == "5h" {
                                if exact_5h {
                                    continue;
                                }
                                exact_5h = bid == "gemini-5h";
                                quota_data.gemini_5h = Some(QuotaWindow {
                                    fraction: Some(f),
                                    reset_time: bucket.reset_time,
                                    ..Default::default()
                                });
                                found_quota = true;
                            } else if bid.contains("weekly") || w == "weekly" {
                                if exact_weekly {
                                    continue;
                                }
                                exact_weekly = bid == "gemini-weekly";
                                quota_data.gemini_weekly = Some(QuotaWindow {
                                    fraction: Some(f),
                                    reset_time: bucket.reset_time,
                                    ..Default::default()
                                });
                                found_quota = true;
                            }
                        } else if bid.contains("3p") || bid.contains("third_party") {
                            if bid.contains("5h") || w == "5h" {
                                quota_data.third_party_5h = Some(QuotaWindow {
                                    fraction: Some(f),
                                    reset_time: bucket.reset_time,
                                    ..Default::default()
                                });
                            } else if bid.contains("weekly") || w == "weekly" {
                                quota_data.third_party_weekly = Some(QuotaWindow {
                                    fraction: Some(f),
                                    reset_time: bucket.reset_time,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }

                if found_quota {
                    super::append_quota_history(&account.id, account.last_quota.as_ref(), Some(&quota_data));
                    probe_and_apply_validation_challenge(&at, &ua, account).await;
                    return finalize_and_persist_quota(account, quota_data);
                }

                quota_err = Some("quota response contained no Gemini quota buckets".to_string());
                call_warn_hook(&format!(
                    "account {}: quota summary returned no Gemini buckets for User-Agent \"{}\" (agy too old or missing?); falling back to fetchAvailableModels, Gemini weekly quota is not refreshed",
                    account.id, ua
                ));
            } else {
                quota_err = Some("quota response contained no Gemini quota buckets".to_string());
            }
        }
    }

    // Fallback to fetchAvailableModels
    let fallback_res = fetch_available_models_quota(&at, &ua, account, &mut quota_data).await;
    if let Err(fallback_err) = fallback_res {
        if let Some(qe) = quota_err {
            return Err(QuotaError::Upstream(format!(
                "quota refresh failed ({}) and fallback failed ({})",
                qe, fallback_err
            )));
        }
        return Err(fallback_err);
    }

    probe_and_apply_validation_challenge(&at, &ua, account).await;
    finalize_and_persist_quota(account, quota_data)
}

/// ScheduleQuotaRefresh starts one bounded, non-blocking refresh per account.
pub fn schedule_quota_refresh(account: &Account, now: Option<f64>) -> bool {
    if account.status == crate::storage::STATUS_DISABLED {
        return false;
    }
    let key = if !account.id.is_empty() {
        account.id.clone()
    } else if !account.email.is_empty() {
        account.email.clone()
    } else {
        return false;
    };

    let current_time = now.unwrap_or_else(crate::clock::now_unix);

    {
        let mut state = REFRESH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.in_flight.get(&key).copied().unwrap_or(false) {
            return false;
        }
        if let Some(retry) = state.retry.get(&key)
            && retry.next_at > current_time
        {
            return false;
        }
        state.in_flight.insert(key.clone(), true);
    }

    let mut acc_clone = account.clone();
    let key_clone = key.clone();

    tokio::spawn(async move {
        let res = query_quota(&mut acc_clone).await;

        let mut state = REFRESH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight.remove(&key_clone);

        if res.is_ok() {
            state.retry.remove(&key_clone);
        } else {
            let prev_attempt = state.retry.get(&key_clone).map(|r| r.attempt).unwrap_or(0);
            let delay = super::next_backoff_delay(prev_attempt);
            let next_at = crate::clock::now_unix() + delay.as_secs_f64();
            state.retry.insert(
                key_clone,
                RetryEntry {
                    attempt: prev_attempt + 1,
                    next_at,
                },
            );
        }
    });

    true
}

/// ScheduleQuotaRefreshSimple adapts ScheduleQuotaRefresh to taking &Account.
pub fn schedule_quota_refresh_simple(account: &Account) {
    let _ = schedule_quota_refresh(account, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestServer;
    use std::sync::atomic::{AtomicI32, Ordering};

    struct TestEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            reset_refresh_state();
            crate::config::reset_data_dir();
            crate::config::set_test_mode(false);
        }
    }

    fn setup_quota_test_dir() -> TestEnv {
        let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::config::set_test_mode(true);
        let dir = tempfile::tempdir().unwrap();
        crate::config::configure_data_dir(dir.path()).unwrap();
        reset_refresh_state();
        TestEnv { _lock: lock, _dir: dir }
    }

    #[tokio::test]
    async fn test_observability_case_g_quota_refresh_success() {
        let _env = setup_quota_test_dir();
        observability::reset();

        let srv = TestServer::start(|_req| {
            let body = br#"{
                "groups": [{
                    "buckets": [
                        {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.85, "resetTime": "2026-09-20T12:00:00Z"},
                        {"bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.65, "resetTime": "2026-09-25T12:00:00Z"}
                    ]
                }]
            }"#.to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let mut acc = Account {
            id: "acc_refresh_success".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await;
        assert!(res.is_ok(), "query_quota failed: {:?}", res);

        let snap = observability::get_snapshot();
        assert_eq!(snap.quota_refresh_attempts, 1);
        assert_eq!(snap.quota_refresh_success, 1);
        assert_eq!(snap.quota_refresh_failure, 0);
    }

    #[tokio::test]
    async fn test_observability_case_h_quota_refresh_failure() {
        let _env = setup_quota_test_dir();
        observability::reset();

        let srv = TestServer::start(|_req| {
            (
                500,
                vec![("content-type".to_string(), "application/json".to_string())],
                br#"{"error":"internal"}"#.to_vec(),
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let mut acc = Account {
            id: "acc_refresh_fail".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await;
        assert!(res.is_err(), "expected error from 500 upstream");

        let snap = observability::get_snapshot();
        assert_eq!(snap.quota_refresh_attempts, 1);
        assert_eq!(snap.quota_refresh_failure, 1);
        assert_eq!(snap.quota_refresh_success, 0);
    }

    #[tokio::test]
    async fn test_observability_case_i_single_flight_quota_refresh() {
        let _env = setup_quota_test_dir();
        observability::reset();

        let upstream_calls = Arc::new(AtomicI32::new(0));
        let upstream_calls_clone = upstream_calls.clone();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let rx_arc = Arc::new(Mutex::new(rx));

        let srv = TestServer::start(move |_req| {
            upstream_calls_clone.fetch_add(1, Ordering::SeqCst);
            let rx = rx_arc.lock().unwrap().clone();
            tokio::task::block_in_place(|| {
                let mut rx = rx;
                while !*rx.borrow_and_update() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            let body = br#"{
                "groups": [{
                    "buckets": [
                        {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.9, "resetTime": "2026-09-20T12:00:00Z"}
                    ]
                }]
            }"#.to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let acc = Account {
            id: "acc_single_flight".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        // 10 concurrent callers attempting to schedule refresh for the same account
        let scheduled_count = Arc::new(AtomicI32::new(0));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let acc_c = acc.clone();
            let sc_c = scheduled_count.clone();
            handles.push(tokio::spawn(async move {
                if schedule_quota_refresh(&acc_c, None) {
                    sc_c.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        for h in handles {
            let _ = h.await;
        }

        // Only 1 should have been scheduled due to single-flight
        assert_eq!(scheduled_count.load(Ordering::SeqCst), 1, "expected 1 caller scheduled");

        // Wait briefly to ensure background task is executing
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Release upstream server to finish query
        let _ = tx.send(true);

        // Wait for background refresh to complete
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while is_refresh_in_flight(&acc.id) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !is_refresh_in_flight(&acc.id),
            "refresh did not complete within deadline"
        );

        let snap = observability::get_snapshot();
        assert_eq!(
            snap.quota_refresh_attempts, 1,
            "quota_refresh_attempts_total = {}, want 1",
            snap.quota_refresh_attempts
        );
        assert_eq!(
            snap.quota_refresh_success, 1,
            "quota_refresh_success_total = {}, want 1",
            snap.quota_refresh_success
        );
        assert_eq!(snap.quota_refresh_failure, 0);
        assert_eq!(
            upstream_calls.load(Ordering::SeqCst),
            1,
            "expected exactly 1 upstream call"
        );
    }

    #[tokio::test]
    async fn test_query_quota_success_persists_fresh_telemetry() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|req| {
            if req.path != "/v1internal:retrieveUserQuotaSummary" {
                return (404, vec![], b"Not Found".to_vec());
            }
            if req.header("authorization") != Some("Bearer mock-access-token") {
                return (401, vec![], b"Unauthorized".to_vec());
            }
            let body = br#"{
                "groups": [{
                    "buckets": [
                        {
                            "bucketId": "gemini-5h-user",
                            "remainingFraction": 0.85,
                            "resetTime": "2026-09-17T15:00:00Z",
                            "window": "5h"
                        },
                        {
                            "bucketId": "gemini-weekly-user",
                            "remainingFraction": 0.95,
                            "resetTime": "2026-09-24T00:00:00Z",
                            "window": "weekly"
                        }
                    ]
                }]
            }"#
            .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-access-token".to_string()))));

        let old_time = crate::clock::now_unix() as i64 - 500;
        let mut acc = Account {
            id: "acc-1".to_string(),
            email: "user@example.com".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.5),
                    ..Default::default()
                }),
                updated_at: Some(old_time),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let before_query = crate::clock::now_unix() as i64;
        let res = query_quota(&mut acc).await.expect("query_quota failed");

        let g5 = res.gemini_5h.expect("expected 5h window");
        let gw = res.gemini_weekly.expect("expected weekly window");
        assert_eq!(g5.fraction, Some(0.85));
        assert_eq!(gw.fraction, Some(0.95));
        assert!(res.updated_at.is_some_and(|u| u >= before_query));

        // Verify persistence in state.db
        let loaded = crate::storage::load_pool().unwrap();
        let stored = &loaded.accounts[0];
        let stored_lq = stored.last_quota.as_ref().unwrap();
        assert_eq!(stored_lq.gemini_5h.as_ref().unwrap().fraction, Some(0.85));
        assert_eq!(stored_lq.gemini_weekly.as_ref().unwrap().fraction, Some(0.95));
        assert!(stored_lq.updated_at.is_some_and(|u| u >= before_query));
    }

    #[tokio::test]
    async fn test_query_quota_partial_response_merges_known_windows() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|_req| {
            // Returns ONLY 5h bucket, omitting weekly
            let body = br#"{
                "groups": [{
                    "buckets": [{
                        "bucketId": "gemini-5h-user",
                        "remainingFraction": 0.70,
                        "resetTime": "2026-09-17T16:00:00Z",
                        "window": "5h"
                    }]
                }]
            }"#
            .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let validation_url = "https://accounts.google.com/verify?id=partial".to_string();
        let mut acc = Account {
            id: "acc-partial".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some(validation_url.clone()),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.60),
                    reset_time: Some(serde_json::Value::String("2026-09-17T12:00:00Z".to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.92),
                    reset_time: Some(serde_json::Value::String("2026-09-24T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some(crate::clock::now_unix() as i64 - 400),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await.expect("query_quota failed");
        assert_eq!(res.gemini_5h.unwrap().fraction, Some(0.70));
        assert_eq!(res.gemini_weekly.unwrap().fraction, Some(0.92));

        let stored_pool = crate::storage::load_pool().unwrap();
        let stored = &stored_pool.accounts[0];
        assert_eq!(stored.status, crate::storage::STATUS_VALIDATION_REQUIRED);
        assert_eq!(stored.validation_url, Some(validation_url));
    }

    #[tokio::test]
    async fn test_query_quota_preserves_auth_restrictions() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|_req| {
            let body =
                br#"{"groups":[{"buckets":[{"bucketId":"gemini-5h-user","remainingFraction":0.7,"window":"5h"}]}]}"#
                    .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        for status in [
            crate::storage::STATUS_VALIDATION_REQUIRED,
            crate::storage::STATUS_AUTH_ERROR,
        ] {
            let validation_url = format!("https://accounts.google.com/verify?id={}", status);
            let cooldown = crate::clock::now_unix() + 86400.0;
            let mut acc = Account {
                id: format!("restricted-{}", status),
                status: status.to_string(),
                validation_url: Some(validation_url.clone()),
                rate_limited_until: Some(cooldown),
                ..Default::default()
            };
            let mut pool = crate::storage::Pool::new_empty();
            pool.accounts.push(acc.clone());
            crate::storage::save_pool(&pool).unwrap();

            let _ = query_quota(&mut acc).await.expect("query_quota failed");

            let stored_pool = crate::storage::load_pool().unwrap();
            let stored = stored_pool.accounts.iter().find(|a| a.id == acc.id).unwrap();
            assert_eq!(stored.status, status);
            assert_eq!(stored.validation_url, Some(validation_url));
            assert_eq!(stored.rate_limited_until, Some(cooldown));
            assert_eq!(
                stored.last_quota.as_ref().unwrap().gemini_5h.as_ref().unwrap().fraction,
                Some(0.7)
            );
        }
    }

    #[tokio::test]
    async fn test_schedule_quota_refresh_stale_account_scheduling() {
        let _env = setup_quota_test_dir();

        let calls = Arc::new(AtomicI32::new(0));
        let calls_clone = calls.clone();
        let srv = TestServer::start(move |_req| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            let body =
                br#"{"groups":[{"buckets":[{"bucketId":"gemini-5h-user","remainingFraction":0.9,"window":"5h"}]}]}"#
                    .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("tok".to_string()))));

        let now = crate::clock::now_unix();

        // Fresh account (updated 10s ago)
        let fresh_acc = Account {
            id: "fresh".to_string(),
            last_quota: Some(QuotaState {
                updated_at: Some((now - 10.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!super::super::refresh_needed(&fresh_acc, now));

        // Stale account (updated 400s ago)
        let stale_acc = Account {
            id: "stale".to_string(),
            last_quota: Some(QuotaState {
                updated_at: Some((now - 400.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(super::super::refresh_needed(&stale_acc, now));

        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(stale_acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let scheduled = schedule_quota_refresh(&stale_acc, Some(now));
        assert!(scheduled);

        // Wait briefly for background task
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            calls.load(Ordering::SeqCst) > 0,
            "expected background refresh HTTP call"
        );
    }

    #[tokio::test]
    async fn test_schedule_quota_refresh_single_flight() {
        let _env = setup_quota_test_dir();

        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let gate_arc = Arc::new(Mutex::new(gate_rx));
        let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);

        let srv = TestServer::start(move |_req| {
            let _ = started_tx.send(true);
            let rx = gate_arc.lock().unwrap().clone();
            tokio::task::block_in_place(|| {
                let mut rx = rx;
                while !*rx.borrow_and_update() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            let body =
                br#"{"groups":[{"buckets":[{"bucketId":"gemini-5h-user","remainingFraction":0.9,"window":"5h"}]}]}"#
                    .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("tok".to_string()))));

        let acc = Account {
            id: "single-flight-acc".to_string(),
            last_quota: Some(QuotaState {
                updated_at: Some(100),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let first = schedule_quota_refresh(&acc, None);
        assert!(first, "expected first schedule to succeed");

        // Wait until HTTP request is in-flight
        while !*started_rx.borrow_and_update() {
            let _ = started_rx.changed().await;
        }

        // Second concurrent call for same account must be rejected (single-flight)
        let second = schedule_quota_refresh(&acc, None);
        assert!(
            !second,
            "expected second concurrent schedule_quota_refresh to return false"
        );
        assert!(is_refresh_in_flight(&acc.id));

        let _ = gate_tx.send(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!is_refresh_in_flight(&acc.id));
    }

    #[tokio::test]
    async fn test_schedule_quota_refresh_backoff_and_reset() {
        let _env = setup_quota_test_dir();

        let status_code = Arc::new(AtomicI32::new(500));
        let sc_clone = status_code.clone();

        let srv = TestServer::start(move |_req| {
            let code = sc_clone.load(Ordering::SeqCst) as u16;
            if code >= 400 {
                return (code, vec![], b"internal error".to_vec());
            }
            let body =
                br#"{"groups":[{"buckets":[{"bucketId":"gemini-5h-user","remainingFraction":0.9,"window":"5h"}]}]}"#
                    .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("tok".to_string()))));

        let acc = Account {
            id: "backoff-acc".to_string(),
            last_quota: Some(QuotaState {
                updated_at: Some(100),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let now = crate::clock::now_unix();

        // First attempt fails (HTTP 500)
        let scheduled = schedule_quota_refresh(&acc, Some(now));
        assert!(scheduled);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let (attempt, next_at, ok) = get_refresh_retry_entry(&acc.id);
        assert!(ok, "expected retry entry to be recorded on failure");
        assert_eq!(attempt, 1);
        let expected_delay = config::QUOTA_REFRESH_BACKOFF[0].as_secs_f64();
        assert!(next_at >= now + expected_delay - 2.0 && next_at <= now + expected_delay + 5.0);

        // Attempting to schedule again before next_at should return false
        assert!(!schedule_quota_refresh(&acc, Some(now + 5.0)));

        // Now switch mock to success
        status_code.store(200, Ordering::SeqCst);

        // Schedule after backoff expires
        let scheduled_after = schedule_quota_refresh(&acc, Some(next_at + 1.0));
        assert!(scheduled_after);

        tokio::time::sleep(Duration::from_millis(100)).await;

        let (_, _, ok_after) = get_refresh_retry_entry(&acc.id);
        assert!(!ok_after, "expected retry entry to be cleared after successful refresh");
    }

    #[tokio::test]
    async fn test_query_quota_fail_closed_in_test_mode() {
        let _env = setup_quota_test_dir();

        let _bg = set_backend_url_base_provider_for_test(None);
        let mut acc = Account {
            id: "test-acc".to_string(),
            ..Default::default()
        };
        let res = query_quota(&mut acc).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("[FAIL-CLOSED TEST GUARD]"));
    }

    #[tokio::test]
    async fn test_query_quota_validation_error() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|_req| {
            let body = br#"{"error":{"message":"VALIDATION_REQUIRED: please verify your account at https://accounts.google.com/verify"}}"#.to_vec();
            (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("tok".to_string()))));

        let mut acc = Account {
            id: "val-acc".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.9),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await.expect("unexpected error");

        assert_eq!(acc.status, "validation_required");
        assert!(acc.rate_limited_until.is_some_and(|r| r > crate::clock::now_unix()));
        assert_eq!(res.gemini_5h.unwrap().fraction, Some(0.0));

        let reloaded = crate::storage::load_pool().unwrap();
        assert_eq!(reloaded.accounts[0].status, "validation_required");
    }

    #[tokio::test]
    async fn test_query_quota_restriction_persistence_failure() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|_req| {
            let body = br#"{"error":{"message":"VALIDATION_REQUIRED"}}"#.to_vec();
            (
                403,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("tok".to_string()))));

        let mut acc = Account {
            id: "val-acc".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let _pg = set_pool_transaction_for_test(Some(Box::new(|_| {
            Err(StorageError::Other("injected persistence failure".to_string()))
        })));

        let res = query_quota(&mut acc).await;
        assert!(res.is_err(), "expected error from persistence failure");
    }

    #[test]
    fn test_quota_freshness_429_invalidation_semantics() {
        let future_5h = "2030-01-01T04:00:00Z";
        let future_weekly = "2030-01-07T00:00:00Z";

        let now = 1700000000.0;
        let mut acc = Account {
            id: "429-acc".to_string(),
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.75),
                    reset_time: Some(serde_json::Value::String(future_5h.to_string())),
                    ..Default::default()
                }),
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.85),
                    reset_time: Some(serde_json::Value::String(future_weekly.to_string())),
                    ..Default::default()
                }),
                updated_at: Some((now - 20.0) as i64),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Initially fresh
        let info_before = super::super::freshness(&acc, now);
        assert_eq!(info_before.class, "fresh");

        // Invalidate freshness by setting UpdatedAt = 0 (as done on 429)
        acc.last_quota.as_mut().unwrap().updated_at = Some(0);

        let info_after = super::super::freshness(&acc, now);
        assert_eq!(info_after.class, "unknown");

        // Invariant: known windows must remain intact
        assert_eq!(
            acc.last_quota.as_ref().unwrap().gemini_5h.as_ref().unwrap().fraction,
            Some(0.75)
        );
        assert_eq!(
            acc.last_quota
                .as_ref()
                .unwrap()
                .gemini_weekly
                .as_ref()
                .unwrap()
                .fraction,
            Some(0.85)
        );
    }

    #[tokio::test]
    async fn test_query_quota_fallback_ignores_non_gemini_default_model() {
        let _env = setup_quota_test_dir();

        let claude_reset = "2030-01-01T05:00:00Z";
        let gemini_reset = "2030-01-01T03:00:00Z";

        let srv = TestServer::start(move |req| {
            if req.path.ends_with("retrieveUserQuotaSummary") {
                return (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    br#"{"groups":[]}"#.to_vec(),
                );
            }
            let body = format!(
                r#"{{"defaultAgentModelId":"claude-sonnet-4-6","models":{{
                    "claude-sonnet-4-6":{{"quotaInfo":{{"remainingFraction":1,"resetTime":"{}"}}}},
                    "gemini-3.8-flash-tiered":{{"quotaInfo":{{"remainingFraction":0.94,"resetTime":"{}"}}}}}}}}"#,
                claude_reset, gemini_reset
            )
            .into_bytes();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let warnings = Arc::new(Mutex::new(Vec::new()));
        let warnings_clone = warnings.clone();
        let _wg = set_warn_hook_for_test(Some(Box::new(move |msg| {
            warnings_clone.lock().unwrap().push(msg.to_string());
        })));

        let mut acc = Account {
            id: "acc-fallback".to_string(),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await.expect("query_quota failed");
        let g5 = res.gemini_5h.expect("expected Gemini 5h");
        assert_eq!(g5.fraction, Some(0.94));
        assert_eq!(g5.reset_time, Some(serde_json::Value::String(gemini_reset.to_string())));

        let w_list = warnings.lock().unwrap();
        assert_eq!(w_list.len(), 1);
        assert!(w_list[0].contains("no Gemini buckets"));
    }

    #[tokio::test]
    async fn test_query_quota_fallback_picks_gemini_model_deterministically() {
        let _env = setup_quota_test_dir();

        let srv = TestServer::start(|req| {
            if req.path.ends_with("retrieveUserQuotaSummary") {
                return (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    br#"{"groups":[]}"#.to_vec(),
                );
            }
            let body = br#"{"models":{
                "gemini-z":{"quotaInfo":{"remainingFraction":0.3,"resetTime":"2030-01-01T00:00:00Z"}},
                "gemini-a":{"quotaInfo":{"remainingFraction":0.7,"resetTime":"2030-01-01T00:00:00Z"}},
                "gemini-m":{"quotaInfo":{"remainingFraction":0.5,"resetTime":"2030-01-01T00:00:00Z"}}}}"#
                .to_vec();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        for i in 0..20 {
            let mut acc = Account {
                id: format!("acc-det-{}", i),
                ..Default::default()
            };
            let mut pool = crate::storage::Pool::new_empty();
            pool.accounts.push(acc.clone());
            crate::storage::save_pool(&pool).unwrap();

            let res = query_quota(&mut acc).await.expect("query_quota failed");
            assert_eq!(
                res.gemini_5h.as_ref().unwrap().fraction,
                Some(0.7),
                "run {}: expected gemini-a (0.7)",
                i
            );
        }
    }

    #[tokio::test]
    async fn test_query_quota_fallback_missing_fraction_means_weekly_exhausted() {
        let _env = setup_quota_test_dir();

        let now_sec = crate::clock::now_unix();
        let reset_ts = jiff::Timestamp::from_second((now_sec + 26.0 * 3600.0) as i64).unwrap();
        let reset = reset_ts.to_string();
        let reset_c = reset.clone();

        let srv = TestServer::start(move |req| {
            if req.path.ends_with("retrieveUserQuotaSummary") {
                return (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    br#"{"groups":[{"buckets":[{"bucketId":"3p-weekly","window":"weekly","remainingFraction":1}]}]}"#
                        .to_vec(),
                );
            }
            let body = format!(
                r#"{{"models":{{"gemini-3-flash":{{"quotaInfo":{{"resetTime":"{}"}}}}}}}}"#,
                reset_c
            )
            .into_bytes();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let mut acc = Account {
            id: "acc-exhausted".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.62),
                    reset_time: Some(serde_json::Value::String("2030-01-01T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await.expect("query_quota failed");
        assert_eq!(res.gemini_5h.as_ref().unwrap().fraction, Some(0.0));
        let gw = res.gemini_weekly.as_ref().unwrap();
        assert_eq!(gw.fraction, Some(0.0));
        assert_eq!(gw.reset_time, Some(serde_json::Value::String(reset)));
    }

    #[tokio::test]
    async fn test_query_quota_fallback_drops_stale_weekly() {
        let _env = setup_quota_test_dir();

        let now_sec = crate::clock::now_unix();
        let reset_ts = jiff::Timestamp::from_second((now_sec + 3.0 * 3600.0) as i64).unwrap();
        let reset = reset_ts.to_string();
        let reset_c = reset.clone();

        let srv = TestServer::start(move |req| {
            if req.path.ends_with("retrieveUserQuotaSummary") {
                return (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    br#"{"groups":[]}"#.to_vec(),
                );
            }
            let body = format!(
                r#"{{"models":{{"gemini-3-flash":{{"quotaInfo":{{"remainingFraction":0.9,"resetTime":"{}"}}}}}}}}"#,
                reset_c
            )
            .into_bytes();
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                body,
            )
        });

        let url = srv.url();
        let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
        let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

        let mut acc = Account {
            id: "acc-healthy".to_string(),
            last_quota: Some(QuotaState {
                gemini_weekly: Some(QuotaWindow {
                    fraction: Some(0.62),
                    reset_time: Some(serde_json::Value::String("2030-01-01T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts.push(acc.clone());
        crate::storage::save_pool(&pool).unwrap();

        let res = query_quota(&mut acc).await.expect("query_quota failed");
        assert_eq!(res.gemini_5h.as_ref().unwrap().fraction, Some(0.9));
        assert!(res.gemini_weekly.is_none(), "expected stale weekly dropped");
    }

    #[test]
    fn test_remaining_fraction_missing_means_zero() {
        let f = remaining_fraction(
            None,
            Some(&serde_json::Value::String("2030-01-01T00:00:00Z".to_string())),
        );
        assert_eq!(f, Some(0.0));

        let f2 = remaining_fraction(None, None);
        assert_eq!(f2, None);

        let f3 = remaining_fraction(Some(&serde_json::json!(0.4)), None);
        assert_eq!(f3, Some(0.4));
    }

    #[test]
    fn test_weekly_unknown_label() {
        let updated = crate::clock::now_unix() as i64;
        let not_renewed = Account {
            last_quota: Some(QuotaState {
                gemini_5h: Some(QuotaWindow {
                    fraction: Some(0.9),
                    reset_time: Some(serde_json::Value::String("2030-01-01T00:00:00Z".to_string())),
                    ..Default::default()
                }),
                updated_at: Some(updated),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            super::super::weekly_unknown_label(&not_renewed),
            "WK -- not reported upstream"
        );

        let empty_acc = Account::default();
        assert_eq!(super::super::weekly_unknown_label(&empty_acc), "WK -- unknown");
    }

    #[tokio::test]
    async fn test_query_quota_exact_gemini_bucket_wins_over_variant() {
        let exact = r#"{"buckets":[{"bucketId":"gemini-weekly","window":"weekly","resetTime":"2030-01-02T00:00:00Z","remainingFraction":0.008},{"bucketId":"gemini-5h","window":"5h","resetTime":"2030-01-01T05:00:00Z","remainingFraction":0.98}]}"#;
        let variant = r#"{"buckets":[{"bucketId":"gemini-argon-wtu-weekly","window":"weekly","resetTime":"2030-01-07T00:00:00Z","remainingFraction":0.56}]}"#;

        for (_name, groups) in [
            ("variant last", format!("{},{}", exact, variant)),
            ("variant first", format!("{},{}", variant, exact)),
        ] {
            let _env = setup_quota_test_dir();
            let groups_c = groups.clone();
            let srv = TestServer::start(move |_req| {
                let body = format!(r#"{{"groups":[{}]}}"#, groups_c).into_bytes();
                (
                    200,
                    vec![("content-type".to_string(), "application/json".to_string())],
                    body,
                )
            });

            let url = srv.url();
            let _bg = set_backend_url_base_provider_for_test(Some(Box::new(move || url.clone())));
            let _tg = set_token_refresher_for_test(Some(Box::new(|_| Ok("mock-token".to_string()))));

            let mut acc = Account {
                id: "acc-variant".to_string(),
                ..Default::default()
            };
            let mut pool = crate::storage::Pool::new_empty();
            pool.accounts.push(acc.clone());
            crate::storage::save_pool(&pool).unwrap();

            let res = query_quota(&mut acc).await.expect("query_quota failed");
            let gw = res.gemini_weekly.expect("expected gemini_weekly");
            assert_eq!(gw.fraction, Some(0.008));
            assert_eq!(
                gw.reset_time,
                Some(serde_json::Value::String("2030-01-02T00:00:00Z".to_string()))
            );
            let g5 = res.gemini_5h.expect("expected gemini_5h");
            assert_eq!(g5.fraction, Some(0.98));
        }
    }
}
