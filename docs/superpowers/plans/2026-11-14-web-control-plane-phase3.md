# Web Control Plane Phase 3 (Config, Reload, Diagnostics) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the dashboard show the running config (without the token), reload `config.json` without a restart, and run the health checks `agy-pool doctor` runs, so config edits and triage no longer need SSH.

**Architecture:** `src/dashboard/control.rs` gains a process-wide `ConfigContext` (config path, applied hash, PID file, startup config, pending restart fields) that `daemon::server::run_foreground` sets after writing the PID file and tests set directly. `GET /api/v1/config` serializes `config::get_static_config()` and strips `dashboard.auth_token`. `POST /api/v1/config/reload` runs `config::load_config_file` (full serde + validation) and either applies everything or nothing; socket and audit-log fields stay pinned to startup values and are reported as `restart_required_fields`; when none are pending, the PID file's `config_hash` is updated so the CLI stops reporting a mismatch. `GET /api/v1/diagnostics` returns structured checks; the upstream DNS/TCP/TLS probe is extracted from `run_doctor` into `diagnostics::probe_upstream` so both share it. `dashboard::serve` routes the reads through a new `token_gate` (the tail of `control_gate`) and the reload through `control_gate`. The frontend adds a "Config & health" drawer.

**Tech Stack:** Rust 2024, tokio, hyper 1.12, http-body-util, serde/serde_json; single-file HTML/JS dashboard. No new crates.

**Spec:** `docs/superpowers/specs/2026-11-12-web-control-plane-design.md` (§4.3.2, §4.5 D, §6 Phase 3, §9; §11 lists the Phase 3 scope decisions this plan follows).

## Global Constraints

