//! Dashboard control and inspection handlers. Accounts are addressed by exact id only: the CLI resolver
//! also matches list positions and name substrings, which would be racy and ambiguous over HTTP.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::{Request, Response, StatusCode};

use super::{api_error, internal_error, json_response};
use crate::config::{ConfigError, StaticConfig};
use crate::proxy::body::ProxyBody;
use crate::storage::{Account, STATUS_DISABLED, STATUS_VALIDATION_REQUIRED};

const MAX_NAME_CHARS: usize = 64;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
static REFRESH_ALL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Handles `POST /api/v1/accounts/<rest>` after `control_gate` allowed it.
pub async fn handle(rest: &str, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
    match rest {
        "refresh-all" => refresh_all().await,
        "reorder" => reorder(req).await,
        _ => match rest.split_once('/') {
            Some((id, "enable")) => set_disabled(id, false).await,
            Some((id, "disable")) => set_disabled(id, true).await,
            Some((id, "rename")) => rename(id, req).await,
            Some((id, "refresh")) => refresh(id).await,
            Some((id, "switch")) => switch(id).await,
            _ => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "unknown dashboard API path"),
        },
    }
}

fn ok(extra: serde_json::Value) -> Response<ProxyBody> {
    let mut body = serde_json::json!({ "ok": true });
    if let (Some(obj), serde_json::Value::Object(more)) = (body.as_object_mut(), extra) {
        obj.extend(more);
    }
    json_response(serde_json::to_vec(&body).unwrap_or_default())
}

fn account_not_found() -> Response<ProxyBody> {
    api_error(StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND", "no account with this id")
}

fn invalid(message: &str) -> Response<ProxyBody> {
    api_error(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", message)
}

fn log_error(message: String) {
    crate::observability::log_message_level(crate::observability::LogLevel::Error, "DASHBOARD", &message);
}

fn storage_failure(what: &str, err: impl std::fmt::Display) -> Response<ProxyBody> {
    log_error(format!("failed to {what}: {err}"));
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "STORAGE_ERROR",
        &format!("failed to {what}"),
    )
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request<hyper::body::Incoming>) -> Option<T> {
    let body = http_body_util::Limited::new(req.into_body(), 4096)
        .collect()
        .await
        .ok()?
        .to_bytes();
    serde_json::from_slice(&body).ok()
}

#[allow(clippy::result_large_err)]
async fn load_account(id: &str) -> Result<Option<Account>, Response<ProxyBody>> {
    let id = id.to_owned();
    match tokio::task::spawn_blocking(crate::storage::load_pool).await {
        Ok(Ok(pool)) => Ok(pool.accounts.into_iter().find(|a| a.id == id)),
        Ok(Err(e)) => Err(storage_failure("load the account pool", e)),
        Err(_) => Err(internal_error()),
    }
}

/// Applies `f` to the account with exactly this id inside one pool transaction; `false` if it does not exist.
#[allow(clippy::result_large_err)]
async fn update_account(
    id: &str,
    what: &'static str,
    f: impl FnOnce(&mut Account) + Send + 'static,
) -> Result<bool, Response<ProxyBody>> {
    let id = id.to_owned();
    let res = tokio::task::spawn_blocking(move || {
        let mut found = false;
        crate::storage::pool_transaction(|pool| {
            if let Some(acc) = pool.accounts.iter_mut().find(|a| a.id == id) {
                f(acc);
                found = true;
            }
            Ok(())
        })
        .map(|()| found)
    })
    .await;
    match res {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(e)) => Err(storage_failure(what, e)),
        Err(_) => Err(internal_error()),
    }
}

