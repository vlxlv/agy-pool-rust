# agy-pool-rust Web Control Plane Design (`axum` + Auth + Listener Isolation)

- **Date**: 2026-11-12
- **Status**: Revised 2026-11-13 after review — §9 overrides any conflicting text above
- **Supersedes / Extends**: [`2026-11-11-monitoring-dashboard-design.md`](2026-11-11-monitoring-dashboard-design.md)
- **Target Components**: `Cargo.toml`, `src/config/mod.rs`, `src/daemon/server.rs`, `src/proxy/mod.rs`, `src/dashboard/*`, `src/diagnostics/mod.rs`, `tests/dashboard.rs`

---

## 1. Intent & Motivation (The Why)

`agy-pool-rust` (currently `v0.3.4`) ships an embedded read-only monitoring dashboard served from `src/dashboard/mod.rs` and `src/dashboard/assets/index.html` over the same `hyper` HTTP/1.1 listener as the reverse proxy (`127.0.0.1` / Tailscale IPs).

In remote headless deployments (such as Tailscale gateway hosts), operators frequently monitor quotas from laptops or phones, yet still have to SSH into the host to perform routine pool actions:
- Enable or disable an account
- Rename an account label
- Trigger an on-demand quota refresh or clear a transient restriction after completing verification
- Reorder accounts or switch the active CLI base account (`agy-pool switch`)
- Switch load-balancing strategy (`smart` ↔ `max_quota`)
- Reload live configuration (`model_mapping`, `keep_alive_secs`) or inspect diagnostics

Meanwhile, the codebase already has two critical architectural properties that make a Web Control Plane clean and safe:
1. **Runtime pool state in `state.db` is hot-read on every proxy request**: `Handler::handle_proxy` (`src/proxy/mod.rs`) calls `storage::load_pool()` and reads `pool.strategy`, account status, cooldowns, and ordering on every generation request. Mutating `state.db` via existing `pool_transaction` / `accounts` helpers takes effect immediately without restarting the proxy.
2. **`src/dashboard/mod.rs` currently uses manual HTTP/1.1 path matching locked to `GET`-only (`Method Not Allowed` otherwise) without authentication or listener separation**. Scaling this hand-rolled router into authenticated mutating control endpoints would create fragile, duplicated validation and CSRF logic.

This design migrates the built-in web server to an **`axum` control plane (`Approach B`)** with `hyper-util` HTTP/1.1 + HTTP/2 (`h2c`) auto-negotiation, graceful shutdown, layered CSRF + token/session authentication, optional dedicated management listener isolation, and runtime control endpoints backed directly by existing `accounts` / `storage` / `quota` / `config` primitives.

---

## 2. Goals, Non-Goals, & Anti-Goals

### 2.1 Goals
1. **`axum` control router with zero-copy reverse-proxy fallback**: Move `/` , `/dashboard`, `/metrics`, `/healthz`, and `/api/v1/*` into an `axum::Router`, routing all other requests to `Handler::handle_proxy` via `.fallback(...)` without buffering streaming LLM request/response bodies.
2. **HTTP/1.1 + HTTP/2 (`h2c`) & connection lifecycle hardening**: Replace raw `http1::Builder` + `accept_tasks.abort_all()` in `src/daemon/server.rs` with `hyper_util::server::conn::auto::Builder` (`http1` + `http2`), `header_read_timeout`, and `GracefulShutdown` paired with a daemon shutdown signal so active SSE tabs drain cleanly instead of blocking shutdown.
3. **Full runtime pool & strategy control from the Web UI**: Support enabling/disabling accounts, renaming labels, refreshing single/all account quotas (`crate::quota::query_quota`), reordering/sorting accounts (`move`/`reorder`/`sort`), switching the active CLI base account (`accounts::switch_account`), and switching `pool.strategy` (`smart` / `max_quota`).
4. **Sanitized config inspection, live reload, & diagnostics**: Expose redacted `StaticConfig` inspection (`GET /api/v1/config`), live reload of reloadable fields via `POST /api/v1/config/reload` and Unix `SIGHUP`, and structured health/diagnostic status (`GET /api/v1/diagnostics`).
5. **Defense-in-depth security**:
   - Preserve and enforce `Host` / `Origin` / `Sec-Fetch-Site` DNS-rebinding & CSRF checks across dashboard & API routes.
   - Add mandatory custom header + JSON enforcement (`X-Agy-Pool-Control: 1`, `Content-Type: application/json`) on mutating routes.
   - Add optional `dashboard.auth_token` (or `AGY_POOL_DASHBOARD_TOKEN` env var) gating all `/api/v1/*` data and control operations via `Authorization: Bearer <token>` or signed `HttpOnly; SameSite=Strict` session cookie.
   - When no `auth_token` is configured, restrict mutating requests to verified kernel peer addresses in loopback or Tailscale CGNAT/ULA ranges (unless explicitly overridden).
   - Add optional listener separation (`dashboard.listen` / `dashboard.port`) so untrusted proxy listeners never expose the control plane.
6. **Single-binary simplicity**: Keep the SPA as an embedded zero-build HTML/CSS/JS asset (`include_str!("assets/index.html")`), with zero external runtime dependencies or Node.js build steps.

### 2.2 Non-Goals (Explicitly Deferred)
- **Web-based Google OAuth login (`agy-pool login`)**: Remote headless servers require copying Google's `localhost` redirect callback URL back into the server, which is brittle over a remote browser and better handled via CLI + SSH or `export`/`import`.
- **Destructive account deletion (`remove_account`) or raw credential export/import**: OAuth `refresh_token` / bundle JSON is never served over HTTP APIs, and permanent account removal stays CLI-only.
- **Editing bind addresses/ports (`server.listen`, `server.port`, `dashboard.listen`, `dashboard.port`) or daemon `stop`/`restart` via Web**: Preventing self-lockout; lifecycle & listener topology stay under CLI / systemd / Ansible control.
- **Multi-user accounts / RBAC / OIDC**: Single-operator admin token model matches `agy-pool` deployment scope.

