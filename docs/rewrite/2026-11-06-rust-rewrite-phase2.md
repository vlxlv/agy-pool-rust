# agy-pool Rust Rewrite — Phase 2 (Domain) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Execution in this repo:** each task is delegated to agy (agy-worker `agy_run`) with the task text as instructions and "the listed tests pass" as acceptance; Claude reviews the diff and re-runs the tests before the next task.

**Goal:** Port the domain packages — `auth`, `accounts`, `quota`, `scheduler`, `forecast`, `conversation` — to Rust with Go-identical behaviour, on top of the Phase 1 foundation.

**Architecture:** One Rust module per Go package under `rust/src/`. Network calls (OAuth token refresh, quota fetch) are `async` on tokio through one shared HTTPS client module (`rust/src/http.rs`, hyper-util + rustls with native roots), which Phase 3's proxy reuses. Time is read through one overridable clock (`rust/src/clock.rs`) replacing Go's per-package `NowFunc` vars. The shared golden fixtures in `testdata/quota/` and `testdata/scheduler/` run against Rust exactly as the Go tests run them.

**Tech Stack:** Phase 1 stack + tokio (`rt-multi-thread`, `macros`, `time`, `net`, `sync`), hyper 1, hyper-util (`client-legacy`, `http1`, `tokio`), http-body-util, hyper-rustls (`native-tokio`, `http1`, `ring`), pbkdf2, hmac, base64, getrandom, url (form encoding).

**Spec:** `docs/superpowers/plans/2026-11-05-rust-rewrite.md` (master plan; its Global Constraints apply unchanged) + the Go source and tests of each ported package.

## Global Constraints

All Global Constraints of the master plan apply. In addition:

- Branch `rust-rewrite` (already pushed). Commit per task; agy may not push — Claude pushes after review.
- Toolchain: `export PATH=$HOME/.cargo/bin:$HOME/sdk/go1.26.10/bin:$PATH AGY_TEST_MODE=1`.
- **Running tests on this host:** only through `scripts/rust-test.sh [cargo args]` (e.g. `scripts/rust-test.sh test --lib storage::`, `scripts/rust-test.sh clippy --all-targets -- -D warnings`). It runs cargo in a private mount namespace with `~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.gemini` and `~/.local/bin` read-only and sets PATH and `AGY_TEST_MODE=1`. Never run bare `cargo test` here: on 2026-11-06 a racing test overwrote the live state.db. Test builds also panic if `data_dir()`/`native_agy_dir()` resolve to a live path; test helpers that take `TEST_LOCK` must return the guard so the test body holds it.
- Go `sort.SliceStable(xs, less)` → Rust `xs.sort_by(|a, b| if less(a, b) { Less } else if less(b, a) { Greater } else { Equal })`. Never rewrite a `less` function as `partial_cmp(...).unwrap()` or a key tuple: ties and NaN/None must order exactly as Go does.
- Go package vars that tests override (`TokenURL`, `HTTPClient`, `NowFunc`, refresh endpoints, backoff tables) become `static` `RwLock`/`OnceLock` values with a setter returning a restore guard; tests holding `crate::config::TEST_LOCK` may override them.
- Go `float64` math is copied operation-for-operation (same order of operations, same constants, `f64` everywhere, `as i64` only where Go converts). Golden fixtures compare with the same tolerance the Go test uses.
- Go time formatting/parsing (RFC 3339, `ParseISOOrTimestamp`) must accept and produce the same strings; use `time`-free hand-ported parsing or the `jiff`/`chrono` crate pinned to an exact version — pick one in Task 6 and use it everywhere after.
- No real network in tests: every HTTP test uses the local test server from Task 6 (`crate::testutil::TestServer`), never Google endpoints.
- Credentials: `DecodeCred` hex+XOR constants are copied verbatim; never print decoded secrets in test output or logs.

## Review Focus

1. **Encrypted export written by Go, imported by Rust (and the reverse)** — expected: byte-identical ciphertext/tag for the same password, salt and nonce; a wrong password or modified tag fails with Go's message. Pinned in Task 7 (`bundle_matches_go_vectors`).
2. **Scheduler ordering with ties, unknown quota (None) and equal health** — expected: same order as Go. Pinned in Task 9 by the golden `ordering_cases.json` runner plus `ordering_ties_match_go`.
3. **Quota fraction/reset values at window edges (exactly 0, exactly 1, reset in the past, missing reset)** — expected: same capacity, freshness, rank and refresh decisions as Go. Pinned in Task 8 by the golden `capacity_cases.json` runner.
4. **Token refresh failing (HTTP 400 `invalid_grant`, 5xx, timeout, malformed JSON)** — expected: same error classification and account status changes as Go. Pinned in Task 6 by the ported auth tests.
5. **Two concurrent quota refreshes for one account** — expected: single-flight, one upstream call, both callers get the result. Pinned in Task 8 by the ported refresh single-flight test.

