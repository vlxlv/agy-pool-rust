# agy-pool-rust Monitoring Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an embedded, zero-dependency, read-only web monitoring dashboard (`GET /`, `GET /dashboard`) and supporting REST + SSE endpoints (`/api/v1/status`, `/api/v1/accounts`, `/api/v1/forecast`, `/api/v1/events`) to `agy-pool-rust`.

**Architecture:** Intercept exact dashboard/API paths inside Hyper's proxy handler (`src/proxy/mod.rs`) before upstream forwarding. An in-memory `TelemetryHub` decouples live SSE events and sliding-window latency/QPS stats from disk audit logging, while REST DTOs reuse existing `storage`, `quota`, `scheduler`, `forecast`, and `daemon::runtime` logic. The Scheme B high-density frontend is compiled into the binary via `include_str!`.

**Tech Stack:** Rust 2024 (`tokio`, `hyper` 1.12, `http-body-util`, `serde`, `serde_json`, `jiff`), vanilla HTML5 / CSS / ES6 JavaScript.

**Spec:** [`docs/superpowers/specs/2026-11-11-monitoring-dashboard-design.md`](../specs/2026-11-11-monitoring-dashboard-design.md)

## Global Constraints

- Single static binary: frontend assets must be embedded via `include_str!` with zero external runtime files or online CDN dependencies.
- Zero new `Cargo.toml` dependencies; edition 2024, `rust-version = "1.99"`.
- Read-only monitoring scope: no write/mutation endpoints, no login/cookie auth layer (protected by Tailscale network boundary), no SQLite/JSON persistence schema changes.
- Binary size increase < 100 KB; dashboard memory overhead < 2 MB; proxy hot path must remain low-overhead (~10k+ QPS) with no blocking I/O or lock contention.
- Exact route matching for `/`, `/dashboard`, `/api/v1/*` with zero interference on proxy paths (`/v1*`, `/v1beta*`, `/v1internal*`, `/metrics`, `/healthz`).

## Review Focus

1. **Non-GET methods or query strings on dashboard/API paths**: Sending `POST /api/v1/status` must return `405 Method Not Allowed` (`Allow: GET`) instead of leaking upstream to Google; `GET /api/v1/accounts?show_email=true` must match `/api/v1/accounts` cleanly (pinned in Task 3).
2. **Uninitialized quotas and expired reset timestamps**: Accounts with `None` quota windows must serialize `five_hour`/`weekly` as `null` without panics or `NaN%`, and expired reset timestamps must clamp `reset_in_secs` to `0` without unsigned underflow (pinned in Task 2).
3. **Slow or disconnected SSE subscribers**: A slow browser tab triggering `RecvError::Lagged` must skip missed frames without breaking the stream or blocking proxy attempts, and client disconnect must exit the background sender task cleanly without leaking resources (pinned in Task 1 & Task 3).
4. **Empty pool, zero requests, and sustainable idle forecast**: Zero accounts or zero recorded requests must produce valid `200 OK` JSON (`qps: 0.0`, zeroed latencies, `is_sustainable: true`, `runway_hours: null`, never `f64::NAN` which breaks `serde_json`) (pinned in Task 1 & Task 2).
5. **Email masking edge cases**: Empty emails, single-char local parts (`a@x.com`), missing `@`, or multi-byte UTF-8 emails must mask safely without slice/char-boundary panics (pinned in Task 2).

---

## File Structure