- Work in `/home/claude/work/agy-pool-rust` on branch `feat/dashboard-config`. Never touch any other directory or branch.
- Run tests **only** through `timeout 900 scripts/test.sh <cargo args>` (cargo is `~/.cargo/bin/cargo`). Never run `cargo test` directly.
- Never start, stop or restart the live `agy-pool` gateway; never run release/LTO builds or benchmarks on this host.
- Format with `~/.cargo/bin/cargo fmt` (`max_width = 120`) before every commit; `timeout 900 scripts/test.sh clippy --all-targets -- -D warnings` must be clean before each task's commit.
- No new dependencies.
- `dashboard.auth_token` must never appear in any response body. Reload responses name changed fields, never their values.
- A rejected reload (`400`) must leave `get_static_config()` and the PID file exactly as they were.
- Config and diagnostics reads need the token like mutations do (spec §9.6): without a configured token they return `403 TOKEN_REQUIRED_FOR_CONTROL` unless `dashboard.allow_unauthenticated_control` is set.
- Storage errors go to the log (`crate::observability::log_message_level(LogLevel::Error, "DASHBOARD", …)`), not the response. Config validation messages *are* returned: the caller passed the control gate and needs them to fix the file.
- Do not create backup files (`*.bak*`), do not push, do not open PRs. Commit only the files listed in the task. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **The token never leaks.** `GET /api/v1/config` must drop `dashboard.auth_token`, and nothing else in the response may contain it. Pinned by `test_config_endpoint_redacts_token` (Task 1).
2. **A broken `config.json` changes nothing.** Malformed JSON, an invalid value, or an unknown key must return `400 INVALID_CONFIG` and keep the active config and PID hash. Pinned by `test_config_reload` (Task 2).
3. **The PID hash means "the daemon runs this file".** It is updated only when no restart-only field differs from startup, and never written if the PID file now belongs to another process. Pinned by `test_config_reload` (Task 2); the reviewer must check the `info.pid` guard in `update_pid_config_hash`.
4. **Token rotation applies at once.** After a reload that changes `dashboard.auth_token`, the old token gets `401`. Pinned by `test_config_reload` (Task 2).
5. **Diagnostics never hang the request.** The upstream probe runs in `spawn_blocking` and the wait is bounded (10 s); DNS has no timeout of its own. No deterministic test; the reviewer must check `upstream_check`.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/diagnostics/mod.rs` | New `probe_upstream()`; `run_doctor` uses it |
| `src/dashboard/control.rs` | `ConfigContext`, `config_response`, `diagnostics_response`, `reload_config` |
| `src/dashboard/mod.rs` | `token_gate`; routes for `/api/v1/config`, `/api/v1/config/reload`, `/api/v1/diagnostics` |
| `src/daemon/server.rs` | `run_foreground` sets the `ConfigContext` after writing the PID file |
| `src/dashboard/assets/index.html` | "Config & health" drawer |
| `tests/dashboard.rs` | Integration tests |
| `docs/superpowers/specs/2026-11-12-web-control-plane-design.md` | §11 Phase 3 scope (already written by Claude) |

---

### Task 1: Config and diagnostics read endpoints

**Files:**
- Modify: `src/diagnostics/mod.rs` (upstream block in `run_doctor` ~line 1069; add `probe_upstream` after `tls_probe` ~line 503)
- Modify: `src/dashboard/control.rs` (module doc, imports, append new section)
- Modify: `src/dashboard/mod.rs` (`control_gate` ~line 312, `serve` route table ~line 484)
- Modify: `tests/dashboard.rs` (guard `Drop`, helpers, two new tests)

**Interfaces:**
- Produces (used by Task 2 and the daemon):
  - `pub fn agy_pool::diagnostics::probe_upstream() -> (String, Result<(), String>)`
  - `pub struct agy_pool::dashboard::control::ConfigContext { pub config_path: Option<PathBuf>, pub config_hash: Option<String>, pub pid_file: Option<PathBuf>, pub startup: StaticConfig, pub restart_required: Vec<String> }` (`Clone`)
  - `pub fn agy_pool::dashboard::control::set_config_context(ctx: Option<ConfigContext>)`
  - `GET /api/v1/config` → `{"config_path", "config_hash", "restart_required_fields": [...], "config": {…StaticConfig, dashboard: {enabled fields…, "auth_token_configured": bool, "env_token_override": bool}}}`
  - `GET /api/v1/diagnostics` → `{"healthy": bool, "checks": [{"name", "status": "ok"|"warn"|"error", "detail"}]}` with names in order `gateway`, `state_db`, `config`, `native_agy`, `upstream`.

- [ ] **Step 1: Write the failing tests** in `tests/dashboard.rs`.

Extend the guard's `Drop` so no test leaks hooks or context into the next one:

```rust
impl<'a> Drop for DashboardTestGuard<'a> {
    fn drop(&mut self) {
        agy_pool::config::set_static_config(agy_pool::config::default_static_config());
        agy_pool::dashboard::control::set_config_context(None);
        agy_pool::diagnostics::set_tls_prober(None);
        agy_pool::diagnostics::set_agy_binary_finder(None);
        agy_pool::config::reset_data_dir();
        agy_pool::config::set_test_mode(false);
    }
}
```

Add after `stored_account` (~line 1108):

```rust
async fn get_json(addr: SocketAddr, path: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
    let headers = match token {
        Some(t) => hdrs(&[("authorization", bearer(t).as_str())]),
        None => HeaderMap::new(),
    };
    let (status, _, body) = send_request(addr, Method::GET, path, headers, Bytes::new()).await;
    (status, serde_json::from_slice(&body).unwrap_or_default())
}
```

Append at the end of the file:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_config_endpoint_redacts_token() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    let mut cfg = agy_pool::config::default_static_config();
    cfg.dashboard.auth_token = Some(TEST_TOKEN.to_string());
    cfg.model_mapping
        .insert("gemini-old".to_string(), "gemini-new".to_string());
    agy_pool::config::set_static_config(cfg);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _) = get_json(addr, "/api/v1/config", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/config",
        hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains(TEST_TOKEN), "token leaked");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dash = &json["config"]["dashboard"];
    assert!(dash.get("auth_token").is_none());
    assert_eq!(dash["auth_token_configured"], true);
    assert_eq!(dash["env_token_override"], false);
    assert_eq!(json["config"]["model_mapping"]["gemini-old"], "gemini-new");
    assert_eq!(json["config"]["server"]["port"], 8899);
    assert_eq!(json["restart_required_fields"], serde_json::json!([]));

    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/config",
        control_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // Without a token, config and diagnostics stay closed unless the operator opted out.
    set_dashboard_config(None, false);
    for path in ["/api/v1/config", "/api/v1/diagnostics"] {
        let (status, _, body) = send_request(addr, Method::GET, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(error_code(&body), "TOKEN_REQUIRED_FOR_CONTROL");
    }
    set_dashboard_config(None, true);
    let (status, json) = get_json(addr, "/api/v1/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["config"]["dashboard"]["auth_token_configured"], false);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_diagnostics_endpoint() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    agy_pool::diagnostics::set_tls_prober(Some(Arc::new(|_: &str, _: Duration| Ok::<(), String>(()))));
    agy_pool::diagnostics::set_agy_binary_finder(Some(Arc::new(|| "/opt/agy/bin/agy".to_string())));
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _) = get_json(addr, "/api/v1/diagnostics", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, json) = get_json(addr, "/api/v1/diagnostics", Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    let checks = json["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["gateway", "state_db", "config", "native_agy", "upstream"]);
    assert!(checks.iter().all(|c| c["status"] == "ok"), "{json}");
    assert_eq!(json["healthy"], true);
    assert!(checks[1]["detail"].as_str().unwrap().contains("2 accounts"));
    assert!(checks[3]["detail"].as_str().unwrap().contains("/opt/agy/bin/agy"));

    agy_pool::diagnostics::set_tls_prober(Some(Arc::new(|_: &str, _: Duration| {
        Err::<(), String>("dns lookup failed".to_string())
    })));
    agy_pool::diagnostics::set_agy_binary_finder(Some(Arc::new(String::new)));
    let (_, json) = get_json(addr, "/api/v1/diagnostics", Some(TEST_TOKEN)).await;
    assert_eq!(json["checks"][3]["status"], "warn");
    assert_eq!(json["checks"][4]["status"], "warn");
    assert!(json["checks"][4]["detail"].as_str().unwrap().contains("dns lookup failed"));
    assert_eq!(json["healthy"], true, "warnings do not make the gateway unhealthy");

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

- [ ] **Step 2: Run them and confirm they fail**

Run: `timeout 900 scripts/test.sh test --test dashboard -- test_config_endpoint_redacts_token test_diagnostics_endpoint`
Expected: compile error (`set_config_context` not found in `dashboard::control`).

- [ ] **Step 3: Extract `probe_upstream`** in `src/diagnostics/mod.rs`. Add after `tls_probe`:

```rust
/// Resolves, connects to and TLS-handshakes the Cloud Code backend; returns the host it probed.
/// Shared by `doctor` and the dashboard diagnostics.
pub fn probe_upstream() -> (String, Result<(), String>) {
    let backend_host = BACKEND_HOST_PROVIDER
        .read()
        .unwrap()
        .as_ref()
        .map(|f| f())
        .unwrap_or_else(|| "cloudaicompanion.googleapis.com".to_string());
    let res = if let Some(ref probe_fn) = *TLS_PROBER.read().unwrap() {
        probe_fn(&backend_host, Duration::from_secs(5))
    } else {
        default_tls_probe(&backend_host, Duration::from_secs(5))
    };
    (backend_host, res)
}
```

In `run_doctor`, replace everything from `let backend_host = BACKEND_HOST_PROVIDER` through the end of the `let tls_res = …;` statement with:

```rust
    let (backend_host, tls_res) = probe_upstream();