---

## File Structure

- `rust/src/clock.rs` — `now()`, `now_unix_f64()`, test override guard.
- `rust/src/http.rs` — shared HTTPS client (`client()`), `post_form`, `get_json`, timeouts.
- `rust/src/testutil.rs` (`#[cfg(test)]`, `pub(crate)`) — `TestServer` (tokio + hyper local HTTP server with a closure handler, records requests).
- `rust/src/auth/mod.rs` — `internal/auth/auth.go`.
- `rust/src/accounts/{mod.rs,crypto.rs,order.rs}` — `internal/accounts/{accounts,crypto,order}.go`.
- `rust/src/quota/{mod.rs,format.rs,history.rs,refresh.rs}` — `internal/quota/*.go`.
- `rust/src/scheduler/mod.rs` — `internal/scheduler/scheduler.go`.
- `rust/src/forecast/{mod.rs,burn_rate.rs,format.rs,runway.rs}` — `internal/forecast/*.go`.
- `rust/src/conversation/mod.rs` — `internal/conversation/conversation.go`.

Dependency order (tasks follow it): `auth` → `accounts` → `quota` → `scheduler` → `forecast`; `conversation` only needs `config`.

### Task 6: Clock, HTTP client, test server, and `auth`

**Files:**
- Create: `rust/src/clock.rs`, `rust/src/http.rs`, `rust/src/testutil.rs`, `rust/src/auth/mod.rs`
- Modify: `rust/src/lib.rs`, `rust/Cargo.toml`
- Spec: `internal/auth/auth.go`, `internal/auth/auth_test.go` (15 tests)

**Interfaces:**
- Consumes: `config::{get_static_config, agy_token_file, assert_safe_native_token_write, TEST_LOCK}`, `storage::{Account, pool_transaction}`, `observability::record_auth_refresh_*`.
- Produces:
  - `clock::now() -> std::time::SystemTime`, `clock::now_unix() -> f64`, `clock::set_now_for_test(f: fn() -> SystemTime) -> ClockGuard` (restores on Drop).
  - `http::client() -> &'static Client` (one pooled client; generation-specific settings come in Phase 3), `http::post_form(url: &str, form: &[(&str, &str)], timeout: Duration) -> Result<(u16, Vec<u8>), HttpError>`, `http::get_json(url: &str, bearer: Option<&str>, timeout: Duration) -> Result<(u16, Vec<u8>), HttpError>`, `http::post_json(url, bearer, body: &[u8], timeout)` same return.
  - `testutil::TestServer::start(handler: impl Fn(Request) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static) -> TestServer` with `.url() -> String`, `.requests() -> Vec<RecordedRequest>`; stops on Drop. Must run under `#[tokio::test]`.
  - `auth`: `decode_cred(hex: &str, key: u8) -> String`, `client_id() -> String`, `client_secret() -> String` (env `AGY_CLIENT_ID`/`AGY_CLIENT_SECRET` override as in Go), `set_token_url_for_test(&str) -> UrlGuard`, and an async Rust fn for every exported Go function in `auth.go` (snake_case), keeping Go's error texts.

- [ ] **Step 1: Pick and pin the time crate** — use `jiff` (exact version) for RFC 3339 parse/format; record the choice in `rust/Cargo.toml` with a one-line comment `# time: jiff (Phase 2 decision, used everywhere)`.
- [ ] **Step 2: Write `clock.rs` and its test**

```rust
use std::sync::RwLock;
use std::time::SystemTime;

static OVERRIDE: RwLock<Option<fn() -> SystemTime>> = RwLock::new(None);

pub fn now() -> SystemTime {
    match *OVERRIDE.read().unwrap_or_else(|e| e.into_inner()) {
        Some(f) => f(),
        None => SystemTime::now(),
    }
}

pub fn now_unix() -> f64 {
    now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub struct ClockGuard(Option<fn() -> SystemTime>);

pub fn set_now_for_test(f: fn() -> SystemTime) -> ClockGuard {
    let mut w = OVERRIDE.write().unwrap_or_else(|e| e.into_inner());
    ClockGuard(w.replace(f))
}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        *OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn override_and_restore() {
        let _l = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fn fixed() -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }
        {
            let _g = set_now_for_test(fixed);
            assert_eq!(now_unix(), 1_800_000_000.0);
        }
        assert!(now_unix() > 1_700_000_000.0 && now_unix() != 1_800_000_000.0);
    }
}
```