- **Create** `src/dashboard/telemetry.rs` — Lock-light in-memory 100-sample latency/QPS window, 50-item audit replay ring buffer, and `tokio::sync::broadcast` hub.
- **Create** `src/dashboard/dto.rs` — JSON response structs, runtime server metadata store, safe email masking, dynamic scheduler state classification, and builders for status/accounts/forecast.
- **Create** `src/dashboard/mod.rs` — Route matcher (`try_handle_request`), HTTP/JSON handlers, and `SseBody` Hyper streaming response with 20-event replay and 2s stats tick.
- **Create** `src/dashboard/assets/index.html` — Single-file Scheme B SPA (header with `show_email` toggle, 4 metric cards, account table with local 1s countdowns, 20-line live audit log, SSE client with 3s auto-reconnect).
- **Create** `tests/dashboard.rs` — End-to-end integration tests for HTTP routes, `405` handling, `?show_email=true`, SSE replay + live push + disconnect, and proxy passthrough isolation.
- **Modify** `src/lib.rs` — Export `pub mod dashboard;`.
- **Modify** `src/proxy/dispatch.rs:80-120` — Emit lightweight attempt telemetry to `dashboard::telemetry` independently of `is_audit_worker_active()`.
- **Modify** `src/proxy/body.rs:103-128` — Emit streaming attempt telemetry in `StreamBody::emit_audit` independently of `is_audit_worker_active()`.
- **Modify** `src/proxy/mod.rs:866-885` — Dispatch dashboard/API routes via `crate::dashboard::try_handle_request(&req)` before proxy forwarding.
- **Modify** `src/daemon/server.rs:245-260` — Register bound listen address and start timestamp via `crate::dashboard::set_runtime_meta`.

---

### Task 1: In-Memory Telemetry Ring Buffer & SSE Broadcast Hub

**Files:**
- Create: `src/dashboard/telemetry.rs`
- Modify: `src/lib.rs:7-24`
- Modify: `src/dashboard/mod.rs` (minimal stub `pub mod telemetry;` so crate compiles)
- Modify: `src/proxy/dispatch.rs:80-120`
- Modify: `src/proxy/body.rs:103-128`

**Interfaces:**
- Consumes: `crate::observability::AuditEvent`, `crate::observability::record_audit_event`, `crate::observability::is_audit_worker_active`
- Produces:
  - `pub struct DashboardAuditEvent` (`timestamp: String`, `model: String`, `account: String`, `ttfb_ms: u64`, `total_ms: u64`, `tokens: i64`, `status: u16`, `failover: bool`)
  - `pub struct LatencyStatsDto` (`sample_count: usize`, `ttfb_p50_ms: u64`, `ttfb_p95_ms: u64`, `total_p50_ms: u64`, `total_p95_ms: u64`)
  - `pub fn record_attempt(ev: DashboardAuditEvent)`
  - `pub fn latency_and_qps_snapshot(now: std::time::Instant) -> (LatencyStatsDto, f64)`
  - `pub fn recent_events(limit: usize) -> Vec<DashboardAuditEvent>`
  - `pub fn subscribe_events() -> tokio::sync::broadcast::Receiver<DashboardAuditEvent>`
  - `pub fn reset_for_test()`

- [ ] **Step 1: Write the failing unit tests in `src/dashboard/telemetry.rs`**