async fn set_disabled(id: &str, disabled: bool) -> Response<ProxyBody> {
    let what = if disabled {
        "disable the account"
    } else {
        "enable the account"
    };
    // Same rules as `agy-pool disable` / `enable`: enabling only lifts a manual disable.
    match update_account(id, what, move |acc| {
        if disabled {
            acc.status = STATUS_DISABLED.to_string();
        } else if acc.status == STATUS_DISABLED {
            acc.status.clear();
        }
    })
    .await
    {
        Ok(true) => ok(serde_json::json!({})),
        Ok(false) => account_not_found(),
        Err(resp) => resp,
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameRequest {
    name: String,
}

async fn rename(id: &str, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
    let Some(body) = read_json::<RenameRequest>(req).await else {
        return invalid("body must be {\"name\": \"...\"}");
    };
    let name = body.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return invalid(&format!(
            "name must be 1-{MAX_NAME_CHARS} characters without control characters"
        ));
    }
    match update_account(id, "rename the account", move |acc| acc.name = name).await {
        Ok(true) => ok(serde_json::json!({})),
        Ok(false) => account_not_found(),
        Err(resp) => resp,
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReorderRequest {
    ordered_ids: Vec<String>,
}

async fn reorder(req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
    let Some(ReorderRequest { ordered_ids }) = read_json(req).await else {
        return invalid("body must be {\"ordered_ids\": [\"acc_1\", ...]}");
    };
    let res = tokio::task::spawn_blocking(move || {
        let mut applied = false;
        crate::storage::pool_transaction(|pool| {
            let unique: std::collections::HashSet<&String> = ordered_ids.iter().collect();
            // Only a full permutation of the current ids is applied; anything else means the page is stale.
            if unique.len() != ordered_ids.len()
                || ordered_ids.len() != pool.accounts.len()
                || !pool.accounts.iter().all(|a| unique.contains(&a.id))
            {
                return Ok(());
            }
            pool.accounts
                .sort_by_key(|a| ordered_ids.iter().position(|id| *id == a.id));
            applied = true;
            Ok(())
        })
        .map(|()| applied)
    })
    .await;
    match res {
        Ok(Ok(true)) => ok(serde_json::json!({})),
        Ok(Ok(false)) => api_error(
            StatusCode::CONFLICT,
            "POOL_CHANGED",
            "ordered_ids must list every account exactly once; reload and try again",
        ),
        Ok(Err(e)) => storage_failure("reorder the accounts", e),
        Err(_) => internal_error(),
    }
}

async fn switch(id: &str) -> Response<ProxyBody> {
    // Check first so an unknown id is a 404 rather than a switch failure.
    match load_account(id).await {
        Ok(Some(_)) => {}
        Ok(None) => return account_not_found(),
        Err(resp) => return resp,
    }
    match crate::accounts::switch_account_id(id).await {
        Ok(_) => ok(serde_json::json!({})),
        Err(e) => {
            log_error(format!("failed to switch the CLI account to {id}: {e}"));
            api_error(
                StatusCode::BAD_GATEWAY,
                "SWITCH_FAILED",
                "could not switch the CLI account; see the gateway log",
            )
        }
    }
}

/// Queries upstream quota for one account and persists a validation restriction the probe cleared.
/// query_quota runs in its own task: cancelling it would leave its single-flight entry behind and hang
/// every later refresh of this account, so only the wait is bounded.
async fn refresh_account(before: Account) -> Result<(), String> {
    let id = before.id.clone();
    let task = tokio::spawn(async move {
        let mut acc = before.clone();
        let quota = crate::quota::query_quota(&mut acc).await.map_err(|e| e.to_string())?;
        if before.status == STATUS_VALIDATION_REQUIRED && acc.status.is_empty() {
            let cleared = tokio::task::spawn_blocking(move || {
                crate::accounts::persist_cleared_restriction(&before, &acc, &quota)
            })
            .await;
            if let Ok(Err(e)) = cleared {
                return Err(format!("quota refreshed but clearing the restriction failed: {e}"));
            }
        }
        Ok::<(), String>(())
    });
    let res = match tokio::time::timeout(REFRESH_TIMEOUT, task).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => Err("refresh task panicked".to_string()),
        Err(_) => Err(format!(
            "timed out after {}s (still running)",
            REFRESH_TIMEOUT.as_secs()
        )),
    };
    if let Err(e) = &res {
        log_error(format!("quota refresh for {id} failed: {e}"));
    }
    res
}

async fn refresh(id: &str) -> Response<ProxyBody> {
    let before = match load_account(id).await {
        Ok(Some(acc)) => acc,
        Ok(None) => return account_not_found(),
        Err(resp) => return resp,
    };
    if before.status == STATUS_DISABLED {
        return api_error(
            StatusCode::CONFLICT,
            "ACCOUNT_DISABLED",
            "enable the account before refreshing its quota",
        );
    }
    let res = refresh_account(before).await;
    let stored = match load_account(id).await {
        Ok(Some(acc)) => acc,
        Ok(None) => return account_not_found(),
        Err(resp) => return resp,
    };
    // A still-restricted account is a normal outcome: hand back the link the operator must open.
    if stored.status == STATUS_VALIDATION_REQUIRED {
        let url = stored.validation_url.filter(|u| u.starts_with("https://"));
        return ok(serde_json::json!({ "status": stored.status, "validation_url": url }));
    }
    match res {
        Ok(()) => ok(serde_json::json!({ "status": stored.status, "validation_url": null })),
        Err(_) => api_error(
            StatusCode::BAD_GATEWAY,
            "QUOTA_REFRESH_FAILED",
            "quota refresh failed; see the gateway log",
        ),
    }
}

/// Clears a single-flight flag even when the client disconnects and the handler future is dropped.
struct FlagGuard(&'static AtomicBool);

impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn refresh_all() -> Response<ProxyBody> {
    if REFRESH_ALL_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return api_error(
            StatusCode::CONFLICT,
            "REFRESH_ALREADY_IN_PROGRESS",
            "a refresh of all accounts is already running",
        );
    }
    let _guard = FlagGuard(&REFRESH_ALL_IN_FLIGHT);
    let pool = match tokio::task::spawn_blocking(crate::storage::load_pool).await {
        Ok(Ok(pool)) => pool,
        Ok(Err(e)) => return storage_failure("load the account pool", e),
        Err(_) => return internal_error(),
    };
    let mut tasks = tokio::task::JoinSet::new();
    for acc in pool.accounts.into_iter().filter(|a| a.status != STATUS_DISABLED) {
        tasks.spawn(refresh_account(acc));
    }
    let (mut refreshed, mut failed) = (0u32, 0u32);
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Ok(())) => refreshed += 1,
            _ => failed += 1,
        }
    }
    ok(serde_json::json!({ "refreshed": refreshed, "failed": failed }))
}

