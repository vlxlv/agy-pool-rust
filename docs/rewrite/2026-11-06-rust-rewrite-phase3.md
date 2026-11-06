# agy-pool Rust Rewrite — Phase 3 (Gateway) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Execution in this repo:** each task is delegated to agy (agy-worker `agy_run`) with the task text as instructions and "the listed tests pass" as acceptance; Claude reviews the diff and re-runs the tests before the next task.

**Goal:** Port the gateway — `proxy`, `daemon`, `diagnostics` — so the Rust binary can serve agy traffic with the same routing, failover, streaming and no-replay behaviour as Go, and prove it with a Go↔Rust differential test.

**Architecture:** One upstream connector (`rust/src/upstream.rs`) does DNS → TCP → optional `HTTP(S)_PROXY` CONNECT tunnel → TLS (rustls, native roots) as explicit steps. Pooled metadata calls (auth, quota, non-generation proxying) use it through a hyper-util client; generation requests use it directly, one fresh connection per attempt, with a hyper `http1::handshake` — so "connected" (Go's `httptrace.GotConn`) is a value the dispatcher sees, not a callback. The downstream server is hyper `http1` on a tokio `TcpListener`. The handler is `async fn(Request<Incoming>) -> Response<ProxyBody>`: an attempt returns `Attempt::Failover(reason)` (nothing sent) or `Attempt::Done(Response)`; a response is returned at most once, so Go's `ResponseCommitter` invariant becomes structural. Everything that Go releases with `defer` (in-flight count, lease, confirmation claim, affinity spill marks) is an RAII guard, because a client disconnect drops the handler future.

**Tech Stack:** Phase 1–2 stack + tokio-rustls, rustls-native-certs, tokio-socks (SOCKS5 client incl. username/password), hyper `server` + `http1` features, `tokio::signal`, `libc` (setsid, kill, flock); dev: rcgen (self-signed cert for the https-proxy test).

**Spec:** `docs/superpowers/plans/2026-11-05-rust-rewrite.md` (master; Global Constraints and Review Focus #4–#5 apply), `docs/superpowers/plans/2026-11-06-rust-rewrite-phase2.md` (Global Constraints apply), and the Go source and tests of `internal/proxy`, `internal/daemon`, `internal/diagnostics`, plus `cmd/agy-pool/main.go`, `internal/cli/daemon_cmd.go` (`runDaemonChild`) and `internal/cli/eval.go` (`proxy-test-server`).

## Global Constraints

All Global Constraints of the master and Phase 2 plans apply — in particular: run every Rust command on this host through `scripts/rust-test.sh`; never bare `cargo`. In addition:

- **No-replay is the top invariant.** A generation request may move to another account only if (a) the upstream connection was never established (DNS, TCP connect, proxy CONNECT, or TLS failed), or (b) upstream answered with one of Go's explicit retryable responses (429 quota, 503/capacity, auth failure, location error, validation-required — exactly the `ActionFailoverNext` cases in `internal/proxy/proxy.go`). Every other outcome is terminal. Default is terminal: a new branch must opt in to failover.
- Generation attempts never reuse a connection (Go `Close=true` / `DisableKeepAlives`); the request body is a fully buffered `Bytes`, sent once; no hyper or tower retry layer anywhere in the crate.
- `Idempotency-Key` / `X-Idempotency-Key` stripped from generation requests; upstream `Accept-Encoding: identity`; header filtering exactly as `FilterRequestHeaders`/`FilterResponseHeaders`/`HopByHopNames`. Header-name case may differ from Go (HTTP/1 header names are case-insensitive) — that is the only accepted wire difference.
- Proxy env: honour `HTTPS_PROXY`/`https_proxy`, `HTTP_PROXY`/`http_proxy`, `NO_PROXY`/`no_proxy` with Go's `ProxyFromEnvironment` rules (loopback/`localhost` targets never proxied; empty scheme means `http`). Proxy URL schemes exactly as Go's `net/http` (`transport.go` in `~/sdk/go1.26.10/src/net/http/`): `http://` (CONNECT for https targets, absolute-form for http targets), `https://` (same, over TLS to the proxy), `socks5://` and `socks5h://` (identical: the target **hostname** is sent to the proxy, which resolves it; default port 1080). Proxy URL userinfo → `Proxy-Authorization: Basic …` for http(s) proxies, RFC 1929 username/password for SOCKS5 (offer no-auth + user/pass when userinfo is present, no-auth only otherwise). Any other scheme fails with a clear error naming the scheme. Every failure while reaching or negotiating with the proxy is a `ConnectError` (failover-safe), since no request byte has reached upstream.
- Downstream: `/metrics` and `/healthz` as Go; streaming responses set `Transfer-Encoding: chunked` and `Connection: close` like `SendStream`; each upstream chunk is forwarded as soon as it arrives (no buffering, no re-compression).
- Timeouts as Go: connect 30 s, TLS handshake 10 s, response headers 300 s, overall client timeout 300 s.
- Every Go test is either ported (snake_case name, same assertions) or listed in `rust/PORTING_NOTES.md` with the reason it does not apply to Rust (e.g. it tests `http.Hijacker` delegation) and the Rust test that covers the same behaviour. Reviewer approves each entry.
- Daemon child processes and `proxy-test-server` runs in tests use temp data dirs and `AGY_TEST_MODE=1`; they never bind port 8899 (use port 0 / an ephemeral port) and never touch the live PID file.

## Review Focus

1. **Upstream connection opened, then the request fails** (reset after send, EOF before headers, malformed response, TLS failure after connect, timeout) — expected: one upstream attempt, 502/504 to the client, `no_replay_prevented_total` +1, no second account contacted. Pinned in Task 13 by the ported `transport_failure_test.go` matrix plus the Task 16 differential test.
2. **Client disconnects while the gateway waits for upstream headers or mid-stream** — expected: the upstream request is aborted, no other account is tried, and in-flight count, lease row and confirmation claim are all released. Pinned in Task 13 (`client_disconnect_releases_everything`).
3. **A slow SSE stream** — expected: the client receives the first event before upstream sends the second. Pinned in Task 13 (`sse_first_chunk_not_buffered`).
4. **A second gateway started while one runs** (same data dir or same port) — expected: refused with Go's message, the running one unaffected. Pinned in Task 14 by the ported PID-lock and multi-instance tests plus `second_rust_gateway_refused_while_go_runs` (Go child holds the lock, Rust must refuse).
5. **`HTTPS_PROXY` set to an http, https or socks5 proxy, with or without credentials** — expected: generation and metadata requests go through it exactly as Go sends them (CONNECT / absolute-form / SOCKS5 CONNECT with the hostname), credentials are presented the way Go presents them, and `NO_PROXY` hosts and loopback targets bypass it. Pinned in Task 11 (`connect_tunnel_through_http_proxy`, `http_proxy_basic_auth`, `https_proxy_tls_to_proxy`, `socks5_remote_dns_and_auth`, `socks5_proxy_refusal_is_connect_error`, `loopback_never_proxied`, `no_proxy_bypasses_tunnel`).

---

## File Structure

- `rust/src/upstream.rs` — connector (DNS/TCP/CONNECT/TLS), `ConnectError` vs `SendError` split, pooled metadata client, `send_generation_once`.
- `rust/src/http.rs` — modified: Phase 2 helpers now use `upstream`'s pooled client (proxy env + same TLS config).
- `rust/src/proxy/mod.rs` — module root, `Handler`, `serve_request`, routing events/audit (`proxy.go`).
- `rust/src/proxy/{affinity,model_cooldown,smooth_rate,confirmation,failover,usage}.rs` — ports of the same-named Go files.
- `rust/src/proxy/dispatch.rs` — per-account attempt (`dispatchAccount`) returning `Attempt`.
- `rust/src/proxy/body.rs` — `ProxyBody` (buffered or streaming), stream wrapper that taps usage, enforces Content-Length, logs end-of-stream lines (`stream.go`, `usage.go`, commit semantics of `commitment.go`).
- `rust/src/daemon/{mod,control,instance,lifecycle,log,pid,runtime,server,systemd}.rs` — ports of `internal/daemon/*.go`.
- `rust/src/diagnostics/{mod,effectiveness,routing_audit,routing_audit_eval}.rs` — ports of `internal/diagnostics/*.go`.
- `rust/src/main.rs` — modified: daemon-child mode (`AGY_POOL_DAEMON_CHILD=1` → `daemon::run_daemon_child`) and `proxy-test-server <port> <state-dir> <backend-url>`, mirroring `cmd/agy-pool/main.go` and `cli/eval.go`.
- `rust/PORTING_NOTES.md` — non-applicable Go tests and known gaps.
- `rust/tests/differential.rs` — Go↔Rust gateway differential test.

### Task 11: Upstream connector with an explicit connect phase

**Files:**
- Create: `rust/src/upstream.rs`, `rust/PORTING_NOTES.md`
- Modify: `rust/src/http.rs`, `rust/src/lib.rs`, `rust/Cargo.toml`
- Spec: `internal/proxy/transport.go`, `internal/proxy/transport_test.go` (8 tests); Go `net/http` `ProxyFromEnvironment` semantics

**Interfaces:**
- Produces:

```rust
pub enum ConnectError { Dns(String), Tcp(String), Proxy(String), Tls(String), Timeout(&'static str) }
pub enum SendError { Io(String), Timeout, Malformed(String) }   // only after the connection exists
pub struct Connected { /* hyper http1 SendRequest + conn task */ }

/// Opens one fresh connection to `uri`'s authority (through the env proxy if any).
/// Err means no request byte can have reached upstream: failover is safe.
pub async fn connect(uri: &http::Uri) -> Result<Connected, ConnectError>;

impl Connected {
    /// Sends the request exactly once on this connection and returns the response head with a
    /// streaming body. Any Err here is ambiguous: the caller must treat it as terminal.
    pub async fn send(self, req: http::Request<http_body_util::Full<bytes::Bytes>>)
        -> Result<http::Response<hyper::body::Incoming>, SendError>;
}

pub fn pooled_client() -> &'static hyper_util::client::legacy::Client<ProxyConnector, http_body_util::Full<bytes::Bytes>>;
pub fn proxy_for(uri: &http::Uri) -> Option<http::Uri>;   // Go ProxyFromEnvironment rules
pub fn is_timeout(e: &SendError) -> bool;                 // Go IsTimeoutError
```

- Consumes: nothing new; `http.rs` switches its client to `pooled_client()`.

- [ ] **Step 1: Write the failing tests** (in `upstream.rs`, `#[tokio::test]`, local listeners only):
  - `connect_refused_is_connect_error`: connect to a closed localhost port → `Err(ConnectError::Tcp(_))`.
  - `connect_ok_then_reset_is_send_error`: a `TcpListener` that accepts and immediately drops the socket → `connect` is `Ok`, `send` is `Err(SendError::Io(_))`.
  - `tls_handshake_failure_is_connect_error`: an `https://` URI to a plain-TCP listener that writes garbage → `Err(ConnectError::Tls(_))`.
  - `connect_tunnel_through_http_proxy`: Go's `ProxyFromEnvironment` never proxies `localhost` or loopback targets, so the target is the fake host `upstream.test:<port>`, which only the fake proxy resolves (it maps `upstream.test` → `127.0.0.1`). The fake proxy listener expects `CONNECT upstream.test:<port> HTTP/1.1`, answers `200`, then splices to a `TestServer`; with `HTTPS_PROXY` pointed at it (env guarded by `TEST_LOCK`), `connect` + `send` of an `https://upstream.test:<port>` request succeeds up to the TLS step (assert the proxy saw exactly one CONNECT for that authority). Also test an `http://upstream.test:<port>/x` target with `HTTP_PROXY`: Go sends it in absolute form (`GET http://upstream.test:<port>/x`) to the proxy, not CONNECT — assert the same.
  - `loopback_never_proxied`: `HTTPS_PROXY`/`HTTP_PROXY` set, target `http://127.0.0.1:<port>` → the request reaches the `TestServer` directly and the proxy sees no connection (Go rule).
  - `no_proxy_bypasses_tunnel`: same proxy, `NO_PROXY=upstream.test` → the proxy sees no connection and `connect` fails with `ConnectError::Dns(_)` (the client now resolves `upstream.test` itself).
  - `http_proxy_basic_auth`: `HTTPS_PROXY=http://user:p%40ss@<proxy>` → the CONNECT carries `Proxy-Authorization: Basic dXNlcjpwQHNz` (userinfo percent-decoded first, as Go does).
  - `https_proxy_tls_to_proxy`: the fake proxy listens with TLS using a self-signed certificate generated in the test (`rcgen`, dev-dependency); a `#[cfg(test)]` hook adds that certificate to the trusted roots; `HTTPS_PROXY=https://localhost:<port>` → the client does TLS to the proxy, then CONNECT inside it.
  - `socks5_remote_dns_and_auth`: a fake SOCKS5 server written in the test (~60 lines: greeting, optional RFC 1929 sub-negotiation, CONNECT, then splice to a `TestServer` it reaches by mapping `upstream.test` → `127.0.0.1`). Cases: (a) `socks5://<proxy>` → greeting offers exactly `[0x00]`; CONNECT uses ATYP `0x03` (domain) with `upstream.test` and the target port; the request then succeeds end to end over plain http. (b) `socks5h://` → identical bytes to (a). (c) `socks5://u:pw@<proxy>` → greeting offers `[0x00, 0x02]`; when the server picks `0x02`, the client sends RFC 1929 `0x01, len, "u", len, "pw"`; success. (d) no port in the URL → the client dials port 1080 (assert by pointing a listener at 1080 only if it is free; otherwise assert the parsed proxy address via `proxy_for`).
  - `socks5_proxy_refusal_is_connect_error`: the fake SOCKS5 server answers CONNECT with REP `0x05` (connection refused) → `Err(ConnectError::Proxy(_))`, and a second fake that rejects auth (`0x01, 0x01`) → `Err(ConnectError::Proxy(_))`.
  - If `tokio-socks` cannot reproduce Go's exact greeting/auth bytes in cases (a)–(c), drop it and hand-write the SOCKS5 client in `upstream.rs` (~80 lines, RFC 1928 CONNECT + RFC 1929 auth); the test bytes are the contract, not the crate.
  - `unknown_proxy_scheme_rejected`: `HTTPS_PROXY=ftp://h:1` → `Err(ConnectError::Proxy(msg))` with `msg` naming `ftp`.
  - Port the 8 `transport_test.go` tests; ones that test Go-only types (`GenerationRoundTripper`, `http.Transport` fields) go to `PORTING_NOTES.md` with the Rust test that covers the behaviour (e.g. `TestGenerationTransport_NoReplayConfiguration` → covered by `connect` being per-attempt and no retry layer existing; add `generation_connection_not_reused`: two `connect` calls produce two accepted sockets on the listener).
- [ ] **Step 2: Run** `scripts/rust-test.sh test --lib upstream::` — expected: compile failure.
- [ ] **Step 3: Implement** `upstream.rs`; make `ProxyConnector` a `tower::Service<Uri>` built from the same `connect` path so pooled and per-request connections share DNS/proxy/TLS code. Switch `http.rs` to `pooled_client()`; Phase 2 tests must stay green.
- [ ] **Step 4: Run** `scripts/rust-test.sh test --lib upstream:: http:: auth:: quota::` — all pass.
- [ ] **Step 5: Gate and commit** — `scripts/rust-test.sh fmt --check && scripts/rust-test.sh clippy --all-targets -- -D warnings && scripts/rust-test.sh test`; commit `feat(rust): upstream connector with explicit connect phase`.

### Task 12: Proxy routing state (affinity, cooldowns, smoothing, confirmation, failover bookkeeping, usage)

**Files:**
- Create: `rust/src/proxy/{mod.rs,affinity.rs,model_cooldown.rs,smooth_rate.rs,confirmation.rs,failover.rs,usage.rs}`
- Spec: the same-named Go files; tests `affinity_test.go` (11), `model_cooldown_test.go` (6), `smooth_rate_test.go` (4), `usage_test.go` (2); the unit-level (non-handler) tests of `failover_test.go` and `verification_pending_test.go` (handler-level ones move to Task 13)

**Interfaces:**
- Consumes: `storage`, `scheduler`, `quota`, `observability`, `clock`, `accounts`.
- Produces: `AffinityManager` (`new(ttl)`, `get`, `set`, `delete`, `apply_affinity(&[Arc<Account>] or indices — same representation as `scheduler::order_candidates`, session_key, now) -> (ordered, matched: bool)`, `held_by_load`, persistence via `storage::save_runtime_value(sessionAffinitiesKey)` with Go's JSON shape), `affinity_manager() -> &'static AffinityManager`, constants `DEFAULT_AFFINITY_TTL`, `MAX_AFFINITY_IN_FLIGHT`, `AFFINITY_MIN_5H_FLOOR`, `AFFINITY_MIN_WEEKLY_FLOOR`; `load_model_cooldowns`, `record_model_capacity_error -> Duration`, `clear_model_cooldown`, `active_model_pauses`, `last_generation_model`, `exclude_model_cooldowns(cands, model, now) -> (cands, excluded: usize)` (persisted under the same runtime key/JSON as Go, `model_pauses`); `reset_smooth_rate`, `model_smooth_interval`, `record_model_dispatch`, `smooth_model_rate`; `try_claim_verification_confirmation`, `release_…`, `clear_all_…`, `is_…_claimed`, `select_and_claim_confirmation_candidate` returning a `ConfirmationClaim` guard that releases on Drop unless `.keep()` is called (Go's explicit release paths become `.keep()`/drop); `record_success`, `record_quota_error(&Account, headers, body) -> Result<i64>`, `record_validation_error`, `record_auth_error`, `is_location_error`, `record_soft_cooldown`; `UsageTap` (parses `usageMetadata` prompt/cached token counts from SSE/JSON bytes as Go's `usageTap`).

- [ ] **Step 1: Port the listed tests.** Add `persisted_affinity_and_pauses_match_go_json`: write affinities and model pauses from Rust, read the runtime values back and compare them with JSON produced by Go for the same input (temporary `internal/proxy/zz_runtime_json_test.go` printing `SaveAffinities`/model pause JSON with `go test -count=1 -v`, deleted after use). Go and Rust gateways will share these runtime keys during the candidate phase.
- [ ] **Step 2: Run** `scripts/rust-test.sh test --lib proxy::` — compile failure.
- [ ] **Step 3: Port the six Go files.**
- [ ] **Step 4: Run** `scripts/rust-test.sh test --lib proxy::` — all pass.
- [ ] **Step 5: Gate and commit** — `feat(rust): port proxy routing state`.

### Task 13: Proxy handler, dispatch, streaming and the no-replay matrix

**Files:**
- Create: `rust/src/proxy/dispatch.rs`, `rust/src/proxy/body.rs`
- Modify: `rust/src/proxy/mod.rs`, `rust/src/main.rs`, `rust/PORTING_NOTES.md`
- Spec: `internal/proxy/{proxy,commitment,stream}.go`; tests `proxy_test.go` (6), `commitment_test.go` (5), `stream_test.go` (2), `transport_failure_test.go` (16), `failover_test.go` (15, handler-level), `inflight_test.go` (4), `observability_test.go` (7), `verification_pending_test.go` (8, handler-level); `internal/cli/eval.go` `proxy-test-server`

**Interfaces:**
- Consumes: Task 11 `upstream::{connect, Connected, ConnectError, SendError, pooled_client, is_timeout}`, Task 12 routing state, `storage::{reserve_in_flight, LeaseGuard}`, `observability::start_in_flight_generation`, `auth`, `scheduler`.
- Produces:

```rust
pub type ProxyBody = http_body_util::combinators::BoxBody<bytes::Bytes, std::io::Error>;
pub enum Attempt { Failover { reason: &'static str, err: Option<String> }, Done(http::Response<ProxyBody>) }
pub struct Handler;                       // Go Handler
impl Handler {
    pub async fn serve(&self, req: http::Request<hyper::body::Incoming>) -> http::Response<ProxyBody>;
}
pub fn set_backend_url_for_test(url: &str) -> Guard;      // Go BackendURLProvider / BackendHostProvider
pub fn set_token_refresher_for_test(f: fn(&Account) -> Result<String, AuthError>) -> Guard;  // Go TokenRefresher
pub fn subscribe_audit(buf: usize) -> (tokio::sync::mpsc::Receiver<RoutingEvent>, AuditSubscription);
pub fn emit_routing_event(ev: RoutingEvent) -> bool;
// plus every other exported fn in proxy.go: hop_by_hop_names, backend_host, backend_url, user_agent,
// read_request_body, is_generation_request, is_stream_request, filter_request_headers,
// filter_response_headers, send_buffered (returns Response), dropped_audit_events, audit_queue_len, ...
```

- `main.rs`: `agy-pool proxy-test-server <port> <state-dir> <backend-url>` exactly as Go's `eval.go` (configure state dir, backend URL/host override, token refresher returning `token-<id>`, print `READY`, serve on `127.0.0.1:<port>`).

- [ ] **Step 1: Port the listed Go tests.** Test harness: a `serve_once(handler, request) -> (status, headers, body bytes)` helper that runs the handler on a real local hyper server (so client disconnects are real socket closes), and raw `tokio::net::TcpListener` upstreams for fault injection exactly like Go's `net.Listen` tests. Commitment tests that check Go wrapper internals (`ResponseCommitter` state enum, `Hijack`) go to `PORTING_NOTES.md`, each mapped to the Rust test that proves the behaviour (a response is returned once; nothing after `Done` can trigger failover).
- [ ] **Step 2: Add the Rust-specific tests** (pin Review Focus #1–#3):

```rust
#[tokio::test]
async fn client_disconnect_releases_everything() {
    // Upstream accepts, reads the request, then waits 2 s before answering.
    // Client sends a generation request through the handler, then closes its socket after 200 ms.
    // Assert after 1 s: upstream saw exactly 1 request; no second account was contacted;
    // observability::in_flight_generation(acc) == 0; no inflight_leases rows remain;
    // no verification confirmation claim remains; the account's affinity binding is unchanged.
}

#[tokio::test]
async fn sse_first_chunk_not_buffered() {
    // Upstream SSE: sends "data: 1\n\n", waits 1 s, sends "data: 2\n\n", ends.
    // Client reads through the handler: the first event must arrive within 300 ms of the
    // request, i.e. before upstream sends the second.
}

#[tokio::test]
async fn ambiguous_failure_never_contacts_second_account() {
    // Pool with two healthy accounts. Upstream: accept, read request, close socket (no response).
    // Expect 502, exactly one upstream connection, no_replay_prevented_total +1,
    // and the second account's upstream counter 0.
}
```

  Write these with real code (the comments describe the required assertions; the harness from Step 1 provides the servers).
- [ ] **Step 3: Run** `scripts/rust-test.sh test --lib proxy::` — compile failure.
- [ ] **Step 4: Port `proxy.go` (handler, dispatch loop, routing events, `/metrics`, `/healthz`), `commitment.go` semantics and `stream.go`.** Dispatch decision table must map 1:1 to `dispatchAccount` in `proxy.go` (token refresh failure → failover; `ConnectError` → failover `PreConnectTransportFailure`; client gone → terminal; any `SendError` → terminal 502/504 + `record_no_replay_prevented`; explicit status cases → failover; everything else → terminal). Streaming: `RecordSuccess` before streaming as Go; body wrapper logs `STREAM TRUNCATED`, the end-of-stream `PROXY … ttfb … total …` and `CACHE …` lines exactly as Go, and records `no_replay_prevented` on truncation.
- [ ] **Step 5: Run** `scripts/rust-test.sh test --lib proxy:: -- --test-threads=16` three times — all pass every time.
- [ ] **Step 6: Gate and commit** — `feat(rust): port proxy handler with structural no-replay`.

### Task 14: Port `daemon` and the daemon-child entry point

**Files:**
- Create: `rust/src/daemon/{mod.rs,control.rs,instance.rs,lifecycle.rs,log.rs,pid.rs,runtime.rs,server.rs,systemd.rs}`, `rust/tests/daemon_child.rs`
- Modify: `rust/src/main.rs`, `rust/src/lib.rs`, `rust/PORTING_NOTES.md`
- Spec: `internal/daemon/*.go` and tests (52); `internal/cli/daemon_cmd.go` `runDaemonChild`; `cmd/agy-pool/main.go`; `contrib/systemd/user/agy-pool.service`

**Interfaces:**
- Consumes: `proxy::Handler`, `config`, `storage`, `quota` (startup sweep, periodic refresh), `auth` (token pre-refresh), `observability`.
- Produces: `InstanceContext` + `resolve_instance_context(data_dir, config_path, port, listen_host)`, `matches_instance`, `check_status_with_file`, `check_instance_status`, `DaemonInfo` (same JSON in the PID file as Go), `write_pid_file`, `read_pid_file`, `is_process_alive`, `is_agy_pool_process` (must recognise both the Go and the Rust binary), `try_acquire_pid_lock(path) -> Result<PidLock, _>` (`flock` on `<pid>.lock`, same file as Go so Go and Rust gateways exclude each other), `remove_pid_file_if_owned`, `check_daemon_mismatch`, `rotate_log_if_needed`, `clear_log`, `maybe_rotate_log`, `format_size`, runtime stats (`collect_runtime_stats` from `/proc/self`, same JSON file as Go; `format_go_version` becomes the Rust version string with the same layout), `ServerOptions` (same fields; `handler` is `Arc<proxy::Handler>`, ready/bound-port signals are `tokio::sync::oneshot`), `run_foreground(opts) -> Result<(), DaemonError>` (PID lock, PID file, listener, SIGTERM/SIGINT graceful shutdown, startup quota sweep, periodic quota refresh, token pre-refresh, log rotation hook), `start_instance`/`start_daemon` (spawn `current_exe()` with `AGY_POOL_DAEMON_CHILD=1`, `setsid`, log redirection as Go), `stop_instance`/`stop_daemon`/`restart_*`, `is_port_listening`, `is_daemon_running`, `is_daemon_outdated`, systemd detection as Go, `run_daemon_child(stderr) -> i32` (port of `runDaemonChild`).
- `main.rs`: if `AGY_POOL_DAEMON_CHILD=1` → `std::process::exit(daemon::run_daemon_child(&mut stderr))`, before any other dispatch, as Go.

- [ ] **Step 1: Port the 52 daemon tests.** Go's `TestMain` (temp data dir, test mode) becomes a shared `fn test_env()` returning a guard that holds `TEST_LOCK` for the whole test. Tests that re-exec the Go test binary use `env!("CARGO_BIN_EXE_agy-pool")` from integration tests in `rust/tests/daemon_child.rs`; `TestDaemonChild_ProductionLifecycle` builds nothing — it starts the Rust binary as a daemon child with a temp `AGY_DATA_DIR`, a temp `AGY_CONFIG_FILE` whose `port` is ephemeral, and a temp `AGY_GEMINI_DIR` (as Go's test does; there is no PID-file override), checks `/healthz`, sends SIGTERM, and checks the PID file is removed.
- [ ] **Step 2: Add** `second_rust_gateway_refused_while_go_runs` (Review Focus #4): build the Go binary into a temp dir (`go build -o $tmp/agy-pool-go ./cmd/agy-pool`, from the repo root), start it as a daemon child on an ephemeral port with a temp data dir, wait for its PID file; start the Rust binary as a daemon child on the same data dir → it must exit non-zero with Go's lock-held message and the Go gateway's `/healthz` must still answer; then the reverse (Rust holds the lock, Go is refused). Kill both at the end even on failure (guard with Drop).
- [ ] **Step 2b: Live-gateway safety (this host runs the real gateway on 8899 under systemd).** `scripts/rust-test.sh` already runs tests in their own PID namespace with a failing `systemctl` stub; add the code-level layer too:
  - No test uses port 8899 or `config::DEFAULT_PORT`; every port comes from binding `127.0.0.1:0`.
  - Every spawned child process (Rust or Go binary) gets explicit temp `AGY_DATA_DIR`, `AGY_CONFIG_FILE` and `AGY_GEMINI_DIR`; never inherit the parent's.
  - All signals go through one function (`daemon::send_signal(pid, sig)`), and all systemctl calls through `daemon::systemctl(scope, verb)`. Under `cfg(test)`, `send_signal` panics if `pid` equals the `pid` in `<real host home>/.local/share/agy-pool/agy-pool.pid`, and `systemctl` panics unconditionally (unit tests cover `cgroup_scope` parsing only). Add `send_signal_refuses_live_gateway_pid` and `systemctl_refused_in_tests` tests.
  - Every test that starts a process holds a Drop guard that SIGKILLs it, so a failing assert never leaks a daemon.
- [ ] **Step 3: Run** `scripts/rust-test.sh test --lib daemon::` and `scripts/rust-test.sh test --test daemon_child` — compile failure.
- [ ] **Step 4: Port the daemon files and the main.rs entry.**
- [ ] **Step 5: Run** both test commands with `-- --test-threads=16` — all pass.
- [ ] **Step 6: Gate and commit** — `feat(rust): port daemon lifecycle and foreground server`.

### Task 15: Port `diagnostics`

**Files:**
- Create: `rust/src/diagnostics/{mod.rs,effectiveness.rs,routing_audit.rs,routing_audit_eval.rs}`
- Modify: `rust/src/lib.rs`
- Spec: `internal/diagnostics/*.go` and tests (41)

**Interfaces:**
- Consumes: `daemon::InstanceContext`, `proxy::RoutingEvent`, `scheduler`, `quota`, `accounts`, `config`, `upstream` (TLS probe).
- Produces: one Rust fn per exported Go fn (`set_agy_binary_finder`, `set_agy_entrypoint_finder`, `set_agy_version_provider`, `set_backend_host_provider`, `set_tls_prober`, `is_shim_or_self`, `validate_native_candidate`, `resolve_native_agy_binary`, `find_real_agy_binary`, `installed_agy_version`, `lease_fallback_rate_high`, `run_doctor(&mut impl Write) -> bool`, `compute_effectiveness_report`, `evaluate_routing_attempt`, `evaluate_frozen_routing_event`) and the Go types with the same JSON field names.

- [ ] **Step 1: Port the 41 tests**; `run_doctor` output tests compare full text with Go where the Go test does.
- [ ] **Step 2: Run** `scripts/rust-test.sh test --lib diagnostics::` — compile failure.
- [ ] **Step 3: Port the four files.** `is_shim_or_self` must treat both the Go and the Rust `agy-pool` binaries as "self".
- [ ] **Step 4: Run** `scripts/rust-test.sh test --lib diagnostics::` — all pass.
- [ ] **Step 5: Gate and commit** — `feat(rust): port diagnostics`.

### Task 16: Go↔Rust gateway differential test and Phase 3 gate

**Files:**
- Create: `rust/tests/differential.rs`
- Modify: `rust/PORTING_NOTES.md` (final review of entries)

**Interfaces:**
- Consumes: both binaries' `proxy-test-server <port> <state-dir> <backend-url>` mode; `rust/interop/goside` to seed identical pools.

- [ ] **Step 1: Write the differential test.** For each scenario: build the Go binary once (`go build -o $tmp/agy-pool-go ./cmd/agy-pool` from the repo root), seed two temp state dirs with the same pool (`goside seed` from Task 5 — extend it with a `seed3` subcommand creating three accounts with distinct quota states), start a scripted fake upstream (a raw tokio listener whose behaviour per request is chosen by the scenario and which records `(Authorization header → account token, path, body)` per connection), start Go and Rust `proxy-test-server` on ephemeral ports pointing at it, send the identical client request to each, and compare:
  - downstream status, body bytes, and the `X-Agy-Pool-Account`, `X-Agy-Pool-Account-Id`, `X-Agy-Pool-Strategy`, `X-Agy-Pool-Affinity` headers;
  - the ordered list of accounts (by `token-<id>`) the upstream saw, and the number of upstream connections.
  Scenarios (each a separate `#[test]`): `ok_buffered`, `ok_sse_three_events`, `quota_429_fails_over`, `capacity_503_fails_over_and_pauses_model`, `auth_401_fails_over`, `ordinary_500_terminal`, `reset_after_request_terminal`, `eof_before_headers_terminal`, `sse_truncated_mid_stream`, `session_affinity_sticks_across_two_requests` (same `cascade_id` twice → same account both times on both gateways).
- [ ] **Step 2: Run** `scripts/rust-test.sh test --test differential -- --test-threads=1` — expected: compile failure, then implement until all scenarios match. A mismatch is a Rust bug unless the reviewer records it in `PORTING_NOTES.md` as an accepted difference.
- [ ] **Step 3: Phase 3 gate**

```bash
scripts/rust-test.sh fmt --check
scripts/rust-test.sh clippy --all-targets -- -D warnings
scripts/rust-test.sh test -- --test-threads=16
export PATH=$HOME/sdk/go1.26.10/bin:$PATH
go test ./internal/proxy ./internal/daemon ./internal/diagnostics
git status --short   # must print nothing
if ls internal/*/zz_* 2>/dev/null; then echo "LEFTOVER zz FILE"; exit 1; fi
```

- [ ] **Step 4: Commit** — `test(rust): Go/Rust gateway differential test`.

---

## After Phase 3

Claude reviews `git diff 85e14f5..rust-rewrite`, reads `rust/PORTING_NOTES.md` entry by entry, re-runs the gate (including the differential test), pushes, and writes the Phase 4 (CLI) plan. Phase 5 (candidate on port 8901) can only start after Phase 4, because the candidate needs `agy-pool run`.