Create `src/dashboard/telemetry.rs` with `#[cfg(test)] mod tests` testing:
1. `test_empty_telemetry_returns_zeros`: `reset_for_test()`, call `latency_and_qps_snapshot(Instant::now())`, assert `sample_count == 0`, all P50/P95 fields `== 0`, and `qps == 0.0` (never NaN).
2. `test_latency_percentiles_and_100_cap`: push 150 `DashboardAuditEvent` items with `ttfb_ms = i` (`1..=150`) and `total_ms = i * 10`; assert `sample_count == 100` (oldest 50 evicted), `ttfb_p50_ms` is in `100..=101`, `ttfb_p95_ms >= 145`, and `recent_events(20).len() == 20` with newest ordering preserved.
3. `test_broadcast_lag_and_replay`: subscribe via `subscribe_events()`, push 150 events (exceeding broadcast capacity `128`), verify `recent_events(50).len() == 50`, and verify subscriber handles `Err(RecvError::Lagged(_))` without blocking `record_attempt`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::telemetry`
Expected: FAIL to compile because `dashboard` module and `telemetry` functions are not yet wired/implemented.

- [ ] **Step 3: Implement `TelemetryHub` in `src/dashboard/telemetry.rs` and export `pub mod dashboard` in `src/lib.rs`**

- Create `src/dashboard/mod.rs` exporting `pub mod telemetry;`.
- Add `pub mod dashboard;` in `src/lib.rs`.
- In `src/dashboard/telemetry.rs`:
  - Store state in a single `LazyLock<Mutex<TelemetryState>>` (holding `VecDeque<LatencySample>` capacity 100 and `VecDeque<DashboardAuditEvent>` capacity 50) plus `LazyLock<broadcast::Sender<DashboardAuditEvent>>` with channel capacity `128`.
  - `record_attempt(ev)`: computes `Instant::now()`, locks `Mutex`, pushes `(now, ev.ttfb_ms, ev.total_ms)` (pop front if `> 100`), pushes `ev.clone()` to replay buffer (pop front if `> 50`), drops `Mutex`, then calls `let _ = tx.send(ev);` outside the mutex.
  - `latency_and_qps_snapshot(now)`: locks `Mutex`, copies up to 100 `ttfb_ms` and `total_ms` into local `Vec<u64>`, counts samples with `now.duration_since(ts) <= Duration::from_secs(60)`, drops `Mutex`. Computes P50/P95 via `sort_unstable` and nearest-rank index; computes `qps = (recent_count as f64 / 60.0 * 100.0).round() / 100.0`.
  - `tokens` conversion rule for proxy hooks: use `tokens.prompt.max(0)` (since `UsageTap` initializes `cached` to `-1`).

- [ ] **Step 4: Hook proxy completion paths in `src/proxy/dispatch.rs:80-120` and `src/proxy/body.rs:103-128`**

- In `src/proxy/dispatch.rs::write_attempt_audit`: unconditionally call `crate::dashboard::telemetry::record_attempt(DashboardAuditEvent { ... })` using `params.model`, `display_account_name`, `ttfb`, `total`, `tokens.prompt.max(0)`, `status_code`, and `failover: !reason.is_empty()` regardless of `is_audit_worker_active()`. Keep the `if is_audit_worker_active()` guard around `record_audit_event(...)` for disk audit logs.
- In `src/proxy/body.rs::StreamBody::emit_audit`: unconditionally call `crate::dashboard::telemetry::record_attempt(...)` regardless of `is_audit_worker_active()`, while keeping disk `record_audit_event(...)` gated on `is_audit_worker_active()`.

- [ ] **Step 5: Run tests and verify pass**

Run: `cargo test --lib dashboard::telemetry && cargo test --lib proxy::tests::observability`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/dashboard/mod.rs src/dashboard/telemetry.rs src/proxy/dispatch.rs src/proxy/body.rs
git commit -m "feat(dashboard): add in-memory telemetry ring buffer and SSE broadcast hub"
```

---

### Task 2: Dashboard DTOs & Data Builders

**Files:**
- Create: `src/dashboard/dto.rs`
- Modify: `src/dashboard/mod.rs` (export `pub mod dto;`)

**Interfaces:**
- Consumes: `crate::dashboard::telemetry::{LatencyStatsDto, latency_and_qps_snapshot}`, `crate::storage::{Account, Pool, load_pool, STATUS_DISABLED, STATUS_VALIDATION_REQUIRED, STATUS_VERIFICATION_PENDING, STATUS_AUTH_ERROR}`, `crate::quota::{compute_capacity_state, display_quota_fractions}`, `crate::scheduler::{is_restricted, is_reserve_capacity, over_weekly_pace_capacity}`, `crate::forecast::{simulate_quota_runway, format_duration_short}`, `crate::daemon::runtime::collect_runtime_stats`, `crate::observability::{all_in_flight_generations, snapshot}`, `crate::proxy::active_model_pauses`, `crate::accounts::display_account_name`
- Produces:
  - `pub fn set_runtime_meta(listen: String, started_at: jiff::Timestamp)`
  - `pub fn mask_email(email: &str) -> String`
  - `pub fn build_status_response() -> StatusResponse`
  - `pub fn build_accounts_response(pool: &Pool, show_email: bool, now_ts: f64) -> AccountsResponse`
  - `pub fn build_forecast_response(pool: &Pool, now_ts: f64) -> ForecastResponse`
  - `pub fn build_sse_stats_event() -> SseStatsEvent`