const UPSTREAM_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
static UPSTREAM_PROBE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// What config reload and inspection need from the running daemon; `run_foreground` sets it.
#[derive(Clone)]
pub struct ConfigContext {
    pub config_path: Option<PathBuf>,
    /// Hash of the config.json content now applied.
    pub config_hash: Option<String>,
    pub pid_file: Option<PathBuf>,
    /// config.json as loaded at startup; reloads compare against it to find restart-only changes.
    pub startup: StaticConfig,
    /// Fields changed in config.json that only a restart applies.
    pub restart_required: Vec<String>,
}

static CONFIG_CONTEXT: RwLock<Option<ConfigContext>> = RwLock::new(None);

pub fn set_config_context(ctx: Option<ConfigContext>) {
    *CONFIG_CONTEXT.write().unwrap_or_else(|e| e.into_inner()) = ctx;
}

/// Called at shutdown; leaves the context alone if another instance in this process has replaced it.
pub fn clear_config_context(pid_file: &Path) {
    let mut ctx = CONFIG_CONTEXT.write().unwrap_or_else(|e| e.into_inner());
    if ctx.as_ref().is_some_and(|c| c.pid_file.as_deref() == Some(pid_file)) {
        *ctx = None;
    }
}

fn config_context() -> Option<ConfigContext> {
    CONFIG_CONTEXT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `GET /api/v1/config`: the active config without the dashboard token.
pub fn config_response() -> Response<ProxyBody> {
    let cfg = crate::config::get_static_config();
    let ctx = config_context();
    let mut config = serde_json::to_value(&cfg).unwrap_or_default();
    // Nothing reads scheduler.strategy at runtime: the live strategy is kept in state.db (see /status).
    if let Some(obj) = config.as_object_mut() {
        obj.remove("scheduler");
    }
    if let Some(dash) = config.get_mut("dashboard").and_then(|d| d.as_object_mut()) {
        dash.remove("auth_token");
        let configured = cfg.dashboard.auth_token.as_deref().is_some_and(|t| !t.is_empty());
        let env_override = std::env::var(crate::config::ENV_DASHBOARD_TOKEN).is_ok_and(|t| !t.is_empty());
        dash.insert("auth_token_configured".to_string(), configured.into());
        dash.insert("env_token_override".to_string(), env_override.into());
    }
    let body = serde_json::json!({
        "config_path": ctx.as_ref().and_then(|c| c.config_path.as_ref()).map(|p| p.display().to_string()),
        "config_hash": ctx.as_ref().and_then(|c| c.config_hash.clone()),
        "restart_required_fields": ctx.map(|c| c.restart_required).unwrap_or_default(),
        "config": config,
    });
    json_response(serde_json::to_vec(&body).unwrap_or_default())
}

#[derive(serde::Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

fn check(name: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

fn config_check() -> Check {
    let defaults = || check("config", "ok", "no config.json; running with built-in defaults");
    let Some(ctx) = config_context() else {
        return defaults();
    };
    let Some(path) = &ctx.config_path else {
        return defaults();
    };
    match crate::config::compute_config_file_hash(path) {
        Err(e) => check("config", "warn", e.to_string()),
        Ok(hash) if Some(&hash) != ctx.config_hash.as_ref() => check(
            "config",
            "warn",
            "config.json changed since it was loaded; reload to apply it",
        ),
        Ok(_) if !ctx.restart_required.is_empty() => check(
            "config",
            "warn",
            format!("restart the gateway to apply: {}", ctx.restart_required.join(", ")),
        ),
        Ok(_) => check("config", "ok", format!("{} matches the running config", path.display())),
    }
}

fn local_checks() -> Vec<Check> {
    let gateway = check(
        "gateway",
        "ok",
        format!("agy-pool {} (pid {})", crate::VERSION, std::process::id()),
    );
    let state_db = match crate::storage::account_counts() {
        Ok((total, enabled)) => check("state_db", "ok", format!("{total} accounts, {enabled} enabled")),
        Err(e) => {
            log_error(format!("diagnostics could not read the account pool: {e}"));
            check("state_db", "error", "cannot read state.db; see the gateway log")
        }
    };
    let native_agy = match crate::diagnostics::resolve_native_agy_binary() {
        Ok((path, source)) => check("native_agy", "ok", format!("{} (source: {source})", path.display())),
        Err(e) => check("native_agy", "warn", e),
    };
    vec![gateway, state_db, config_check(), native_agy]
}

/// The probe blocks (DNS has no timeout of its own), so it runs on a blocking thread and only the wait is bounded.
/// One probe at a time: a hung probe must not pile up blocking threads as the page is reloaded.
async fn upstream_check() -> Check {
    if UPSTREAM_PROBE_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return check(
            "upstream",
            "warn",
            "an earlier probe is still waiting for upstream; try again later",
        );
    }
    let probe = tokio::task::spawn_blocking(|| {
        let _guard = FlagGuard(&UPSTREAM_PROBE_IN_FLIGHT);
        crate::diagnostics::probe_upstream()
    });
    match tokio::time::timeout(UPSTREAM_PROBE_TIMEOUT, probe).await {
        Ok(Ok((host, Ok(())))) => check("upstream", "ok", format!("{host}:443 reachable (DNS, TCP, TLS)")),
        Ok(Ok((host, Err(e)))) => check("upstream", "warn", format!("{host}:443: {e}")),
        Ok(Err(_)) => check("upstream", "error", "probe task failed"),
        Err(_) => check(
            "upstream",
            "warn",
            format!("no answer within {}s", UPSTREAM_PROBE_TIMEOUT.as_secs()),
        ),
    }
}

/// `GET /api/v1/diagnostics`: the `doctor` checks that matter for a running gateway, as data.
pub async fn diagnostics_response() -> Response<ProxyBody> {
    let (local, upstream) = tokio::join!(tokio::task::spawn_blocking(local_checks), upstream_check());
    let Ok(mut checks) = local else {
        return internal_error();
    };
    checks.push(upstream);
    let healthy = checks.iter().all(|c| c.status != "error");
    json_response(serde_json::to_vec(&serde_json::json!({ "healthy": healthy, "checks": checks })).unwrap_or_default())
}

/// One reload at a time: two interleaved reloads could pin a stale config.
static RELOAD_LOCK: Mutex<()> = Mutex::new(());

enum ReloadError {
    NoConfigFile(String),
    Unreadable(String),
    Invalid(String),
}

fn changed_fields(fields: &[(&str, bool)]) -> Vec<String> {
    fields
        .iter()
        .filter(|(_, changed)| *changed)
        .map(|(name, _)| name.to_string())
        .collect()
}

/// Fields the listeners and log writers took at startup; only a restart applies them.
fn restart_required_fields(startup: &StaticConfig, new: &StaticConfig) -> Vec<String> {
    changed_fields(&[
        ("server.listen", startup.server.listen != new.server.listen),
        ("server.port", startup.server.port != new.server.port),
        (
            "logging.backup_count",
            startup.logging.backup_count != new.logging.backup_count,
        ),
        (
            "logging.audit_max_size_bytes",
            startup.logging.audit_max_size_bytes != new.logging.audit_max_size_bytes,
        ),
    ])
}

/// Fields read per request or per use, so `set_static_config` applies them at once.
fn applied_fields(current: &StaticConfig, new: &StaticConfig) -> Vec<String> {
    changed_fields(&[
        ("model_mapping", current.model_mapping != new.model_mapping),
        (
            "server.keep_alive_secs",
            current.server.keep_alive_secs != new.server.keep_alive_secs,
        ),
        ("native_agy.binary", current.native_agy.binary != new.native_agy.binary),
        (
            "logging.max_size_bytes",
            current.logging.max_size_bytes != new.logging.max_size_bytes,
        ),
        // Compares the effective tokens: AGY_POOL_DASHBOARD_TOKEN overrides config.json.
        (
            "dashboard.auth_token",
            crate::config::dashboard_token(&current.dashboard) != crate::config::dashboard_token(&new.dashboard),
        ),
        (
            "dashboard.allow_unauthenticated_control",
            current.dashboard.allow_unauthenticated_control != new.dashboard.allow_unauthenticated_control,
        ),
    ])
}

fn update_pid_config_hash(pid_file: &Path, hash: Option<String>) -> Result<(), crate::daemon::DaemonError> {
    let mut info = crate::daemon::read_pid_file(pid_file)?;
    // Never rewrite a PID file another process has claimed since.
    if info.pid != std::process::id() as i32 {
        return Ok(());
    }
    info.config_hash = hash;
    crate::daemon::write_pid_file(pid_file, info)
}

fn reload_blocking() -> Result<serde_json::Value, ReloadError> {
    let _lock = RELOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ctx = config_context();
    let Some((ctx, path)) = ctx.and_then(|c| c.config_path.clone().map(|p| (c, p))) else {
        return Err(ReloadError::NoConfigFile(
            "the gateway was started without a config.json; restart it with one to enable reload".to_string(),
        ));
    };
    // A missing file is the operator's to fix; any other read failure (e.g. permissions) is CONFIG_UNREADABLE.
    if let Ok(false) = path.try_exists() {
        return Err(ReloadError::NoConfigFile(format!(
            "{} no longer exists",
            path.display()
        )));
    }
    // load_config_file parses and validates everything; on error nothing below runs.
    let (new_cfg, hash) = crate::config::load_config_file_with_hash(&path).map_err(|e| match e {
        ConfigError::Io(msg) => ReloadError::Unreadable(msg),
        other => ReloadError::Invalid(other.to_string()),
    })?;
    let hash = Some(hash);
    let restart_required = restart_required_fields(&ctx.startup, &new_cfg);
    let running = crate::config::get_static_config();
    let applied = applied_fields(&running, &new_cfg);

    // Restart-only fields keep their running values so /config shows what is actually running.
    let mut active = new_cfg;
    active.server.listen = running.server.listen;
    active.server.port = running.server.port;
    active.logging.backup_count = running.logging.backup_count;
    active.logging.audit_max_size_bytes = running.logging.audit_max_size_bytes;
    crate::daemon::set_rotate_limits(active.logging.max_size_bytes, active.logging.backup_count as i32);
    crate::config::set_static_config(active);

    // The CLI compares this hash with config.json; only a fully applied file may clear its mismatch warning.
    if restart_required.is_empty()
        && let Some(pid_file) = &ctx.pid_file
        && let Err(e) = update_pid_config_hash(pid_file, hash.clone())
    {
        log_error(format!("config reloaded but updating the PID file failed: {e}"));
    }
    crate::observability::log_message_level(
        crate::observability::LogLevel::Info,
        "DASHBOARD",
        &format!("config reloaded; applied {applied:?}, restart required for {restart_required:?}"),
    );
    set_config_context(Some(ConfigContext {
        config_hash: hash.clone(),
        restart_required: restart_required.clone(),
        ..ctx
    }));
    Ok(serde_json::json!({
        "applied_fields": applied,
        "restart_required_fields": restart_required,
        "config_hash": hash,
    }))
}

/// Handles `POST /api/v1/config/reload` after `control_gate` allowed it. Applies all or nothing.
pub async fn reload_config() -> Response<ProxyBody> {
    match tokio::task::spawn_blocking(reload_blocking).await {
        Ok(Ok(body)) => ok(body),
        Ok(Err(ReloadError::NoConfigFile(msg))) => api_error(StatusCode::BAD_REQUEST, "NO_CONFIG_FILE", &msg),
        Ok(Err(ReloadError::Unreadable(msg))) => {
            log_error(format!("config reload failed: {msg}"));
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "CONFIG_UNREADABLE", &msg)
        }
        // The caller passed the control gate; the validation message is what they need to fix the file.
        Ok(Err(ReloadError::Invalid(msg))) => api_error(StatusCode::BAD_REQUEST, "INVALID_CONFIG", &msg),
        Err(_) => internal_error(),
    }
}