### 2.3 Anti-Goals
- **No second async/HTTP runtime**: Do not add `actix-web` or separate non-`hyper` server stacks alongside `tokio` + `hyper 1.12`.
- **No proxy latency regression or body buffering**: `axum` middleware/extractors must not consume or inspect bodies on reverse-proxy fallback routes.
- **No divergent storage writes**: Control endpoints must never bypass `pool_transaction` / `accounts` / `quota::query_quota` invariants with ad-hoc SQLite queries or `config.json` writes.
- **No `X-Forwarded-For` trust**: Peer network trust checks for unauthenticated Tailscale/loopback access must inspect only the raw TCP peer `SocketAddr`, never spoofable proxy headers.

---

## 3. Approaches Considered & Selected Tradeoffs

| Approach | Mechanism | Pros | Tradeoffs / Why Chosen or Rejected |
| :--- | :--- | :--- | :--- |
| **A. Incremental `POST` handlers in `hyper`** | Keep manual path matching in `src/dashboard/mod.rs`; add `POST` branches and inline header checks | Smallest diff; zero new crates; minimal binary size growth | Routing, JSON parsing error handling, auth/CSRF middleware, and optional split-listener dispatch become repetitive boilerplate quickly once >3 mutations + login + reload exist |
| **B. `axum` Control Plane + `fallback` Proxy + Auth + Optional Split Listener (Selected)** | Mount `/dashboard`, `/metrics`, `/healthz`, `/api/v1/*` on `axum::Router`; convert `axum` errors/auth cleanly in tower layers; route all unmatched paths to `Handler::handle_proxy` (`hyper::Request` pass-through); use `hyper-util` `auto::Builder` + `GracefulShutdown` | Shares exact `tokio` + `hyper 1.12` + `tower-service` foundation already in `Cargo.toml`; declarative middleware cleanly enforces Host/Origin/Auth/CSRF once; zero-copy proxy fallback; HTTP/2 unblocks SSE connection limits; clean path for split `dashboard.listen/port` | Adds `axum`, `matchit`, `tower` crates (~hundreds of KB release binary growth, slightly longer clean build) — acceptable for long-term maintainability & security |
| **C. Full Remote Web Management (incl. OAuth + Daemon Lifecycle)** | Add remote OAuth callback relay, credential backup download/upload, inline `config.json` port editing, and daemon self-restart | Eliminates CLI even for initial provisioning or listener changes | High security & reliability risk (token/refresh_token exposure over HTTP, OAuth redirect relay fragility, instant Web lockout on bad bind config) — rejected |

---

## 4. Detailed Architecture & Design

### 4.1 Crate Dependencies (`Cargo.toml`)
- Enable `http2` feature on existing `hyper` and `hyper-util` dependencies (or `server-auto` + `http2` so `hyper_util::server::conn::auto::Builder` supports both HTTP/1.1 and h2c HTTP/2).
- Add `axum = { version = "0.8", default-features = false, features = ["http1", "http2", "json", "query", "tokio"] }`.
- Add `tower = { version = "0.5", default-features = false, features = ["util"] }` if needed for `TowerToHyperService` / layer composition (reuse existing `tower-service`, `sha2`, `hmac`, `getrandom`, `bytes`, `http-body-util` already in `Cargo.toml` — no duplicate crypto crates needed).

### 4.2 Server Topology, HTTP/2 & Graceful Shutdown (`src/daemon/server.rs`, `src/proxy/mod.rs`)

1. **Per-connection peer metadata**:
   - In `listener.accept().await`, capture `(stream, peer_addr)`.
   - Insert `PeerAddr(peer_addr)` (`struct PeerAddr(pub SocketAddr)`) into each incoming `Request`'s extensions inside the hyper connection service wrapper **before** handing the request to `axum::Router`. This works identically for single-listener and split-listener modes without relying on `axum::serve` replacing `server.rs`'s existing multi-listener bind loop, `bound_port_tx`, or fail-closed socket error handling.

2. **Protocol & timeout hardening**:
   - Replace `hyper::server::conn::http1::Builder::new()` with `hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())`:
     - `http1().pipeline_flush(true).header_read_timeout(Duration::from_secs(30))`
     - `http2().keep_alive_interval(Some(Duration::from_secs(20))).keep_alive_timeout(Duration::from_secs(10))`
   - Wrap connection serving with `hyper_util::server::graceful::GracefulShutdown`:
     - Pass a shared `shutdown_notify: tokio::sync::watch::Receiver<bool>` into SSE tasks (`/api/v1/events`) so open SSE streams exit immediately upon shutdown instead of stalling `graceful.shutdown()`.
     - On shutdown (`SIGTERM` / `SIGINT` / `shutdown_rx`), abort `accept_tasks`, broadcast `shutdown_notify = true`, and await `tokio::time::timeout(Duration::from_secs(5), graceful.shutdown())` before tearing down telemetry and `stats_writer`.