- [ ] **Step 1: Write the failing unit tests in `src/dashboard/dto.rs`**

Add `#[cfg(test)] mod tests` covering:
1. `test_mask_email_edge_cases`: `"" -> ""`, `"user@gmail.com" -> "u***r@gmail.com"`, `"a@x.com" -> "a***@x.com"`, `"ab@x.com" -> "a***@x.com"`, `"invalid-email" -> "i***"`, `"äöü@example.com" -> "ä***ü@example.com"` (no UTF-8 slice panic).
2. `test_accounts_dto_null_quotas_and_show_email_toggle`: build a `Pool` with one uninitialized account (`last_quota: None`, `email: "alice@example.com"`), one cooling account (`rate_limited_until: Some(now + 60.0)`), one reserve account (`5h = 0.08`), one disabled account (`status: STATUS_DISABLED`), and one active account with expired `reset5_time` (`now - 10.0`).
   - With `show_email = false`: serialize to `serde_json::Value` and assert `email` key is omitted (`acc.get("email").is_none()`), `email_masked == "a***e@example.com"`.
   - With `show_email = true`: assert `email == "alice@example.com"`.
   - Assert uninitialized account has `"five_hour": null` and `"weekly": null`.
   - Assert expired reset time yields `reset_in_secs == 0` (no underflow).
   - Assert `scheduler_state` values are `"active"`, `"reserve"`, `"cooling"`, `"disabled"`.
3. `test_forecast_dto_empty_and_sustainable`:
   - Empty `Pool::default()` returns `is_sustainable: true`, `runway_hours: None`, `runway_text: "Sustainable"`, `burn_rate_requests_per_hour: 0.0`, valid JSON (`serde_json::to_string` succeeds without NaN errors).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::dto`
Expected: FAIL because `dto` module does not exist yet.

- [ ] **Step 3: Implement `src/dashboard/dto.rs`**

- Add `pub mod dto;` to `src/dashboard/mod.rs`.
- Implement `RuntimeMeta` stored in `LazyLock<RwLock<Option<(String, jiff::Timestamp)>>>` with `set_runtime_meta(listen, started_at)` and fallback `(String::from("127.0.0.1:8899"), jiff::Timestamp::now())`.
- Implement `mask_email(email: &str) -> String` using `.chars()` boundaries (never byte slicing).
- Define serializable DTO structs matching Spec Section 4.2:
  - `StatusResponse` (`version`, `status: "running"`, `listen`, `strategy`, `uptime_secs`, `memory_rss_bytes`, `in_flight_requests`, `total_requests`, `qps`, `latency: LatencyStatsDto`)
  - `QuotaWindowDto` (`fraction`, `remaining_percent`, `reset_at_unix`, `reset_in_secs`)
  - `AccountDto` (`id`, `name`, `email_masked`, `#[serde(skip_serializing_if = "Option::is_none")] email: Option<String>`, `storage_status`, `scheduler_state`, `five_hour: Option<QuotaWindowDto>`, `weekly: Option<QuotaWindowDto>`, `hits: i64`, `in_flight: i64`)
  - `AccountsResponse` (`accounts`, `total_accounts`, `active_accounts`)
  - `ForecastResponse` (`runway_hours: Option<f64>`, `runway_text: String`, `is_sustainable: bool`, `burn_rate_requests_per_hour: f64`, `window_used: String`)
  - `SseStatsEvent` (`in_flight: i64`, `qps: f64`, `rss_mb: f64`)