```

- [ ] **Step 4: Add the read handlers** to `src/dashboard/control.rs`.

Replace the first two module-doc lines with:

```rust
//! Dashboard control and inspection handlers. Accounts are addressed by exact id only: the CLI resolver
//! also matches list positions and name substrings, which would be racy and ambiguous over HTTP.
```

Change the imports to:

```rust
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::{Request, Response, StatusCode};

use super::{api_error, internal_error, json_response};
use crate::config::StaticConfig;
use crate::proxy::body::ProxyBody;
use crate::storage::{Account, STATUS_DISABLED, STATUS_VALIDATION_REQUIRED};
```


Append at the end of the file:

```rust
const UPSTREAM_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What config reload and inspection need from the running daemon; `run_foreground` sets it.
#[derive(Clone)]
pub struct ConfigContext {
    pub config_path: Option<PathBuf>,
    /// Hash of the config.json content now applied.
    pub config_hash: Option<String>,
    pub pid_file: Option<PathBuf>,
    /// The config the listeners and log writers were started with.
    pub startup: StaticConfig,
    /// Fields changed in config.json that only a restart applies.
    pub restart_required: Vec<String>,
}

static CONFIG_CONTEXT: RwLock<Option<ConfigContext>> = RwLock::new(None);

pub fn set_config_context(ctx: Option<ConfigContext>) {
    *CONFIG_CONTEXT.write().unwrap_or_else(|e| e.into_inner()) = ctx;
}