3. **Listener Modes**:
   - **Shared Mode (default, when `dashboard.enabled == true` and `dashboard.port.is_none()` and `dashboard.listen.is_empty()`)**:
     - Primary listeners (`server.listen` × `server.port`) run `control_and_proxy_router` where dashboard/control routes sit above `.fallback(proxy_fallback)`.
   - **Split Mode (when `dashboard.enabled == true` and (`dashboard.port.is_some()` or `!dashboard.listen.is_empty()`))**:
     - Primary listeners (`server.listen` × `server.port`) run `proxy_only_router` (`/healthz`, `/metrics`, `.fallback(proxy_fallback)` — `/dashboard` and `/api/v1/*` fall through or return `404`; to ensure clients probing `/dashboard` on the proxy port don't forward accidentally upstream to Google, explicitly return `404 Not Found` for `/`, `/dashboard`, `/dashboard/*`, and `/api/v1/*` on proxy-only listeners).
     - Dedicated control listeners bind `resolved_dashboard_addrs` (`dashboard.listen` × `dashboard.port.unwrap_or(actual_port)`) and run `dashboard_only_router` (dashboard + `/api/v1/*` + `/healthz` + `/metrics`, `.fallback(|| async { StatusCode::NOT_FOUND })`).
     - Validation rule: if split mode resolves to an `(ip, port)` pair that overlaps with primary `(ip, port)`, reject config at startup with a descriptive `DaemonError` / `ConfigError` rather than failing obscurely on `EADDRINUSE` mid-bind. If `dashboard.port` is `Some(0)` (e.g., tests), bind ephemeral port just like `server.port = 0` and expose both `runtime_listen` and `dashboard_listen` in runtime metadata (`StatusResponse` + `DaemonInfo` optional field).
   - **Disabled Mode (`dashboard.enabled == false`)**:
     - Do not bind extra listeners and do not serve `/dashboard` or `/api/v1/*` (return `404` for `/dashboard` & `/api/v1/*` on proxy listener so stray browser visits don't hit upstream Google endpoints, while `/healthz` and `/metrics` remain active).

4. **Trailing-Slash Path Compatibility**:
   - Preserve `v0.3.4` behavior where both `/dashboard` and `/dashboard/` (and `/api/v1/status` and `/api/v1/status/`, etc.) match seamlessly by stripping non-root trailing slashes in an outer path-normalization layer on dashboard routes **before** `axum` routing, while keeping `/v1beta/...` proxy fallback paths completely untouched.

---

### 4.3 Configuration Schema & Live Reload (`src/config/mod.rs`)

Extend `StaticConfig` (`#[serde(deny_unknown_fields)]`) with `#[serde(default)] pub dashboard: DashboardConfig`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_enabled")] // true
    pub enabled: bool,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_listen",
        serialize_with = "serialize_listen_opt"
    )]
    pub listen: Vec<String>,
    #[serde(default)]
    pub port: Option<i64>, // None = share primary listener; 0 = ephemeral (tests); 1..=65535
    #[serde(default)]
    pub auth_token: Option<String>, // Empty string treated as None / rejected if whitespace-only
    #[serde(default)]
    pub allow_unauthenticated_control: bool, // Escape hatch if token=None on non-CGNAT/non-loopback LAN
}
```

Validation rules in `validate_config`:
- Every entry in `dashboard.listen` passes `is_valid_listen_target`, and `"0.0.0.0"` / `"::"` cannot be combined with other targets (same rules as `server.listen`).
- If `dashboard.port` is `Some(p)`, `p` must be `0..=65535` (or `1..=65535` unless test mode, matching `server.port` validation).
- If `!dashboard.listen.is_empty()` and `dashboard.port.is_none()`, require an explicit `dashboard.port` (reject ambiguous split-listen without dedicated port, since primary `server.listen` is bound to `server.port`).
- If `dashboard.port.is_some()` and `dashboard.listen.is_empty()`, default control listener binding to `["127.0.0.1"]` (fail-safe localhost-only control port unless operator explicitly sets Tailscale/LAN address).
- Environment variable override: `AGY_POOL_DASHBOARD_TOKEN` (if non-empty) overrides `dashboard.auth_token` at startup and on reload.

#### Startup Validation & Live Config Reload (`set_static_config`, `reload_static_config`, `SIGHUP`)
1. **Strict Daemon Startup Policy (Fail Closed)**:
   - Keep `daemon::instance::resolve_instance_context` lenient when reading `config.json` (`load_config_file().ok()`) so management CLI commands (`stop`, `status`) continue working while an operator fixes a broken config file.
   - Inside daemon startup (`start_instance` / `run_foreground`), explicitly parse and validate any explicit or existing default `config.json` strictly (**fail closed with `DaemonError` instead of silently falling back to `StaticConfig::default()`**, so a JSON typo in `dashboard.auth_token`, `dashboard.listen`, `dashboard.port`, or `model_mapping` can never silently disable auth or expose control endpoints).
   - Call `crate::config::set_static_config(loaded_cfg)` in `run_foreground` and record the startup `StaticConfig` snapshot + `startup_config_hash` + `pid_file` path in daemon runtime metadata.

2. **Live Config Reload (`POST /api/v1/config/reload` + Unix `SIGHUP`)**:
   - If `config_path` is `None` or missing on disk, return `400 Bad Request` (`NO_CONFIG_FILE`) on HTTP API and log warning on `SIGHUP`.
   - Call `load_config_file(&path)` (which runs full `serde` + `validate_config`; invalid JSON or validation error aborts reload cleanly without touching `ACTIVE_STATIC_CONFIG`).
   - Compare against the **startup** `StaticConfig` snapshot (so cumulative edits vs running sockets stay accurate across multiple reloads) as well as the current `ACTIVE_STATIC_CONFIG`:
     - **Applied live immediately** via `set_static_config(new_cfg)`:
       - `model_mapping` (read per request via `get_static_config()` in `src/proxy/mod.rs:912`)
       - `server.keep_alive_secs` (read per request in `src/proxy/mod.rs:924`)
       - `native_agy.binary` (read per resolution in `src/diagnostics/mod.rs:308`)
       - `dashboard.auth_token` (if not overridden by `AGY_POOL_DASHBOARD_TOKEN`) and `dashboard.allow_unauthenticated_control`
     - **Requires restart** (reported in `restart_required_fields: Vec<String>` and pinned to startup values until process restart):
       - `server.listen`, `server.port`, `dashboard.enabled`, `dashboard.listen`, `dashboard.port`, `logging.*`
     - **PID `DaemonInfo.config_hash` update rule**:
       - If `restart_required_fields` (relative to startup `StaticConfig`) is **empty**, atomically update `DaemonInfo.config_hash = Some(new_hash)` in `agy-pool.pid` so CLI `status`/`run`/`start` do not falsely report `ConfigMismatch`.
       - If `restart_required_fields` is **non-empty**, keep `DaemonInfo.config_hash` at its startup hash so CLI `ConfigMismatch` warnings accurately signal that socket/logging changes require a restart.
     - Note on `scheduler.strategy`: `state.db` (`pool.strategy`) is the runtime source of truth while the daemon runs (CLI `agy-pool strategy` writes `state.db`). Therefore reload does **not** silently overwrite `state.db` `pool.strategy`; instead, if `new_cfg.scheduler.strategy != current_pool.strategy`, report `scheduler.strategy` in response metadata (`config_strategy_differs: true`) so the user decides explicitly.

---

### 4.4 Security, Authentication, & CSRF Model (`src/dashboard/mod.rs` & `control.rs`)

All routes under `/`, `/dashboard`, and `/api/v1/*` pass through defense-in-depth middleware (**proxy fallback routes NEVER pass through dashboard middleware**):

1. **Layer 1 — Host / Origin / Fetch-Metadata Guard (all requests to `/` / `/dashboard` / `/api/v1/*`)**:
   - Retains existing `is_valid_host_and_origin` semantics (rejects multiple `Host`/`Origin` headers, unknown public domain suffixes to block DNS rebinding, cross-site `Origin` mismatches, and `Sec-Fetch-Site: cross-site` for non-top-level-navigation API/fetch requests).
   - Updated hostname allowlist also recognizes any explicit hosts/IPs bound in `dashboard.listen` as well as `server.listen`.

2. **Layer 2 — Explicit Control Header & Content-Type on Mutations (`POST` / `PUT` / `PATCH` / `DELETE`)**:
   - All mutating dashboard API endpoints (including `/api/v1/auth/login` and `/api/v1/auth/logout`) MUST receive:
     - `X-Agy-Pool-Control: 1` (custom non-simple header forcing CORS preflight — blocked cross-origin because no `Access-Control-Allow-Origin` is returned).
     - `Content-Type: application/json` for any endpoint accepting a JSON body (or empty-body actions like `/enable`, `/disable`, `/refresh`, `/switch`, `/reload`, `/logout` still require `X-Agy-Pool-Control: 1`).

3. **Layer 3 — Authentication & Trusted-Peer Gate**:
   - Determine effective token: `std::env::var("AGY_POOL_DASHBOARD_TOKEN").ok().filter(|s| !s.is_empty()).or_else(|| cfg.dashboard.auth_token.clone().filter(|s| !s.is_empty()))`.
   - **When an effective `auth_token` IS configured (`mode = "token"`)**:
     - `GET /`, `GET /dashboard`, `GET /api/v1/auth/status`, and `POST /api/v1/auth/login` are reachable without prior authentication so the browser can load the UI shell (HTML static asset contains zero sensitive runtime/account/email/config data) and prompt for the token.
     - **Every other `/api/v1/*` route (all reads including `status`, `accounts`, `forecast`, `events`, `config`, `diagnostics`, `logs`, AND all control mutations)** requires EITHER:
       - Header `Authorization: Bearer <token>` matching via constant-time `sha2::Sha256` digest + fixed-time XOR comparison, OR
       - Cookie `agy_session=<payload>.<hmac_hex>` verified against an ephemeral 32-byte per-daemon-process CSPRNG key (`getrandom::fill`) concatenated with a hash of the current `auth_token` (so both daemon restarts AND live token rotation via config reload immediately invalidate old cookies).
     - Cookie attributes: `HttpOnly; SameSite=Strict; Path=/; Max-Age=43200` (12 hours; expiration checked inside signed payload). `Secure` is intentionally omitted because Tailnet MagicDNS HTTP endpoints (`http://sjc:8890`) are plain HTTP over WireGuard (`Secure` cookies would be dropped by browsers on `http://`!).
     - SSE `EventSource('/api/v1/events')` automatically sends the `HttpOnly; SameSite=Strict` cookie without leaking secrets into URL query strings or browser history.
     - **Login brute-force protection**: In-memory sliding tracker per `PeerAddr.ip()` — after 5 failed `/api/v1/auth/login` attempts within 60s, return `429 Too Many Requests` (`Retry-After: 60`) for that IP for 60s (bounded map capped at, e.g., 1024 entries to avoid unbounded memory growth).
   - **When NO `auth_token` is configured**:
     - Read `GET` routes continue to work under Layer 1 (`Host`/`Origin`) for backward compatibility with `v0.3.4` (`mode = "trusted_network"` or `"open"`).
     - Mutating `POST` control endpoints inspect raw TCP `PeerAddr.ip()`:
       - Allowed (`mode = "trusted_network"`) iff `ip.is_loopback()` OR IPv4 in `100.64.0.0/10` (RFC6598 Tailscale CGNAT) OR IPv6 in `fd7a:115c:a1e0::/48` (Tailscale ULA) OR `dashboard.allow_unauthenticated_control == true` (`mode = "open"`).
       - Otherwise return `403 Forbidden` with code `TOKEN_REQUIRED_FOR_REMOTE_CONTROL` telling the operator to configure `dashboard.auth_token` or `AGY_POOL_DASHBOARD_TOKEN`.

---

### 4.5 Control Plane API Contracts (`/api/v1/*`)

All responses use `Cache-Control: no-store`. Structured error payload:
```json
{
  "error": {
    "code": "INVALID_ARGUMENT",
    "message": "Account name cannot be empty"
  }
}
```

#### A. Auth & Status
- `GET /api/v1/auth/status` -> `200 OK`:
  ```json
  { "auth_required": true, "authenticated": false, "mode": "token" }
  ```
- `POST /api/v1/auth/login` -> body `{ "token": "..." }` -> `200 OK` + `Set-Cookie: agy_session=...` or `401 Unauthorized` / `429 Too Many Requests`.
- `POST /api/v1/auth/logout` -> `200 OK` + `Set-Cookie: agy_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0`.
- `GET /api/v1/status`: keeps existing `StatusResponse` fields (`version`, `status`, `listen`, `strategy`, `uptime_secs`, `memory_rss_bytes`, `in_flight_requests`, `total_requests`, `qps`, `latency`) and adds optional metadata: `dashboard_listen` (if split mode), `control_mode` (`"token" | "trusted_network" | "open"`), `config_reloadable: bool`, `restart_required_fields: Vec<String>`.

#### B. Strategy Control
- `POST /api/v1/strategy` -> body `{ "strategy": "smart" | "max_quota" }`
  - Validates against `[STRATEGY_SMART, STRATEGY_MAX_QUOTA]`.
  - Runs `storage::pool_transaction(|p| { p.strategy = strategy.clone(); Ok(()) })` inside `spawn_blocking`.
  - Broadcasts `telemetry::notify_pool_changed("strategy", Some(&strategy))`.
  - Returns `200 OK` `{ "ok": true, "strategy": "smart" }`.

#### C. Account Pool Controls (Addressing Strictly by Immutable `account.id`)
All account routes address `{id}` matched strictly against `acc.id` (never volatile 1-based list indices, eliminating race conditions when sorting or reordering concurrently across browser tabs). Unknown ID -> `404 Not Found` (`ACCOUNT_NOT_FOUND`).

1. `POST /api/v1/accounts/{id}/enable` & `POST /api/v1/accounts/{id}/disable`
   - Calls `accounts::enable_account(&id)` / `accounts::disable_account(&id)` (resolving by exact `id`) inside `spawn_blocking`.
   - Note: `enable_account` / `disable_account` reject ambiguous matches; matching strictly on `acc.id` inside a helper or passing verified `id` prevents numeric-index ambiguity when `id` happens to be digits (`"1"`, `"2"` — important! Check `find_account_by_target` vs `resolve_account_index`: in `accounts/mod.rs`, `is_digits("1")` matches 1-based index OR id, so **control endpoints MUST resolve by exact `acc.id` first**, never falling back to 1-based array position!).
   - Broadcasts `pool_changed`, returns updated single `AccountDto` + `{ "ok": true }`.
2. `POST /api/v1/accounts/{id}/rename` -> body `{ "name": "Work-Pro" }`
   - Matches `accounts::rename_account` CLI rules: validates trimmed name is non-empty (`<= 64` chars, no ASCII control characters) and **allows duplicate display names**. Resolves strictly by exact `id` inside `pool_transaction`.
   - Broadcasts `pool_changed`, returns updated `AccountDto`.
3. `POST /api/v1/accounts/{id}/refresh`
   - Async handler with timeout (`15s`): loads account snapshot for `id` (`404 ACCOUNT_NOT_FOUND` if missing). If `acc.status == STATUS_DISABLED`, returns `409 Conflict` (`ACCOUNT_DISABLED`: `"Enable account before refreshing quota"`), honoring `crate::quota::query_quota`'s disabled-account guard cleanly.
   - Checks `crate::quota::refresh::is_refresh_in_flight` / single-flight coalescing so concurrent clicks don't duplicate upstream Google API calls.
   - Captures `was_validation_required` and `snap = accounts::snapshot_account(&acc)`, then calls `crate::quota::query_quota(&mut acc).await` (the real upstream quota + restriction prober, **not** `accounts::verify_account` alone since `set_quota_prober` is a test-only hook). Note that `probe_and_apply_validation_challenge` clears `acc.status`, `validation_url`, and `rate_limited_until` in memory upon passing, while `finalize_and_persist_quota` only writes `last_quota`; therefore, if `was_validation_required && acc.status.is_empty()` and `acc.last_quota` succeeded, the `/refresh` handler additionally invokes `accounts::clear_validation_restriction_cas(&acc, &snap, &q)` (or `auth::persist_account_fields`) to persist the cleared restriction in `state.db` without changing background/CLI `finalize_and_persist_quota` behavior.
   - If the updated account still has `status == "validation_required"`, formats and returns `validation_url` along with refreshed `account: AccountDto` and `probe_status`.
   - Broadcasts `pool_changed`.
4. `POST /api/v1/accounts/refresh-all`
   - Protected by a process-wide atomic/lock guard (`REFRESH_ALL_IN_FLIGHT: AtomicBool` or single-flight `tokio::sync::Notify`) -> concurrent clicks return `409 Conflict` (`REFRESH_ALREADY_IN_PROGRESS`).
   - Probes all enabled accounts (`status != STATUS_DISABLED`) with bounded concurrency (`futures`/`tokio::sync::Semaphore` with permits = 3, per-account timeout `15s`), using the same restriction-clear persistence check per account.
   - Broadcasts `pool_changed`, returns `{ "ok": true, "refreshed": N, "failed": M, "results": [...] }`.
5. `POST /api/v1/accounts/{id}/switch`
   - Calls `accounts::switch_account` (resolved strictly by `id`) to update `active_account_id` in `state.db` and sync native CLI token file (`~/.gemini/antigravity` or configured native path).
   - Note: `switch_account` returns an error if `AGY_TEST_MODE=1` targets real home paths or token refresh fails; return `500` / `400` with sanitized error message.
   - Broadcasts `pool_changed`, returns `{ "ok": true, "active_account_id": id }`.
6. `POST /api/v1/accounts/reorder`
   - Accepts either:
     - Explicit full order: `{ "mode": "explicit", "ordered_ids": ["id3", "id1", "id2"] }` -> validates `ordered_ids` is an exact permutation of current pool account IDs (else `409 POOL_CHANGED` / `400 INVALID_ORDER`, preventing silent loss if an account was added/removed concurrently via CLI) and persists atomically via `pool_transaction`.
     - Deterministic sort: `{ "mode": "sort", "by": "quota" | "hits" | "name" | "status", "reverse": false }` -> delegates to `crate::accounts::order::sort_accounts` (`refresh = false`, fast local sort). **Note**: `"smart"` sort is intentionally omitted here because `accounts::order::sort_accounts("smart", ...)` depends on `SMART_ORDER_HOOK`, which is unpopulated in production (`None` falls back to a silent no-op); runtime smart balancing is controlled via `POST /api/v1/strategy`.
   - Broadcasts `pool_changed`, returns updated `ordered_ids`.

#### D. Config & Diagnostics
- `GET /api/v1/config` -> `200 OK`: returns sanitized `StaticConfig`:
  - `config_path`, `config_hash` (if loaded), `server` (`listen`, `port`, `keep_alive_secs`), `scheduler` (`strategy`), `logging` (`max_size_bytes`, `backup_count`, `audit_max_size_bytes`), `model_mapping`, `native_agy` (`binary`), `dashboard` (`enabled`, `listen`, `port`, `allow_unauthenticated_control`, `auth_token_configured: bool`, `env_token_override: bool`). **Never includes raw `auth_token`.**
- `POST /api/v1/config/reload` -> reloads `config.json` from `config_path` (shared helper with Unix `SIGHUP`):
  - Returns `{ "ok": true, "applied_fields": [...], "restart_required_fields": [...], "config_hash": "..." }`.
  - If `auth_token` changed and session cookies exist, cookie verification automatically fails closed because the server's HMAC key binds the active token digest.
- `GET /api/v1/diagnostics` -> runs structured internal health checks (`StateDb` integrity/latency, on-disk `config.json` hash vs running `DaemonInfo.config_hash` / `restart_required_fields` drift, native `agy` binary resolution check via `diagnostics::resolve_native_agy_binary()`, active TLS/proxy environment flags, pool ready/cooldown/restricted summary) without writing ANSI output or shelling out.

#### E. Real-time Sync via SSE (`/api/v1/events`)
- Keep `event: audit` (replay last 20 + live stream) and `event: stats` (every 2s).
- Add `event: pool_changed`:
  ```text
  event: pool_changed
  data: {"reason":"strategy","detail":"max_quota","ts":1794382000}
  ```
  Emitted after any successful mutation (`strategy`, `enable`, `disable`, `rename`, `refresh`, `refresh-all`, `switch`, `reorder`, `config/reload`). Any connected browser tab receiving `pool_changed` triggers a debounced (`150ms`) refresh of `/api/v1/accounts` and `/api/v1/status`.

---

### 4.6 Frontend Control UX (`src/dashboard/assets/index.html`)

Keep zero-build single-file HTML/CSS/JS architecture (preserving existing light/dark/system theme switcher and `#endpoint-pill`):
1. **Auth Handling**:
   - On page bootstrap, check `GET /api/v1/auth/status` first. If `auth_required && !authenticated`, display a compact centered Token Login card (with password input calling `POST /api/v1/auth/login` with `X-Agy-Pool-Control: 1`); only start `EventSource` and fetch `/api/v1/accounts` / `/api/v1/status` after authentication succeeds.
   - Any `401` on subsequent API calls resets UI to the login modal.
2. **Header Control Strip**:
   - Replace static strategy text in `#endpoint-pill` or pair it with a compact segmented control (`smart` / `max_quota`), a `↻ Refresh All` button (disabled while in-flight), a `Config & Health` drawer trigger, and a lock/logout icon when `auth_required`.
   - Show a dismissible warning banner if `restart_required_fields` is non-empty after a config reload.
3. **Account Table Row Actions**:
   - Enable/Disable toggle pill (optimistic disabled state while pending, confirms before disabling the last active account).
   - Inline Rename (`✎` opens inline text input + Enter/Save, Esc/Cancel).
   - Single-account `↻` Refresh Quota button (if response includes `validation_url`, opens a modal showing the URL to copy/open cleanly).
   - Reorder arrows (`↑` / `↓` + `Sort ▾` header menu for `quota`/`hits`/`name`/`status` — safer on mobile/touch than drag-and-drop alone).
   - `Set CLI Base` action in a compact row `⋯` menu (with confirmation prompt since it updates `~/.gemini/antigravity` CLI active credentials).
4. **Config & Diagnostics Drawer**:
   - Displays sanitized `StaticConfig`, `model_mapping` table, `Reload Config` button, and diagnostics checks (`StateDb`, native binary, config drift).
5. **Security & Rendering Invariant**:
   - Every dynamic string (`account.name`, `account.email`, `model_mapping`, `validation_url`, server error messages) continues to pass through `esc()` HTML escaping, and `validation_url` links strictly enforce `https://` / `http://` scheme allowlist before rendering `<a href>`.
   - `window.agyFetch(url, opts)` helper automatically attaches `X-Agy-Pool-Control: 1` and `Content-Type: application/json` for mutations and surfaces structured `{ error: { message } }` in a bottom-right toast.

---

## 5. Module Organization

```text
src/
├── config/mod.rs          // DashboardConfig, env token helper, validate_config, reload helper
├── daemon/server.rs       // hyper-util auto::Builder, GracefulShutdown, PeerAddr, split listener, SIGHUP reload, SSE shutdown watch
├── proxy/mod.rs           // Zero-copy ProxyBody fallback adapter for unmatched axum fallback requests
├── diagnostics/mod.rs     // Structured DiagnosticSnapshot helper reused by CLI doctor & /api/v1/diagnostics
└── dashboard/
    ├── mod.rs             // axum Router builders (shared/split/disabled), Host/Origin/Auth/CSRF tower layers, SSE + shutdown select
    ├── control.rs         // Mutating REST handlers (/auth/*, /strategy, /accounts/*, /config/*, /diagnostics), HMAC cookie signer, login rate-limiter
    ├── dto.rs             // StatusResponse extensions, AccountDto, DiagnosticDto, SanitizedConfigDto
    ├── telemetry.rs       // Add PoolChanged broadcast variant or dedicated notify channel
    └── assets/
        └── index.html     // Single-file SPA with login modal, strategy switch, account row actions, config/diagnostics drawer
```

---

## 6. Phased Implementation Plan

1. **Phase 1 — Transport & Security Foundation**:
   - Add `axum` + `http2` features to `Cargo.toml`.
   - Add `DashboardConfig` + validation + `reload_from_disk` to `src/config/mod.rs`.
   - Upgrade `src/daemon/server.rs` to `auto::Builder` + `GracefulShutdown` + `PeerAddr` extension + SSE shutdown watch + optional split dashboard listener + `SIGHUP`.
   - Migrate existing `GET` routes and `Host`/`Origin`/`Sec-Fetch-Site` validation into `axum::Router` + `fallback` proxy in `src/dashboard/mod.rs` & `src/proxy/mod.rs`.
   - Implement `auth_token` + `HttpOnly; SameSite=Strict` HMAC cookie auth + login rate-limit + trusted-network peer check.
2. **Phase 2 — Core Runtime Controls & UI**:
   - Implement `/api/v1/strategy`, `/api/v1/accounts/{id}/{enable,disable,rename,refresh,switch}`, `/api/v1/accounts/refresh-all`, `/api/v1/accounts/reorder`, and `pool_changed` SSE event in `src/dashboard/control.rs` & `telemetry.rs`.
   - Update `src/dashboard/assets/index.html` with auth modal, strategy toggle, row controls, `Refresh All`, `validation_url` modal, and live toast notifications.
3. **Phase 3 — Config Inspection, Reload, & Diagnostics**:
   - Implement `/api/v1/config`, `/api/v1/config/reload`, `/api/v1/diagnostics`, and UI drawer.
   - Optional follow-up: redacted log viewer if needed later (`GET /api/v1/logs/recent`), omitted from initial phases unless explicitly prioritized.

---

## 7. Verification & Test Plan

1. **Unit Tests**:
   - `config/mod.rs`: `DashboardConfig` parsing (string vs list `listen`, invalid port, split-listen without port, `deny_unknown_fields`, env token override, reload diff classification between live-applied vs `restart_required`).
   - `control.rs`: Constant-time token verification, HMAC cookie mint/verify/expiry/tamper rejection, token rotation invalidating existing cookies, loopback + Tailscale CGNAT (`100.64.0.0/10`) + ULA (`fd7a:115c:a1e0::/48`) classification, login brute-force `429` throttling.
2. **Integration Tests (`tests/dashboard.rs`)**:
   - Preserve all 7 existing dashboard/proxy isolation tests (updated to include `X-Agy-Pool-Control` checks or auth setups).
   - Security matrix:
     - Mutations without `X-Agy-Pool-Control: 1` rejected (`403`).
     - Mutations from cross-site `Origin` or `Sec-Fetch-Site: cross-site` rejected (`403`).
     - With `auth_token` configured: unauthenticated `/api/v1/*` returns `401`; `/` HTML shell loads without leaking account data; `/auth/login` sets `HttpOnly; SameSite=Strict` cookie allowing subsequent `GET` + `POST` + SSE; invalid/expired cookie returns `401`.
     - Without `auth_token`: simulated public non-CGNAT peer returns `403 TOKEN_REQUIRED_FOR_REMOTE_CONTROL` on `POST`.
   - Control roundtrips:
     - `strategy` switch updates `state.db` and `StatusResponse`, emits `pool_changed` SSE event, and affects subsequent proxy routing.
     - Exact-ID resolution prevents numeric account ID `"1"` vs 1-based array index collision when reordering.
     - `reorder` rejects stale partial/mismatched `ordered_ids` (`409`).
     - `config/reload` applies `model_mapping` live without dropping proxy listener and rejects invalid `config.json` atomically (`400`) while keeping old active config.
   - Split-listener test: proxy port returns `404` on `/dashboard` & `/api/v1/*`, dashboard port returns `404` on `/v1beta/*` proxy paths.
   - Graceful shutdown test: open SSE connection exits cleanly within `< 1s` on daemon shutdown.
3. **CI / Static Checks**:
   - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, full `AGY_TEST_MODE=1 cargo test`, release binary size delta check, and updated `tests/dashboard.rs` embedded SPA constraint checks.

---

## 8. Explicit Design Decisions & Items Left to the Builder

### 8.1 Decisions Settled in This Spec
1. **Immutable `account.id` addressing**: Control APIs resolve accounts strictly by `acc.id` (exact equality), never falling back to 1-based UI position, avoiding both UI index drift races and numeric-ID vs position collisions.
2. **Real upstream probe via `crate::quota::query_quota`**: `/accounts/{id}/refresh` and `/accounts/refresh-all` invoke `crate::quota::query_quota` (returning `409 ACCOUNT_DISABLED` if disabled) rather than `accounts::verify_account` alone (whose global `get_quota_prober()` hook is `None` in production daemon builds), and persist cleared `validation_required` restrictions cleanly via `clear_validation_restriction_cas`.
3. **Fail-closed startup config + conditional PID hash updates**: `start_instance` / `run_foreground` fail closed on malformed `config.json` (leaving `stop`/`status` lenient) and wire `set_static_config`; live reloads update `DaemonInfo.config_hash` only when zero restart-required fields differ from startup.
4. **Ephemeral per-process cookie signing key bound to token digest**: Avoids persisting web sessions in SQLite while ensuring daemon restarts and config-reloaded token changes automatically revoke prior browser sessions.
5. **Tailscale-friendly default + fail-closed public default when untokenized**: Unauthenticated mutations require loopback or Tailscale IP ranges unless explicitly opted out via `allow_unauthenticated_control: true`, protecting accidental `0.0.0.0` deployments without breaking zero-config Tailnet UX.
6. **`h2c` HTTP/2 + `GracefulShutdown` + SSE shutdown watch**: Unblocks browser 6-connection HTTP/1.1 limits when multiple SSE tabs stay open while guaranteeing clean shutdown without hanging on open event streams.

### 8.2 Left to the Builder
- Exact Rust helper ergonomics for normalizing trailing slashes cleanly in `axum` without affecting fallback proxy URIs.
- Exact `axum` 0.8 feature set and `TowerToHyperService` glue in `src/daemon/server.rs`.
- Specific UI layout refinements (toast stacking, mobile table collapse behavior, row `⋯` action menu styling) inside `src/dashboard/assets/index.html`.
- Exact diagnostic check structs in `src/diagnostics/mod.rs` to keep CLI `doctor` and `/api/v1/diagnostics` sharing one source of truth.

---

## 9. Revisions After Review (2026-11-13)

These decisions override the sections above wherever they conflict.

1. **Static config loading** — fixed separately in PRs #5 and #6 (0.3.5). The daemon now fails to start on an invalid `config.json` or `AGY_POOL_DASHBOARD_TOKEN` instead of falling back to defaults, so a typo can never silently drop the dashboard token (§4.3.1).
2. **Mutations always need the token**, including from loopback and Tailscale peers. The trusted-network mode (§4.4 "When NO `auth_token` is configured", CGNAT/ULA ranges) is dropped. Without a token, every mutation returns `403 TOKEN_REQUIRED_FOR_CONTROL` unless `dashboard.allow_unauthenticated_control: true`. Reads stay open without a token (0.3.x behavior); with a token, every `/api/v1/*` route except `GET /api/v1/auth/status` needs it.
3. **No HTTP/2 / h2c.** Browsers never speak h2c, so it brings no SSE connection-limit relief. The server keeps `http1::Builder`.
4. **Rollback:** the `dashboard` key stays in `config.json`. Because `StaticConfig` uses `deny_unknown_fields`, 0.3.5 and older reject a file containing it; the README documents removing the key before downgrading.
5. **Auth transport: Bearer token only.** The browser keeps the token in `sessionStorage` and sends `Authorization: Bearer <token>`; SSE is read through `fetch()` streaming because `EventSource` cannot send headers. No cookies, no HMAC session signing, no `/api/v1/auth/login` or `/logout`. `X-Agy-Pool-Control: 1` and `Content-Type: application/json` remain mandatory on mutations.
6. **New read endpoints** (`/config`, `/diagnostics`, Phase 3) always require the token, and any proxy URL they show is redacted.
7. **Token strength:** a configured token must be at least 32 visible ASCII characters. Wrong tokens count against a per-client budget (5 per 60 s; IPv6 counted per /64) and a global budget (100 per 60 s); either one exhausted returns `429` with `Retry-After: 60`. Missing tokens do not count.
8. **No axum.** With cookies, login endpoints and h2c gone, axum's middleware and protocol benefits no longer apply, and its `!Sync` body type and automatic `Allow: GET,HEAD` header would need adapters. Phase 1 extends the existing hand-written dispatch: `dashboard::owns()` decides which requests the dashboard answers, and `dashboard::serve()` runs the Host/Origin guard, the auth gates and the handlers. Proxy requests never enter the dashboard code. Revisit if Phase 2/3 routing becomes unwieldy.

Phase 1 is therefore: `dashboard.auth_token` / `allow_unauthenticated_control` config + env override, fail-closed startup, Bearer auth + failure limiter, `PeerAddr`, `GET /api/v1/auth/status`, `POST /api/v1/strategy`, and the frontend token prompt + fetch-stream SSE + strategy selector. Split listeners, graceful shutdown, `header_read_timeout`, the `pool_changed` SSE event and all other controls are deferred to later plans.

## 10. Phase 2 Scope Decisions (2026-11-13)

1. **Endpoints:** `POST /api/v1/accounts/{id}/{enable,disable,rename,refresh,switch}`, `/api/v1/accounts/refresh-all`, `/api/v1/accounts/reorder` (`{"ordered_ids": [...]}`, full permutation only, `409 POOL_CHANGED` otherwise). All go through the Phase 1 control gate. Mutation responses are `{"ok": true, ...}` and the browser re-fetches `/api/v1/accounts`; they do not return an `AccountDto`.
2. **Exact ids everywhere.** Handlers match `acc.id` exactly. `accounts::switch_account` now tries an exact id before the CLI's position/name/substring resolver, which also fixes `agy-pool switch <id>` when an id looks like a position or another account's name.
3. **Dropped from Phase 2:** the `sort` reorder mode (the CLI keeps `agy-pool sort`; the UI has ↑/↓), and the `pool_changed` SSE event (the acting tab re-fetches at once and other tabs catch up within the 5 s poll).
4. **Refresh never cancels `query_quota`.** Its single-flight map would keep a dropped leader's entry and hang every later refresh of that account, so the dashboard spawns the query and only bounds the wait (15 s). Refresh-all runs every enabled account at once (pools are small) behind a process-wide in-flight flag (`409 REFRESH_ALREADY_IN_PROGRESS`).
5. **Verification links:** only `https://` URLs are returned; the UI shows them in a dialog. Rename uses `prompt()`, destructive actions use `confirm()`, and errors use a small toast instead of `alert()`.

## 11. Phase 3 Scope Decisions (2026-11-14)

1. **Endpoints:** `GET /api/v1/config`, `POST /api/v1/config/reload`, `GET /api/v1/diagnostics`. The two reads go through `token_gate` (the tail of `control_gate`): with a token they need it; without one they return `403 TOKEN_REQUIRED_FOR_CONTROL` unless `allow_unauthenticated_control` is set (§9.6 "always require the token").
2. **Reload is all or nothing.** `load_config_file` parses and validates the whole file; any error returns `400 INVALID_CONFIG` (`NO_CONFIG_FILE` if there is no file) and changes nothing.
3. **Live fields:** `model_mapping`, `server.keep_alive_secs`, `native_agy.binary`, `logging.max_size_bytes` (gateway log rotation), `dashboard.auth_token`, `dashboard.allow_unauthenticated_control`. **Restart-only:** `server.listen`, `server.port`, `logging.backup_count` and `logging.audit_max_size_bytes` (the audit writer takes them at startup). Restart-only fields keep their startup values in the active config, so `GET /config` shows what is running. The PID `config_hash` is updated only when no restart-only field differs from startup (§4.3.2).
4. **Diagnostics checks:** `gateway`, `state_db`, `config` (on-disk hash vs applied, pending restart fields), `native_agy`, `upstream` (DNS + TCP + TLS, shared with `doctor` through `diagnostics::probe_upstream`, wait bounded to 10 s). No proxy URLs are shown, so nothing needs redacting.
5. **Dropped from Phase 3:** `SIGHUP` reload, `config_strategy_differs` (state.db stays the source of truth for the strategy), the `pool_changed` SSE event, and the log viewer.
6. **UI:** a non-modal right-hand drawer (a modal backdrop would hide the toast) with the checks, the restart notice, the sanitized config as JSON text, and Reload / Run checks again buttons.