- Derive `scheduler_state`: `"disabled"` if `acc.status == STATUS_DISABLED`, `"restricted"` / `"cooling"` if `is_restricted(acc)` or `acc.rate_limited_until.is_some_and(|r| r > now)` or model pause active, `"depleted"` if `cap.is_depleted`, `"reserve"` if `is_reserve_capacity(&cap) || over_weekly_pace_capacity(&cap)`, else `"active"`.
- In `build_forecast_response(pool, now_ts)`: call `simulate_quota_runway(pool, None, Some(now_ts))`. When `None` or `current_runway.first_stall_sec` is `None`, set `is_sustainable = true`, `runway_hours = None`, `runway_text = "Sustainable".to_string()`. Otherwise round `secs / 3600.0` to 1 decimal and format via `format_duration_short`.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --lib dashboard::dto`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/dashboard/mod.rs src/dashboard/dto.rs
git commit -m "feat(dashboard): add status, accounts, and forecast DTO builders"
```

---

### Task 3: HTTP Router, REST Endpoints & SSE Stream

**Files:**
- Modify: `src/dashboard/mod.rs`
- Modify: `src/proxy/mod.rs:866-886`
- Modify: `src/daemon/server.rs:245-260`
- Create: `tests/dashboard.rs`

**Interfaces:**
- Consumes: `crate::dashboard::dto::*`, `crate::dashboard::telemetry::*`, `crate::proxy::body::{ProxyBody, full_body}`, `crate::storage::load_pool`
- Produces:
  - `pub fn try_handle_request(req: &hyper::Request<hyper::body::Incoming>) -> Option<hyper::Response<ProxyBody>>`
  - `pub fn dashboard_html() -> &'static str` (temporarily placeholder `"<!doctype html><html><body>agy-pool dashboard</body></html>"` until Task 4 creates `assets/index.html`)

- [ ] **Step 1: Write the failing integration tests in `tests/dashboard.rs`**

Create `tests/dashboard.rs` using isolated temp dirs (`TempDir` + `configure_state_dir`, protected by `crate::config::TEST_LOCK` / async mutex similar to `src/proxy/tests/mod.rs`) and real Hyper HTTP/1 localhost connections:
1. `test_dashboard_html_and_rest_routes`:
   - `GET /` and `GET /dashboard` and `GET /dashboard/` return `200 OK` with `text/html; charset=utf-8`.
   - `GET /api/v1/status` returns `200 OK` JSON containing `version`, `strategy`, `latency.ttfb_p50_ms`, `qps`.
   - `GET /api/v1/accounts` returns `200 OK` JSON with `email_masked` and omits `email`; `GET /api/v1/accounts?show_email=true` includes full `email`.
   - `GET /api/v1/forecast` returns `200 OK` JSON with `is_sustainable`, `runway_text`.
2. `test_dashboard_non_get_returns_405`:
   - `POST /api/v1/status` and `POST /dashboard` return `405 Method Not Allowed` with `Allow: GET` header instead of proxying upstream.
3. `test_proxy_routes_remain_unaffected`:
   - `GET /healthz` returns `200 OK` `{"status":"ok"}`; `GET /metrics` returns `200 OK` Prometheus text; `POST /v1/models/gemini:generateContent` passes through proxy dispatch (returns `503`/upstream error on empty pool, NOT intercepted by dashboard).