fn config_context() -> Option<ConfigContext> {
    CONFIG_CONTEXT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `GET /api/v1/config`: the active config without the dashboard token.
pub fn config_response() -> Response<ProxyBody> {
    let cfg = crate::config::get_static_config();
    let ctx = config_context();
    let mut config = serde_json::to_value(&cfg).unwrap_or_default();
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
    let state_db = match crate::storage::load_pool() {
        Ok(pool) => {
            let enabled = pool.accounts.iter().filter(|a| a.status != STATUS_DISABLED).count();
            check(
                "state_db",
                "ok",
                format!("{} accounts, {enabled} enabled", pool.accounts.len()),
            )
        }
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
async fn upstream_check() -> Check {
    let probe = tokio::task::spawn_blocking(crate::diagnostics::probe_upstream);
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
```

- [ ] **Step 5: Route the reads** in `src/dashboard/mod.rs`.

Replace the `match token { … }` at the end of `control_gate` with `token_gate(req, cfg, token)`, and add above `control_gate`:

```rust
/// Without a token, only an operator opt-out lets requests through. Used by mutations and by
/// the config/diagnostics reads, which expose more than the 0.3.x read API.
fn token_gate(
    req: &Request<hyper::body::Incoming>,
    cfg: &DashboardConfig,
    token: Option<&str>,
) -> Option<Response<ProxyBody>> {
    match token {
        Some(_) => read_gate(req, token),
        None if cfg.allow_unauthenticated_control => None,
        None => Some(api_error(
            StatusCode::FORBIDDEN,
            "TOKEN_REQUIRED_FOR_CONTROL",
            "set dashboard.auth_token or AGY_POOL_DASHBOARD_TOKEN to enable dashboard controls",
        )),
    }
}
```

In `serve`, add `| "/api/v1/config" | "/api/v1/diagnostics"` to the GET-only list (the arm that returns `method_not_allowed("GET")`), and add before the `r if r.starts_with("/api/v1/accounts/") && !is_post` arm:

```rust
        "/api/v1/config" | "/api/v1/diagnostics" => match token_gate(&req, &cfg, token) {
            Some(denied) => denied,
            None if route == "/api/v1/config" => control::config_response(),
            None => control::diagnostics_response().await,
        },
```

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: all pass, including the two new tests and `test_strategy_control_matrix` (unchanged `control_gate` behavior).

Run: `timeout 900 scripts/test.sh test --lib diagnostics::`
Expected: all pass (`run_doctor` output unchanged).

- [ ] **Step 7: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/diagnostics/mod.rs src/dashboard/control.rs src/dashboard/mod.rs tests/dashboard.rs
git commit -m "feat(dashboard): config and diagnostics read endpoints

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Config reload

**Files:**
- Modify: `src/dashboard/control.rs` (append)
- Modify: `src/dashboard/mod.rs` (`serve` route table)
- Modify: `src/daemon/server.rs` (`run_foreground`, right after `super::pid::write_pid_file(&pid_file, pid_info)?;` ~line 516)
- Modify: `tests/dashboard.rs` (helper + one new test)

**Interfaces:**
- Consumes: `ConfigContext`, `set_config_context` (Task 1); `config::load_config_file`, `config::compute_config_file_hash`, `daemon::{read_pid_file, write_pid_file, set_rotate_limits}`.
- Produces: `POST /api/v1/config/reload` → `200 {"ok": true, "applied_fields": [...], "restart_required_fields": [...], "config_hash": "…"}`, `400 NO_CONFIG_FILE`, `400 INVALID_CONFIG`.
  - Live fields (`applied_fields`, compared with the active config, in this order): `model_mapping`, `server.keep_alive_secs`, `native_agy.binary`, `logging.max_size_bytes`, `dashboard.auth_token`, `dashboard.allow_unauthenticated_control`.
  - Restart-only fields (`restart_required_fields`, compared with the startup config, in this order): `server.listen`, `server.port`, `logging.backup_count`, `logging.audit_max_size_bytes`. They keep their startup values in the active config.

- [ ] **Step 1: Write the failing test** in `tests/dashboard.rs`. Add after `get_json`:

```rust
fn write_config(path: &std::path::Path, json: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec(json).unwrap()).unwrap();
}
```

Append at the end of the file:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_config_reload() {
    use agy_pool::dashboard::control::{ConfigContext, set_config_context};

    let _guard = lock_test().await;
    let tmp = setup_test_env();
    let config_path = tmp.path().join("config.json");
    let pid_file = tmp.path().join("agy-pool.pid");
    let base = serde_json::json!({
        "version": 1,
        "model_mapping": { "gemini-old": "gemini-a" },
        "dashboard": { "auth_token": TEST_TOKEN },
    });
    write_config(&config_path, &base);
    let startup = agy_pool::config::load_config_file(&config_path).unwrap();
    agy_pool::config::set_static_config(startup.clone());
    let startup_hash = agy_pool::config::compute_config_file_hash(&config_path).unwrap();
    agy_pool::daemon::write_pid_file(
        &pid_file,
        agy_pool::daemon::DaemonInfo {
            pid: std::process::id() as i32,
            config_hash: Some(startup_hash.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let context = ConfigContext {
        config_path: Some(config_path.clone()),
        config_hash: Some(startup_hash),
        pid_file: Some(pid_file.clone()),
        startup,
        restart_required: Vec::new(),
    };
    set_config_context(Some(context.clone()));
    let pid_hash = || agy_pool::daemon::read_pid_file(&pid_file).unwrap().config_hash;
    let (addr, server_task, shutdown_tx) = start_server().await;

    // Live fields apply at once, and the PID file learns the new hash.
    let mut edited = base.clone();
    edited["model_mapping"]["gemini-old"] = "gemini-b".into();
    edited["server"] = serde_json::json!({ "keep_alive_secs": 7 });
    edited["logging"] = serde_json::json!({ "max_size_bytes": 123456 });
    write_config(&config_path, &edited);
    let (status, body) = post_control(addr, "/api/v1/config/reload", "{}").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(!String::from_utf8_lossy(&body).contains(TEST_TOKEN), "token leaked");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["applied_fields"],
        serde_json::json!(["model_mapping", "server.keep_alive_secs", "logging.max_size_bytes"])
    );
    assert_eq!(json["restart_required_fields"], serde_json::json!([]));
    let edited_hash = agy_pool::config::compute_config_file_hash(&config_path).unwrap();
    assert_eq!(json["config_hash"], edited_hash.as_str());
    assert_eq!(pid_hash().as_deref(), Some(edited_hash.as_str()));
    let active = agy_pool::config::get_static_config();
    assert_eq!(active.model_mapping["gemini-old"], "gemini-b");
    assert_eq!(active.server.keep_alive_secs, 7);
    assert_eq!(active.logging.max_size_bytes, 123456);

    // The control header is still required.
    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/config/reload",
        hdrs(&[
            ("content-type", "application/json"),
            ("authorization", bearer(TEST_TOKEN).as_str()),
        ]),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/config/reload", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // Broken JSON, invalid values and unknown keys change nothing.
    for broken in [
        "{",
        r#"{"version": 1, "server": {"port": 0}}"#,
        r#"{"version": 1, "surprise": true}"#,
    ] {
        std::fs::write(&config_path, broken).unwrap();
        let (status, body) = post_control(addr, "/api/v1/config/reload", "{}").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{broken}");
        assert_eq!(error_code(&body), "INVALID_CONFIG");
    }
    assert_eq!(agy_pool::config::get_static_config(), active);
    assert_eq!(pid_hash().as_deref(), Some(edited_hash.as_str()));

    // Socket fields need a restart: they stay pinned, and the PID hash keeps flagging the difference.
    let mut moved = edited.clone();
    moved["server"]["port"] = 9999.into();
    moved["model_mapping"]["gemini-old"] = "gemini-c".into();
    write_config(&config_path, &moved);
    let (status, body) = post_control(addr, "/api/v1/config/reload", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["applied_fields"], serde_json::json!(["model_mapping"]));
    assert_eq!(json["restart_required_fields"], serde_json::json!(["server.port"]));
    let active = agy_pool::config::get_static_config();
    assert_eq!(active.server.port, 8899);
    assert_eq!(active.model_mapping["gemini-old"], "gemini-c");
    assert_eq!(pid_hash().as_deref(), Some(edited_hash.as_str()));
    let (_, json) = get_json(addr, "/api/v1/config", Some(TEST_TOKEN)).await;
    assert_eq!(json["restart_required_fields"], serde_json::json!(["server.port"]));
    assert_eq!(json["config"]["server"]["port"], 8899);

    // A new token applies at once; the old one stops working.
    let new_token = "fedcba9876543210fedcba9876543210";
    let mut rotated = edited.clone();
    rotated["dashboard"]["auth_token"] = new_token.into();
    write_config(&config_path, &rotated);
    let (status, body) = post_control(addr, "/api/v1/config/reload", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains(new_token), "token leaked");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["applied_fields"],
        serde_json::json!(["model_mapping", "dashboard.auth_token"])
    );
    assert_eq!(json["restart_required_fields"], serde_json::json!([]));
    let rotated_hash = agy_pool::config::compute_config_file_hash(&config_path).unwrap();
    assert_eq!(pid_hash().as_deref(), Some(rotated_hash.as_str()));
    let (status, _) = get_json(addr, "/api/v1/config", Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get_json(addr, "/api/v1/config", Some(new_token)).await;
    assert_eq!(status, StatusCode::OK);

    // No file to reload: a deleted config.json, or a gateway started without one.
    let new_headers = || {
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json"),
            ("authorization", bearer(new_token).as_str()),
        ])
    };
    std::fs::remove_file(&config_path).unwrap();
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/config/reload",
        new_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "NO_CONFIG_FILE");
    set_config_context(Some(ConfigContext {
        config_path: None,
        ..context
    }));
    let (status, _, body) = send_request(
        addr,
        Method::POST,
        "/api/v1/config/reload",
        new_headers(),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "NO_CONFIG_FILE");
    assert_eq!(agy_pool::config::get_static_config().dashboard.auth_token.as_deref(), Some(new_token));

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `timeout 900 scripts/test.sh test --test dashboard -- test_config_reload`
Expected: FAIL — `POST /api/v1/config/reload` returns `404`.

- [ ] **Step 3: Implement the reload** — in `src/dashboard/control.rs` change the imports to `use std::path::{Path, PathBuf};`, `use std::sync::{Mutex, RwLock};` and `use crate::config::{ConfigError, StaticConfig};` (keep the rest), then append:

```rust
/// One reload at a time: two interleaved reloads could pin a stale config.
static RELOAD_LOCK: Mutex<()> = Mutex::new(());

enum ReloadError {
    NoConfigFile(String),
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
        ("logging.backup_count", startup.logging.backup_count != new.logging.backup_count),
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
        ("dashboard.auth_token", current.dashboard.auth_token != new.dashboard.auth_token),
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
    // load_config_file parses and validates everything; on error nothing below runs.
    let new_cfg = crate::config::load_config_file(&path).map_err(|e| match e {
        ConfigError::Io(msg) => ReloadError::NoConfigFile(msg),
        other => ReloadError::Invalid(other.to_string()),
    })?;
    let hash = crate::config::compute_config_file_hash(&path).ok();
    let restart_required = restart_required_fields(&ctx.startup, &new_cfg);
    let applied = applied_fields(&crate::config::get_static_config(), &new_cfg);

    // Restart-only fields keep their startup values so /config shows what is actually running.
    let mut active = new_cfg;
    active.server.listen = ctx.startup.server.listen.clone();
    active.server.port = ctx.startup.server.port;
    active.logging.backup_count = ctx.startup.logging.backup_count;
    active.logging.audit_max_size_bytes = ctx.startup.logging.audit_max_size_bytes;
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
        // The caller passed the control gate; the validation message is what they need to fix the file.
        Ok(Err(ReloadError::Invalid(msg))) => api_error(StatusCode::BAD_REQUEST, "INVALID_CONFIG", &msg),
        Err(_) => internal_error(),
    }
}
```

- [ ] **Step 4: Route it** in `src/dashboard/mod.rs` `serve`, right after the `"/api/v1/config" | "/api/v1/diagnostics"` arm from Task 1:

```rust
        "/api/v1/config/reload" if !is_post => method_not_allowed("POST"),
        "/api/v1/config/reload" => match control_gate(&req, &cfg, token) {
            Some(denied) => denied,
            None => control::reload_config().await,
        },
```

- [ ] **Step 5: Wire the daemon** — in `src/daemon/server.rs` `run_foreground`, directly after `super::pid::write_pid_file(&pid_file, pid_info)?;`:

```rust
    crate::dashboard::control::set_config_context(Some(crate::dashboard::control::ConfigContext {
        config_path: ctx_inst.config_path.clone(),
        config_hash: ctx_inst.config_hash.clone(),
        pid_file: Some(pid_file.clone()),
        startup: crate::config::get_static_config(),
        restart_required: Vec::new(),
    }));
```

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: all pass.

- [ ] **Step 7: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/control.rs src/dashboard/mod.rs src/daemon/server.rs tests/dashboard.rs
git commit -m "feat(dashboard): reload config.json without restarting the gateway

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Frontend — Config & health drawer

**Files:**
- Modify: `src/dashboard/assets/index.html`
- Modify: `tests/dashboard.rs` (`test_embedded_spa_asset_constraints`)

**Interfaces:**
- Consumes: `GET /api/v1/config`, `GET /api/v1/diagnostics`, `POST /api/v1/config/reload` (Tasks 1–2); existing `agyFetch`, `control`, `showToast`, `escapeHtml`, `fetchStatus`, `controlEnabled`.

- [ ] **Step 1: Write the failing asset assertions** — in `test_embedded_spa_asset_constraints`, after the Phase 2 `for needle in [...]` loop:

```rust
    // Config & health drawer (Phase 3).
    for needle in [
        "/api/v1/config",
        "/api/v1/config/reload",
        "/api/v1/diagnostics",
        "config-dialog",
        "config-btn",
    ] {
        assert!(body_str.contains(needle), "missing {needle}");
    }
    assert!(
        body_str.contains("configView.textContent ="),
        "config JSON must be rendered as text, not HTML"
    );
```

- [ ] **Step 2: Run it and confirm it fails**

Run: `timeout 900 scripts/test.sh test --test dashboard -- test_embedded_spa_asset_constraints`
Expected: FAIL — `missing /api/v1/config`.

- [ ] **Step 3: Add the drawer.**

CSS — add after the `#verify-dialog a { … }` rule:

```css
    #config-dialog {
      position: fixed;
      inset: 0 0 0 auto;
      z-index: 50;
      margin: 0;
      width: min(560px, 100vw);
      height: 100vh;
      max-height: 100vh;
      box-sizing: border-box;
      overflow: auto;
      padding: 16px;
      border: 0;
      border-left: 1px solid var(--border);
      background: var(--card-bg);
      color: var(--text-main);
    }
    #config-dialog h2 { font-size: 15px; margin: 0 0 10px; }
    #config-view {
      font: 11px/1.4 var(--font-mono);
      background: var(--bg);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 8px;
      overflow: auto;
    }
    .diag-list { list-style: none; margin: 0 0 10px; padding: 0; font-size: 13px; }
    .diag-list li { padding: 2px 0; }
    .diag-ok { color: var(--green-text); }
    .diag-warn { color: var(--amber-text); }
    .diag-error { color: var(--red-text); }
```

HTML — add right after `</dialog>` of `verify-dialog`. It opens with `show()`, not `showModal()`: a modal's backdrop would cover the toast (z-index 60) that reports reload results.

```html
  <dialog id="config-dialog" aria-labelledby="config-title">
    <h2 id="config-title">Config &amp; health</h2>
    <ul id="diag-list" class="diag-list"></ul>
    <p id="config-restart" class="diag-warn" hidden></p>
    <div class="row-actions">
      <button id="reload-config-btn" class="row-btn" type="button" title="Re-read config.json and apply what can change without a restart">Reload config.json</button>
      <button id="diag-rerun-btn" class="row-btn" type="button">Run checks again</button>
      <form method="dialog"><button class="row-btn" type="submit">Close</button></form>
    </div>
    <pre id="config-view"></pre>
  </dialog>
```

In `header-controls`, add as the first child (before `strategy-select`):

```html
      <button id="config-btn" type="button" class="row-btn" hidden>Config &amp; health</button>
```

JS — in `checkAuthAndStart`, after `document.getElementById('refresh-all-btn').hidden = !controlEnabled;`:

```js
      document.getElementById('config-btn').hidden = !controlEnabled;
```

JS — add after the `refresh-all-btn` click listener (before `function init()`):

```js
    const DIAG_ICONS = { ok: '✓', warn: '⚠', error: '✖' };
    const configView = document.getElementById('config-view');

    function renderConfig(data) {
      configView.textContent = JSON.stringify(Object.assign({ config_path: data.config_path }, data.config), null, 2);
      const restart = data.restart_required_fields || [];
      const note = document.getElementById('config-restart');
      note.hidden = restart.length === 0;
      note.textContent = `Restart the gateway to apply: ${restart.join(', ')}`;
    }

    async function loadConfigPanel() {
      const list = document.getElementById('diag-list');
      list.innerHTML = '<li>Running checks…</li>';
      try {
        const [cfgRes, diagRes] = await Promise.all([agyFetch('/api/v1/config'), agyFetch('/api/v1/diagnostics')]);
        if (cfgRes.ok) renderConfig(await cfgRes.json());
        if (!diagRes.ok) throw new Error(`HTTP ${diagRes.status}`);
        const d = await diagRes.json();
        list.innerHTML = d.checks.map(c => `<li class="diag-${escapeHtml(c.status)}">${DIAG_ICONS[c.status] || '?'} <strong>${escapeHtml(c.name)}</strong>: ${escapeHtml(c.detail)}</li>`).join('');
      } catch (err) {
        list.innerHTML = `<li class="diag-error">Checks failed: ${escapeHtml(String(err))}</li>`;
      }
    }

    document.getElementById('config-btn').addEventListener('click', () => {
      const dlg = document.getElementById('config-dialog');
      if (!dlg.open) dlg.show();
      loadConfigPanel();
    });
    document.getElementById('diag-rerun-btn').addEventListener('click', loadConfigPanel);
    document.getElementById('reload-config-btn').addEventListener('click', async (e) => {
      const btn = e.currentTarget;
      btn.disabled = true;
      const data = await control('/api/v1/config/reload', {});
      btn.disabled = false;
      if (!data) return;
      const applied = data.applied_fields.length ? data.applied_fields.join(', ') : 'nothing changed';
      const restart = data.restart_required_fields;
      showToast(restart.length
        ? `Applied: ${applied}. Restart needed for: ${restart.join(', ')}`
        : `Config reloaded (${applied}).`, restart.length > 0);
      loadConfigPanel();
      fetchStatus();
    });
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: all pass; the asset stays under 90 000 bytes (`wc -c src/dashboard/assets/index.html` ≈ 76 000).

- [ ] **Step 5: Full suite, format, lint, commit**

```bash
~/.cargo/bin/cargo fmt --check
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
timeout 900 scripts/test.sh test
git add src/dashboard/assets/index.html tests/dashboard.rs
git commit -m "feat(dashboard): config and health drawer

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## After all tasks (Claude, not agy)

- Opus review of Task 2 (token rotation, all-or-nothing reload, PID file rewrite); Sonnet review of Tasks 1 and 3.
- Manual browser check on a throwaway gateway with a copy of a config and fake accounts (never the live pool): open the drawer, reload a valid and an invalid config, rotate the token.
- Push, PR, CI.