- [ ] **Step 3: Write `http.rs` + `testutil.rs`** and a test `post_form_round_trip` (`#[tokio::test]`): a `TestServer` answering `200` with `{"ok":true}` records one request; `http::post_form(&format!("{}/token", srv.url()), &[("a","1")], 5s)` returns `(200, body)`, and the recorded request has method `POST`, path `/token`, header `content-type: application/x-www-form-urlencoded`, body `a=1`.
- [ ] **Step 4: Port the 15 auth tests** (snake_case names; Go `httptest.NewServer` → `TestServer`; Go `TokenURL = server.URL` → `set_token_url_for_test`). Run `cargo test --lib auth::` — expected: compile failure.
- [ ] **Step 5: Port `auth.go`.** Run `cargo test --lib auth:: clock:: http::` — expected: 15 + 1 + 1 passed.
- [ ] **Step 6: Gate and commit** — `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`; commit `feat(rust): port auth with shared clock and HTTP client`.

### Task 7: Port `accounts` (pool operations, ordering, encrypted bundles)

**Files:**
- Create: `rust/src/accounts/{mod.rs,crypto.rs,order.rs}`, `rust/src/accounts/testdata_bundle.json` (generated, see Step 1)
- Modify: `rust/src/lib.rs`, `rust/Cargo.toml`
- Spec: `internal/accounts/{accounts,crypto,order}.go` and their tests (44 tests)

**Interfaces:**
- Consumes: `auth`, `config`, `storage`, `clock`.
- Produces: one Rust fn per exported Go fn (snake_case) incl. `set_before_clear_hook`, `set_pool_transaction`, `set_account_fields_persister`, `set_smart_order_hook` (Go hook vars → `RwLock<Option<Box<dyn Fn ... + Send + Sync>>>`), `encrypt_bundle(plaintext: &[u8], password: &str, salt: &[u8], nonce: &[u8]) -> Result<EncryptedBundle, AccountsError>`, `decrypt_bundle(&EncryptedBundle, &str) -> Result<Vec<u8>, AccountsError>`, `export_pool(path, encrypt, password, no_stats) -> Result<String, _>`, `import_pool(path, password, replace, skip_existing) -> Result<ImportResult, _>`, `move_account`, `swap_accounts`, `display_account_name`. `EncryptedBundle` serde field names = Go JSON tags. Tag check uses `hmac::Mac::verify_slice` (constant time).

- [ ] **Step 1: Generate Go test vectors** — create the temporary file `internal/accounts/zz_vectors_test.go`:

```go
package accounts

import (
	"encoding/json"
	"os"
	"testing"
)

func TestZZVectors(t *testing.T) {
	type vec struct {
		Password  string           `json:"password"`
		Plaintext string           `json:"plaintext"`
		Salt      []byte           `json:"salt"`
		Nonce     []byte           `json:"nonce"`
		Bundle    *EncryptedBundle `json:"bundle"`
	}
	var out []vec
	for _, p := range []string{"", "a", "exactly-32-bytes-of-plaintext!!!", "longer than one 32-byte keystream block, with unicode: 配额 ✓"} {
		salt := []byte("0123456789abcdef")
		nonce := []byte("fedcba9876543210")
		b, err := EncryptBundle([]byte(p), "correct horse", salt, nonce)
		if err != nil {
			t.Fatal(err)
		}
		out = append(out, vec{"correct horse", p, salt, nonce, b})
	}
	data, _ := json.MarshalIndent(out, "", "  ")
	if err := os.WriteFile(os.Getenv("VEC_OUT"), data, 0o644); err != nil {
		t.Fatal(err)
	}
}
```

Run `VEC_OUT=$PWD/rust/src/accounts/testdata_bundle.json go test -run TestZZVectors ./internal/accounts`, then delete `internal/accounts/zz_vectors_test.go` (must not be committed).