4. `test_sse_events_replay_and_live_stream`:
   - Call `dashboard::telemetry::record_attempt(...)` before connecting.
   - Open `GET /api/v1/events` via Hyper HTTP/1 client, verify `200 OK` and `Content-Type: text/event-stream; charset=utf-8`.
   - Read initial frames (`body.frame().await` with timeout) and assert the pre-recorded event is replayed as `event: audit\ndata: {...}\n\n`.
   - Call `dashboard::telemetry::record_attempt(...)` again while stream is open, assert the new live event arrives over the same SSE stream.
   - Drop client stream/sender and verify clean teardown without panic.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test dashboard`
Expected: FAIL because `try_handle_request` does not exist yet.

- [ ] **Step 3: Implement `try_handle_request` and `SseBody` in `src/dashboard/mod.rs`, and integrate into `src/proxy/mod.rs` & `src/daemon/server.rs`**

- In `src/dashboard/mod.rs`:
  - Match exact path `req.uri().path()` against `"/"`, `"/dashboard"`, `"/dashboard/"`, `"/api/v1/status"`, `"/api/v1/status/"`, `"/api/v1/accounts"`, `"/api/v1/accounts/"`, `"/api/v1/forecast"`, `"/api/v1/forecast/"`, `"/api/v1/events"`, `"/api/v1/events/"`. Return `None` for anything else.
  - If matched and `req.method() != Method::GET`, return `Some(405 Method Not Allowed)` with `Allow: GET`.
  - Parse `?show_email=true` query parameter via `url::form_urlencoded::parse` (from existing `url` dependency).
  - For JSON endpoints, serialize DTOs with `serde_json::to_vec`, set `Content-Type: application/json`, `Cache-Control: no-store`.
  - For `/api/v1/events`: create `mpsc::channel::<Bytes>(64)`, spawn task that sends up to 20 replay events from `telemetry::recent_events(20)`, then `tokio::select!` across `rx.recv()`, `ticker.tick()` (2s stats event), and `tx.closed()` (disconnect cleanup). Wrap `mpsc::Receiver<Bytes>` in `SseBody` implementing `hyper::body::Body<Data = Bytes, Error = std::io::Error>`, return `200 OK` with `Content-Type: text/event-stream; charset=utf-8`, `Cache-Control: no-cache`.
- In `src/proxy/mod.rs:867`: call `if let Some(resp) = crate::dashboard::try_handle_request(&req) { return resp; }` at the top of `handle_proxy`.
- In `src/daemon/server.rs:247`: call `crate::dashboard::dto::set_runtime_meta(format!("{}:{}", ctx_inst.listen_host, actual_port), start_time);`.

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test --test dashboard && cargo test --lib proxy::tests`
Expected: PASS (all new dashboard tests and all existing proxy tests pass).

- [ ] **Step 5: Commit**

```bash
git add src/dashboard/mod.rs src/proxy/mod.rs src/daemon/server.rs tests/dashboard.rs
git commit -m "feat(dashboard): wire HTTP routes, REST endpoints, and SSE stream"
```

---

### Task 4: Single-File Embedded SPA Frontend (Scheme B)

**Files:**
- Create: `src/dashboard/assets/index.html`
- Modify: `src/dashboard/mod.rs` (switch `dashboard_html()` to `include_str!("assets/index.html")`)
- Modify: `tests/dashboard.rs` (add HTML asset verification test)

**Interfaces:**
- Consumes: `GET /api/v1/status`, `GET /api/v1/accounts?show_email=...`, `GET /api/v1/forecast`, `GET /api/v1/events` (SSE `audit` & `stats` events)
- Produces: Static self-contained HTML/CSS/JS served at `GET /` and `GET /dashboard`

- [ ] **Step 1: Write the failing test for the embedded SPA asset in `tests/dashboard.rs`**