- [ ] **Step 2: Write the vector test** in `rust/src/accounts/crypto.rs` (pins Review Focus #1):

```rust
#[test]
fn bundle_matches_go_vectors() {
    #[derive(serde::Deserialize)]
    struct Vec_ {
        password: String,
        plaintext: String,
        salt: String,  // Go []byte → base64 in JSON
        nonce: String,
        bundle: EncryptedBundle,
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let vecs: Vec<Vec_> = serde_json::from_str(include_str!("testdata_bundle.json")).unwrap();
    assert_eq!(vecs.len(), 4);
    for v in vecs {
        let salt = b64.decode(&v.salt).unwrap();
        let nonce = b64.decode(&v.nonce).unwrap();
        let got = encrypt_bundle(v.plaintext.as_bytes(), &v.password, &salt, &nonce).unwrap();
        assert_eq!(got.ciphertext, v.bundle.ciphertext, "ciphertext for {:?}", v.plaintext);
        assert_eq!(got.tag, v.bundle.tag, "tag for {:?}", v.plaintext);
        assert_eq!(decrypt_bundle(&v.bundle, &v.password).unwrap(), v.plaintext.as_bytes());
        assert!(decrypt_bundle(&v.bundle, "wrong password").is_err());
        let mut tampered = v.bundle.clone();
        tampered.tag = b64.encode([0u8; 32]);
        assert!(decrypt_bundle(&tampered, &v.password).is_err());
    }
}
```

- [ ] **Step 3: Port the 44 accounts tests** (snake_case). Run `cargo test --lib accounts::` — expected: compile failure.
- [ ] **Step 4: Port the three Go files**; `order.go`'s `sort.SliceStable` follows the Global Constraint. Run `cargo test --lib accounts::` — expected: 45 passed.
- [ ] **Step 5: Gate and commit** — `feat(rust): port accounts with Go-compatible encrypted bundles`.

### Task 8: Port `quota` (capacity, formatting, history, refresh)

**Files:**
- Create: `rust/src/quota/{mod.rs,format.rs,history.rs,refresh.rs}`
- Modify: `rust/src/lib.rs`
- Spec: `internal/quota/{quota,format,history,refresh}.go` and tests (32 tests); fixture `testdata/quota/capacity_cases.json`

**Interfaces:**
- Consumes: `accounts`, `auth`, `config`, `observability`, `storage`, `clock`, `http`.
- Produces: `CapacityState` (all Go fields, same names snake_case, `Option<f64>` for Go `*float64`, `Option<SystemTime>` for `*time.Time`), `compute_capacity_state(&Account, now: f64) -> CapacityState`, `freshness(...)`, `parse_quota_fraction`, `parse_iso_or_timestamp`, `render_progress_bar`, `render_compact_progress_bar`, `format_remaining_time`, history append/read for `quota-history.jsonl` (same line format: `{"account","g5","g5_reset","gw","gw_reset","ts"}` key order), async refresh fns with single-flight (`tokio::sync` + a `Mutex<HashMap<String, Shared>>`), `reset_refresh_state`, `refresh_retry_entry`, `is_refresh_in_flight`; endpoint and client overrides for tests as guards.

- [ ] **Step 1: Port the 32 quota tests**, including the golden runner that reads `concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/quota/capacity_cases.json")` and checks `expected_capacity`, `expected_freshness`, `expected_rank`, `expected_refresh_needed` for all 6 cases with the Go test's tolerance; refresh tests use `TestServer`. Run `cargo test --lib quota::` — expected: compile failure.
- [ ] **Step 2: Port the four Go files.** The background refresh goroutine (`refresh.go` `go func()`) becomes `tokio::spawn`; callers that are sync in Go and stay sync in Rust use `tokio::runtime::Handle::current()` only if Go's call graph requires it — otherwise make the caller `async`.
- [ ] **Step 3: Run** `cargo test --lib quota::` — expected: 32 passed (golden runner included).
- [ ] **Step 4: History cross-check** — append one entry from Rust to a temp `quota-history.jsonl` and assert the line equals, byte-for-byte, the line Go's history writer produces for the same values (copy the expected line from the Go history test or generate it with a temporary `zz_` Go test as in Task 7, then delete that file).
- [ ] **Step 5: Gate and commit** — `feat(rust): port quota capacity, history and refresh`.

### Task 9: Port `scheduler`

**Files:**
- Create: `rust/src/scheduler/mod.rs`
- Modify: `rust/src/lib.rs`
- Spec: `internal/scheduler/scheduler.go` and tests (42 tests); fixture `testdata/scheduler/ordering_cases.json`

**Interfaces:**
- Consumes: `accounts`, `config`, `observability`, `quota`, `storage`, `clock`.
- Produces: every exported Go const/fn snake_case/SCREAMING_CASE: `FIVE_HOUR_RESERVE_FLOOR`, `WEEKLY_RESERVE_FLOOR`, `WEEKLY_PACE_SLACK`, `SMART_HARVEST_5H_WINDOW`, `SMART_HARVEST_MIN_HEADROOM`, `SMART_HARVEST_MIN_WEEKLY_HEADROOM`, `over_weekly_pace`, `over_weekly_pace_capacity`, `effective_worst_pace`, `smart_harvest_candidate`, `is_reserve_candidate`, `is_restricted`, `order_candidates(accounts: &[Account], strategy: &str, pool: &Pool, now: f64) -> Vec<Account>` (Go returns `[]*Account`; Rust returns indices or `Arc<Account>` clones — pick `Vec<Arc<Account>>` only if `storage::Pool` already holds `Arc`, else `Vec<usize>` indices into `pool.accounts`; document the choice in the fn doc), `rotate_top_tier`, `advance_smart_rr_cursor`, `smart_rr_cursor`, `reset_smart_rr_cursor`.

- [ ] **Step 1: Port the 42 scheduler tests** incl. the golden `ordering_cases.json` runner (both cases, comparing `expected_ordered_ids`). Add (pins Review Focus #2):

```rust
#[test]
fn ordering_ties_match_go() {
    // Three accounts with identical quota state: Go's stable sort keeps pool order.
    // A fourth with unknown quota (no LastQuota) must land where Go puts it.
    // Build them with the same helper the ported tests use, then:
    // assert_eq!(ids(order_candidates(...)), expected) where `expected` is copied from running
    // the same input through Go (temporary zz_ test, deleted afterwards, as in Task 7).
}
```

Replace the comments with real code: build the four accounts, generate the expected ID order from Go with a temporary `internal/scheduler/zz_ties_test.go` that prints `OrderCandidates` IDs for the same input under strategies `smart`, `round-robin` (use the exact strategy constants from `internal/config`), then delete that file. The Rust test asserts both orders.

- [ ] **Step 2: Run** `cargo test --lib scheduler::` — expected: compile failure.
- [ ] **Step 3: Port `scheduler.go`.** `sort.SliceStable` per the Global Constraint.
- [ ] **Step 4: Run** `cargo test --lib scheduler::` — expected: 43 passed.
- [ ] **Step 5: Gate and commit** — `feat(rust): port scheduler`.

### Task 10: Port `forecast` and `conversation`, then Phase 2 gate

**Files:**
- Create: `rust/src/forecast/{mod.rs,burn_rate.rs,format.rs,runway.rs}`, `rust/src/conversation/mod.rs`
- Modify: `rust/src/lib.rs`
- Spec: `internal/forecast/*.go` (4 tests), `internal/conversation/conversation.go` (4 tests)

**Interfaces:**
- Consumes: `accounts`, `quota`, `storage`, `config`.
- Produces: `get_burn_rate(&Pool, now: Option<f64>) -> BurnRateTelemetry`, `print_runway_summary(w: &mut impl Write, &Pool, now: f64, width: Option<usize>)` (output byte-identical to Go for the same input), `format_duration_short(Option<f64>) -> String`, `run_single_simulation(...)`, `simulate_quota_runway(&Pool, pace: Option<f64>, now: Option<f64>) -> Option<RunwayForecast>`; `find_latest_conversation_for_dir(&Path) -> Result<(String, String, String), ConversationError>`, `resolve_continue_arg(extra: &[String], err_out: &mut impl Write) -> Vec<String>`. Conversation DB is opened read-only with rusqlite (`OpenFlags::SQLITE_OPEN_READ_ONLY`), same query text as Go.

- [ ] **Step 1: Port the 8 tests**; add `runway_summary_matches_go`: fixed pool from `testdata/pool/valid_3_accounts.json`, fixed `now`, width 100 — expected text generated once from Go via a temporary `zz_` test (deleted afterwards) and pasted as a raw string.
- [ ] **Step 2: Run** `cargo test --lib forecast:: conversation::` — expected: compile failure.
- [ ] **Step 3: Port the files.**
- [ ] **Step 4: Run** `cargo test --lib forecast:: conversation::` — expected: 9 passed.
- [ ] **Step 5: Phase 2 gate**

```bash
cd rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && git status --short   # must print nothing
if ls internal/*/zz_* 2>/dev/null; then echo "LEFTOVER zz FILE"; exit 1; fi
go test ./internal/auth ./internal/accounts ./internal/quota ./internal/scheduler ./internal/forecast ./internal/conversation
```

Expected: all green; Go packages unchanged and passing.

- [ ] **Step 6: Commit** — `feat(rust): port forecast and conversation`.

---

## After Phase 2

Claude reviews `git diff 2b842dd..rust-rewrite`, re-runs the gate, checks no `zz_` Go files were committed (`git log --name-only 2b842dd..rust-rewrite | grep zz_` is empty), pushes, and writes the Phase 3 (Gateway) plan.