Add `test_embedded_spa_asset_constraints` to `tests/dashboard.rs`:
- Fetch `GET /dashboard` over Hyper.
- Assert body length `> 2048` and `< 90_000` bytes.
- Assert body contains no external CDN URLs (`!body.contains("https://")` and `!body.contains("http://")` in `<script` or `<link` tags).
- Assert body references all four backend endpoints: `"/api/v1/status"`, `"/api/v1/accounts"`, `"/api/v1/forecast"`, `"/api/v1/events"`.
- Assert body includes the email visibility toggle control (`show_email`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test dashboard test_embedded_spa_asset_constraints`
Expected: FAIL because `dashboard_html()` still returns the short stub.

- [ ] **Step 3: Build `src/dashboard/assets/index.html` and embed via `include_str!`**

Design `src/dashboard/assets/index.html` cleanly following Scheme B (high-density dark slate theme, zero external assets):
- **Header Bar**: `agy-pool` badge + live running status dot + listen address & strategy pill + SSE connection indicator (Connected / Reconnecting...) + **"Show Email" toggle checkbox** (default unchecked; persists choice or triggers immediate `/api/v1/accounts` fetch with `?show_email=true/false`).
- **Top Metrics Row (4 Cards)**:
  1. `5H Pool Remaining` (weighted/average 5h capacity % + active/total accounts count)
  2. `Concurrency & QPS` (`in_flight` live + `qps` req/s)
  3. `Latency (TTFB / Total)` (`ttfb_p50_ms` primary + `total_p50_ms` / P95 subtitle)
  4. `Quota Runway` (`runway_text` or `"Sustainable"` + `burn_rate_requests_per_hour` req/h)
- **Account Quota Table**:
  - Columns: Account (`name` + `email` or `email_masked`), Status badge (color-coded `active`/`reserve`/`cooling`/`disabled`), 5H Quota (progress bar + `%`), 5H Reset (client-side 1s live countdown from `reset_at_unix`, showing `Ready` at 0), Weekly Quota (progress bar + `%` + countdown), Hits / In-Flight.
  - Safely handles `null` quota windows (`—` / `Uninitialized`).
- **Live Request Stream (Bottom Panel)**:
  - Latest 20 audit events prepended newest-first: Time, Model, Account, TTFB / Total (`ms`), Tokens, Status pill (`200` green, `429` yellow, error red, failover badge if `failover: true`).
  - Pause/Resume stream button + Clear button.
- **Data Refresh Loop**:
  - Initial `Promise.all` fetch of `/api/v1/status`, `/api/v1/accounts`, `/api/v1/forecast`.
  - Persistent `EventSource('/api/v1/events')` updating live audit rows and `in_flight`/`qps`/`rss` counters; `onerror` shows yellow reconnect banner and reconnects automatically every 3s.
  - Background `setInterval` every 5s refreshing `/api/v1/accounts`, `/api/v1/status`, `/api/v1/forecast`, plus 1s local tick updating `reset_in_secs` countdown strings without full table DOM flicker.
- Update `dashboard_html()` in `src/dashboard/mod.rs` to return `include_str!("assets/index.html")`.

- [ ] **Step 4: Run tests and release size/build verification**

Run: `cargo test && cargo build --release`
Expected: All unit + integration tests PASS; `cargo build --release` succeeds cleanly. Also inspect `ls -lh target/release/agy-pool` to verify minimal binary growth (< 100 KB over baseline ~7.88 MiB).

- [ ] **Step 5: Commit**

```bash
git add src/dashboard/assets/index.html src/dashboard/mod.rs tests/dashboard.rs
git commit -m "feat(dashboard): add embedded Scheme B SPA monitoring frontend"
```

---

## Self-Review Results

- **Spec coverage**:
  - Embedded single-binary SPA (`GET /`, `/dashboard`) -> Task 3 & Task 4.
  - REST APIs (`/api/v1/status`, `/api/v1/accounts?show_email=true`, `/api/v1/forecast`) -> Task 2 & Task 3.
  - Decoupled SSE stream (`/api/v1/events` with 20-event replay + 2s stats heartbeat) -> Task 1 & Task 3.
  - Scheme B high-density UI (metrics cards, quota table, 1s local countdowns, live audit stream, email toggle) -> Task 4.
- **Type consistency**: `DashboardAuditEvent`, `LatencyStatsDto`, `StatusResponse`, `AccountsResponse`, `ForecastResponse`, `SseStatsEvent`, `set_runtime_meta`, `try_handle_request` match across all tasks.
- **Review Focus coverage**: All 5 uncovered failure modes (`405` on non-GET, `null` quota + reset clamping, SSE lag/disconnect, empty/sustainable forecast without NaN, UTF-8/edge-case email masking) have explicit tests pinned to Tasks 1, 2, and 3.
