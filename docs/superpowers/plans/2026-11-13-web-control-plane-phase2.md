# Web Control Plane Phase 2 (Account Controls) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the dashboard enable, disable, rename, reorder, refresh and switch accounts, and refresh all quotas, so routine pool work no longer needs SSH.

**Architecture:** A new `src/dashboard/control.rs` holds the account handlers. `dashboard::serve` sends every `POST /api/v1/accounts/<…>` through the existing `control_gate` (control header + JSON + token), then into `control::handle`. Accounts are addressed by exact `id` only; mutations go through `storage::pool_transaction`, quota refreshes through `quota::query_quota`, and CLI switching through `accounts::switch_account` (taught to prefer an exact id). The frontend adds a row action cell, a "Refresh all" button, a small toast, and a dialog for verification links.

**Tech Stack:** Rust 2024, tokio, hyper 1.12, http-body-util, serde/serde_json; single-file HTML/JS dashboard. No new crates.

**Spec:** `docs/superpowers/specs/2026-11-12-web-control-plane-design.md` (§4.5 C and §9; §10 lists the Phase 2 scope decisions this plan follows).

## Global Constraints

- Work in `/home/claude/work/agy-pool-rust-cp2` on branch `feat/dashboard-controls`. Never touch `/home/claude/work/agy-pool-rust` or any other directory.
- Run tests **only** through `timeout 900 scripts/test.sh <cargo args>` (cargo is `~/.cargo/bin/cargo`). Never run `cargo test` directly.
- Never start, stop or restart the live `agy-pool` gateway; never run release/LTO builds or benchmarks on this host.
- Format with `~/.cargo/bin/cargo fmt` (`max_width = 120`) before every commit; `timeout 900 scripts/test.sh clippy --all-targets -- -D warnings` must be clean before each task's commit.
- No new dependencies.
- Accounts are addressed by **exact `id` equality** in every dashboard handler. Never call `resolve_account_index` / `find_account_by_target` with a client-supplied value (they also match list positions, names and substrings).
- Every mutation is `POST` behind `control_gate` (Phase 1). JSON error shape `{"error":{"code":"…","message":"…"}}`; JSON responses carry `Cache-Control: no-store`.
- Never put storage or upstream error text in a response; log it with `crate::observability::log_message_level(LogLevel::Error, "DASHBOARD", …)` and return a fixed message.
- Never cancel a running `quota::query_quota` future: its single-flight entry would stay in `FLIGHT_MAP` and every later refresh of that account would wait forever. Spawn it and time out the wait instead.
- Only `https://` validation URLs are ever returned to the browser.
- Do not create backup files (`*.bak*`), do not push, do not open PRs, do not change any branch other than `feat/dashboard-controls`. Commit only the files listed in the task. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **An account whose name equals another account's id, or a digit-only id.** `switch` must pick the account with that exact id, not the first name match or the list position. Pinned by `switch_account_prefers_exact_id` (Task 1) and `test_switch_account_by_exact_id` (Task 2).
2. **The pool changes between page load and a reorder click** (CLI added/removed an account). The reorder must be refused with `409 POOL_CHANGED`, never silently drop or duplicate accounts. Pinned in `test_account_mutations` (Task 2).
3. **A verification link that is not `https://`** (corrupted state, hostile upstream). It must never reach the browser. Pinned in `test_quota_refresh_controls` (Task 2).
4. **The browser tab closes or the 15 s limit hits mid-refresh.** The upstream query must keep running and clean up after itself. There is no deterministic test for this without a slow mock upstream; the reviewer must check that `refresh_account` spawns `query_quota` and only times out the `JoinHandle`.
5. **Disabling the last enabled account from a phone tap.** The UI must ask for confirmation. Pinned by the asset test in Task 3.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/accounts/mod.rs` | `switch_account` prefers exact id; new `persist_cleared_restriction` |
| `src/dashboard/dto.rs` | `AccountDto.is_cli_base` |
| `src/dashboard/control.rs` (new) | Account control handlers and quota refresh helper |
| `src/dashboard/mod.rs` | Route `/api/v1/accounts/<…>` POSTs to `control::handle` |
| `src/dashboard/assets/index.html` | Row actions, Refresh all, toast, verification dialog |
| `tests/dashboard.rs` | Integration tests |
| `docs/superpowers/specs/2026-11-12-web-control-plane-design.md` | §10 Phase 2 scope (already written by Claude) |

---

### Task 1: Exact-id switching, restriction persistence helper, `is_cli_base`

**Files:**
- Modify: `src/accounts/mod.rs` (`switch_account` ~line 676; add a function after `clear_validation_restriction_cas` ~line 1245; tests module)
- Modify: `src/dashboard/dto.rs` (`AccountDto` ~line 30, `build_accounts_response` push ~line 217, any test literals)

**Interfaces:**
- Produces (used by Task 2):
  - `pub fn persist_cleared_restriction(before: &crate::storage::Account, after: &crate::storage::Account, quota: &crate::storage::QuotaState) -> Result<bool, AccountsError>`
  - `switch_account(target)` resolves an exact `id` match before falling back to `find_account_by_target`.
  - `AccountDto.is_cli_base: bool` (JSON field `is_cli_base`) — true for `pool.active_account_id`.

- [ ] **Step 1: Write the failing unit tests** — append inside `mod tests` in `src/accounts/mod.rs` (it already has `setup_isolated_test_dir()`):

```rust
    #[tokio::test]
    async fn switch_account_prefers_exact_id() {
        let _env = setup_isolated_test_dir();
        let token = |t: &str| crate::storage::Account {
            access_token: t.to_string(),
            token_expiry: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        // acc_1 is *named* like acc_2's id, and acc_3 has a digit-only id that is also a valid list position.
        let mut a1 = token("tok1");
        a1.id = "acc_1".to_string();
        a1.name = "acc_2".to_string();
        let mut a2 = token("tok2");
        a2.id = "acc_2".to_string();
        let mut a3 = token("tok3");
        a3.id = "1".to_string();
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![a1, a2, a3];
        pool.active_account_id = Some("acc_1".to_string());
        crate::storage::save_pool(&pool).unwrap();

        assert_eq!(switch_account("acc_2").await.unwrap().id, "acc_2");
        assert_eq!(switch_account("1").await.unwrap().id, "1");
        let loaded = crate::storage::load_pool().unwrap();
        assert_eq!(loaded.active_account_id.as_deref(), Some("1"));
    }

    #[test]
    fn persist_cleared_restriction_only_when_unchanged() {
        let _env = setup_isolated_test_dir();
        let restricted = crate::storage::Account {
            id: "acc_1".to_string(),
            status: crate::storage::STATUS_VALIDATION_REQUIRED.to_string(),
            validation_url: Some("https://accounts.google.com/v1".to_string()),
            rate_limited_until: Some(crate::clock::now_unix() + 3600.0),
            ..Default::default()
        };
        let mut pool = crate::storage::Pool::new_empty();
        pool.accounts = vec![restricted.clone()];
        crate::storage::save_pool(&pool).unwrap();

        let mut cleared = restricted.clone();
        cleared.status.clear();
        cleared.validation_url = None;
        cleared.rate_limited_until = None;
        let quota = crate::storage::QuotaState::default();

        // Someone re-restricted the account with a new link meanwhile: keep the stored state.
        crate::storage::pool_transaction(|p| {
            p.accounts[0].validation_url = Some("https://accounts.google.com/v2".to_string());
            Ok(())
        })
        .unwrap();
        assert!(!persist_cleared_restriction(&restricted, &cleared, &quota).unwrap());
        let stored = crate::storage::load_pool().unwrap().accounts[0].clone();
        assert_eq!(stored.status, crate::storage::STATUS_VALIDATION_REQUIRED);

        // Unchanged since `before`: the clear is persisted.
        assert!(persist_cleared_restriction(&stored, &cleared, &quota).unwrap());
        let stored = crate::storage::load_pool().unwrap().accounts[0].clone();
        assert!(stored.status.is_empty());
        assert_eq!(stored.validation_url, None);
        assert_eq!(stored.rate_limited_until, None);
    }
```

And append inside `mod tests` in `src/dashboard/dto.rs`:

```rust
    #[test]
    fn accounts_response_marks_cli_base() {
        let pool = crate::storage::Pool {
            accounts: vec![
                crate::storage::Account {
                    id: "acc_1".to_string(),
                    ..Default::default()
                },
                crate::storage::Account {
                    id: "acc_2".to_string(),
                    ..Default::default()
                },
            ],
            active_account_id: Some("acc_2".to_string()),
            ..Default::default()
        };
        let resp = build_accounts_response(&pool, false, 0.0);
        let flags: Vec<(String, bool)> = resp.accounts.iter().map(|a| (a.id.clone(), a.is_cli_base)).collect();
        assert_eq!(flags, vec![("acc_1".to_string(), false), ("acc_2".to_string(), true)]);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --lib accounts::tests::switch_account_prefers_exact_id accounts::tests::persist_cleared_restriction dto::tests::accounts_response_marks_cli_base`
Expected: compile errors (`persist_cleared_restriction`, `is_cli_base` not found). If cargo rejects several filters in one call, run the three filters as separate commands.

- [ ] **Step 3: Implement.**

In `switch_account`, replace the non-auto branch:

```rust
        match find_account_by_target(&pool.accounts, target) {
            Some(a) => a.clone(),
            None => {
                return Err(AccountsError::Message(format!("account '{}' not found", target)));
            }
        }
```

with:

```rust
        // An exact id wins over list positions and name matches (the dashboard always passes ids).
        match pool
            .accounts
            .iter()
            .find(|a| a.id == target)
            .or_else(|| find_account_by_target(&pool.accounts, target))
        {
            Some(a) => a.clone(),
            None => {
                return Err(AccountsError::Message(format!("account '{}' not found", target)));
            }
        }
```

Add after `fn clear_validation_restriction_cas(…) { … }`:

```rust
/// Persists a validation restriction that a quota probe cleared on `after`, unless the stored account
/// changed since `before` was read. Returns whether the restriction was cleared.
pub fn persist_cleared_restriction(
    before: &crate::storage::Account,
    after: &crate::storage::Account,
    quota: &crate::storage::QuotaState,
) -> Result<bool, AccountsError> {
    clear_validation_restriction_cas(after, &snapshot_account(before), quota).map(|r| r.cleared)
}
```

In `src/dashboard/dto.rs` add the last field of `AccountDto`:

```rust
    /// The account the native `agy` CLI currently uses (`pool.active_account_id`).
    pub is_cli_base: bool,
```

and in `build_accounts_response`, add to the `AccountDto { … }` literal:

```rust
            is_cli_base: pool.active_account_id.as_deref() == Some(acc.id.as_str()),
```

Run `grep -rn "AccountDto {" src tests` and add `is_cli_base: false` to any other literal that fails to compile.

- [ ] **Step 4: Run the tests**

Run: `timeout 900 scripts/test.sh test --lib accounts::` then `timeout 900 scripts/test.sh test --lib dashboard::`
Expected: all pass, including the three new tests and the existing `test_switch_account`.

- [ ] **Step 5: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/accounts/mod.rs src/dashboard/dto.rs
git commit -m "feat(accounts): switch by exact id first and expose restriction-clear persistence

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Account control endpoints

**Files:**
- Create: `src/dashboard/control.rs`
- Modify: `src/dashboard/mod.rs` (module list at the top; `serve` match; make `api_error`, `json_response`, `internal_error` `pub(super)` if needed by `control.rs` — they are private functions in the parent module, which a child module can already call, so no change should be needed)
- Modify: `tests/dashboard.rs`

**Interfaces:**
- Consumes: `control_gate`, `api_error`, `json_response`, `internal_error` (Phase 1, `src/dashboard/mod.rs`); `accounts::{switch_account, persist_cleared_restriction}` (Task 1); `quota::query_quota`; `storage::{load_pool, pool_transaction, STATUS_DISABLED, STATUS_VALIDATION_REQUIRED}`.
- Produces (used by Task 3) — HTTP contract, all `POST`, all behind `control_gate`:
  - `/api/v1/accounts/{id}/enable`, `/disable` → `200 {"ok":true}`
  - `/api/v1/accounts/{id}/rename` body `{"name":"…"}` → `200 {"ok":true}`; `400 INVALID_ARGUMENT` for empty / >64 chars / control characters
  - `/api/v1/accounts/{id}/switch` → `200 {"ok":true}`; `502 SWITCH_FAILED`
  - `/api/v1/accounts/{id}/refresh` → `200 {"ok":true,"status":"…","validation_url":"https://…"|null}`; `409 ACCOUNT_DISABLED`; `502 QUOTA_REFRESH_FAILED`
  - `/api/v1/accounts/refresh-all` → `200 {"ok":true,"refreshed":N,"failed":M}`; `409 REFRESH_ALREADY_IN_PROGRESS`
  - `/api/v1/accounts/reorder` body `{"ordered_ids":[…]}` → `200 {"ok":true}`; `409 POOL_CHANGED`
  - Unknown id → `404 ACCOUNT_NOT_FOUND`; unknown action → `404 NOT_FOUND`; non-POST → `405` with `Allow: POST`.

- [ ] **Step 1: Update the Phase 1 test whose path became a real route.** In `tests/dashboard.rs`, `test_trailing_slash_and_unknown_api_paths`, change the array entry

```rust
(Method::POST, "/api/v1/accounts/acc_1/enable")
```

to

```rust
(Method::POST, "/api/v1/nope/deeper")
```

- [ ] **Step 2: Write the failing integration tests** — append to `tests/dashboard.rs` (helpers `TEST_TOKEN`, `set_dashboard_config`, `hdrs`, `bearer`, `error_code` exist from Phase 1):

```rust
fn control_headers() -> HeaderMap {
    hdrs(&[
        ("x-agy-pool-control", "1"),
        ("content-type", "application/json"),
        ("authorization", bearer(TEST_TOKEN).as_str()),
    ])
}

async fn post_control(addr: SocketAddr, path: &str, body: &str) -> (StatusCode, Bytes) {
    let (status, _, body) =
        send_request(addr, Method::POST, path, control_headers(), Bytes::copy_from_slice(body.as_bytes())).await;
    (status, body)
}

fn stored_account(id: &str) -> agy_pool::storage::Account {
    agy_pool::storage::load_pool()
        .unwrap()
        .accounts
        .into_iter()
        .find(|a| a.id == id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_account_mutations() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    // Gate: no control header / no token are refused before anything changes.
    let (status, _, _) = send_request(
        addr,
        Method::POST,
        "/api/v1/accounts/acc_1/disable",
        hdrs(&[("content-type", "application/json")]),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, headers, _) =
        send_request(addr, Method::GET, "/api/v1/accounts/acc_1/disable", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(hyper::header::ALLOW).unwrap(), "POST");
    assert_eq!(stored_account("acc_1").status, "");

    // Disable / enable.
    assert_eq!(post_control(addr, "/api/v1/accounts/acc_1/disable", "{}").await.0, StatusCode::OK);
    assert_eq!(stored_account("acc_1").status, agy_pool::storage::STATUS_DISABLED);
    assert_eq!(post_control(addr, "/api/v1/accounts/acc_1/enable", "{}").await.0, StatusCode::OK);
    assert_eq!(stored_account("acc_1").status, "");

    // Unknown id and unknown action.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/disable", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND"));
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/explode", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::NOT_FOUND, "NOT_FOUND"));
    // A list position or an email is not an id.
    let (status, _) = post_control(addr, "/api/v1/accounts/1/disable", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Rename trims and validates.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/rename", r#"{"name":"  Work  "}"#).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(stored_account("acc_2").name, "Work");
    let long = format!(r#"{{"name":"{}"}}"#, "a".repeat(65));
    for bad in [r#"{"name":"   "}"#, long.as_str(), r#"{"name":"a\nb"}"#, r#"{"title":"x"}"#, "nope"] {
        let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/rename", bad).await;
        assert_eq!((status, error_code(&body).as_str()), (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT"), "{bad}");
    }
    assert_eq!(stored_account("acc_2").name, "Work");

    // Reorder: a full permutation is applied; anything else means the page is stale.
    let ids = || -> Vec<String> { agy_pool::storage::load_pool().unwrap().accounts.into_iter().map(|a| a.id).collect() };
    assert_eq!(
        post_control(addr, "/api/v1/accounts/reorder", r#"{"ordered_ids":["acc_2","acc_1"]}"#).await.0,
        StatusCode::OK
    );
    assert_eq!(ids(), vec!["acc_2", "acc_1"]);
    for stale in [
        r#"{"ordered_ids":["acc_1"]}"#,
        r#"{"ordered_ids":["acc_1","acc_1"]}"#,
        r#"{"ordered_ids":["acc_1","acc_3"]}"#,
        r#"{"ordered_ids":["acc_1","acc_2","acc_3"]}"#,
    ] {
        let (status, body) = post_control(addr, "/api/v1/accounts/reorder", stale).await;
        assert_eq!((status, error_code(&body).as_str()), (StatusCode::CONFLICT, "POOL_CHANGED"), "{stale}");
    }
    assert_eq!(ids(), vec!["acc_2", "acc_1"]);

    // Without a configured token, controls stay off.
    set_dashboard_config(None, false);
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/disable", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::FORBIDDEN, "TOKEN_REQUIRED_FOR_CONTROL"));

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_switch_account_by_exact_id() {
    let _guard = lock_test().await;
    let tmp = setup_test_env();
    let native = tmp.path().join("native_gemini");
    std::fs::create_dir_all(&native).unwrap();
    agy_pool::config::set_native_agy_dir(&native);
    let expiry = Some(agy_pool::clock::now_unix() + 3600.0);
    let pool = agy_pool::storage::Pool {
        accounts: vec![
            // Named like the other account's id: an exact id must still win.
            agy_pool::storage::Account {
                id: "acc_1".to_string(),
                name: "acc_2".to_string(),
                access_token: "tok1".to_string(),
                token_expiry: expiry,
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_2".to_string(),
                access_token: "tok2".to_string(),
                token_expiry: expiry,
                ..Default::default()
            },
        ],
        active_account_id: Some("acc_1".to_string()),
        strategy: "smart".to_string(),
        ..Default::default()
    };
    agy_pool::storage::save_pool(&pool).unwrap();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/switch", "{}").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        agy_pool::storage::load_pool().unwrap().active_account_id.as_deref(),
        Some("acc_2")
    );
    let (_, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts",
        hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
        Bytes::new(),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["accounts"][0]["is_cli_base"], false);
    assert_eq!(json["accounts"][1]["is_cli_base"], true);

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/switch", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND"));

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_quota_refresh_controls() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    let pool = agy_pool::storage::Pool {
        accounts: vec![
            agy_pool::storage::Account {
                id: "acc_1".to_string(),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_2".to_string(),
                status: agy_pool::storage::STATUS_DISABLED.to_string(),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_3".to_string(),
                status: agy_pool::storage::STATUS_VALIDATION_REQUIRED.to_string(),
                validation_url: Some("https://accounts.google.com/signin/continue?x=1".to_string()),
                ..Default::default()
            },
            agy_pool::storage::Account {
                id: "acc_4".to_string(),
                status: agy_pool::storage::STATUS_VALIDATION_REQUIRED.to_string(),
                validation_url: Some("javascript:alert(1)".to_string()),
                ..Default::default()
            },
        ],
        strategy: "smart".to_string(),
        ..Default::default()
    };
    agy_pool::storage::save_pool(&pool).unwrap();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, body) = post_control(addr, "/api/v1/accounts/acc_2/refresh", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::CONFLICT, "ACCOUNT_DISABLED"));
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_9/refresh", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::NOT_FOUND, "ACCOUNT_NOT_FOUND"));

    // Test mode has no upstream: query_quota fails fast and the error text stays out of the response.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_1/refresh", "{}").await;
    assert_eq!((status, error_code(&body).as_str()), (StatusCode::BAD_GATEWAY, "QUOTA_REFRESH_FAILED"));
    assert!(!String::from_utf8_lossy(&body).contains("TEST GUARD"));

    // A still-restricted account reports its https verification link; other schemes are dropped.
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_3/refresh", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "validation_required");
    assert_eq!(json["validation_url"], "https://accounts.google.com/signin/continue?x=1");
    let (status, body) = post_control(addr, "/api/v1/accounts/acc_4/refresh", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["validation_url"], serde_json::Value::Null);

    // Refresh all skips disabled accounts.
    let (status, body) = post_control(addr, "/api/v1/accounts/refresh-all", "{}").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["refreshed"].as_u64().unwrap() + json["failed"].as_u64().unwrap(), 3);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: the 3 new tests FAIL (e.g. `/api/v1/accounts/acc_1/disable` returns `404 NOT_FOUND`); the 12 Phase 1 tests pass.

- [ ] **Step 4: Create `src/dashboard/control.rs`:**

```rust
//! Account controls for the dashboard API. Accounts are addressed by exact id only: the CLI resolver
//! also matches list positions and name substrings, which would be racy and ambiguous over HTTP.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::{Request, Response, StatusCode};

use super::{api_error, internal_error, json_response};
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
    let body = http_body_util::Limited::new(req.into_body(), 4096).collect().await.ok()?.to_bytes();
    serde_json::from_slice(&body).ok()
}

async fn load_account(id: &str) -> Result<Option<Account>, Response<ProxyBody>> {
    let id = id.to_owned();
    match tokio::task::spawn_blocking(crate::storage::load_pool).await {
        Ok(Ok(pool)) => Ok(pool.accounts.into_iter().find(|a| a.id == id)),
        Ok(Err(e)) => Err(storage_failure("load the account pool", e)),
        Err(_) => Err(internal_error()),
    }
}

/// Applies `f` to the account with exactly this id inside one pool transaction; `false` if it does not exist.
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
    let what = if disabled { "disable the account" } else { "enable the account" };
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
    // Check the exact id first: switch_account falls back to name and substring matches.
    match load_account(id).await {
        Ok(Some(_)) => {}
        Ok(None) => return account_not_found(),
        Err(resp) => return resp,
    }
    match crate::accounts::switch_account(id).await {
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
            let cleared =
                tokio::task::spawn_blocking(move || crate::accounts::persist_cleared_restriction(&before, &acc, &quota))
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
        Err(_) => Err(format!("timed out after {}s (still running)", REFRESH_TIMEOUT.as_secs())),
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

/// Clears REFRESH_ALL_IN_FLIGHT even when the client disconnects and the handler future is dropped.
struct RefreshAllGuard;

impl Drop for RefreshAllGuard {
    fn drop(&mut self) {
        REFRESH_ALL_IN_FLIGHT.store(false, Ordering::Release);
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
    let _guard = RefreshAllGuard;
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
```

- [ ] **Step 5: Route it in `src/dashboard/mod.rs`.** Add `pub mod control;` next to `pub mod auth;`. In `serve`, insert these two arms directly **before** the final `_ => api_error(StatusCode::NOT_FOUND, …)` arm:

```rust
        r if r.starts_with("/api/v1/accounts/") && !is_post => method_not_allowed("POST"),
        r if r.starts_with("/api/v1/accounts/") => match control_gate(&req, &cfg, token) {
            Some(denied) => denied,
            None => {
                let rest = r.trim_start_matches("/api/v1/accounts/").to_owned();
                control::handle(&rest, req).await
            }
        },
```

(`/api/v1/accounts` itself — the GET list — is matched by an earlier arm and is unaffected.)

- [ ] **Step 6: Run the tests**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: 15 passed.

Run: `timeout 900 scripts/test.sh`
Expected: 0 failed in every `test result:` line. If anything outside `tests/dashboard.rs`, `accounts::` or `dashboard::` fails, stop and report the failing test names and output.

- [ ] **Step 7: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/control.rs src/dashboard/mod.rs tests/dashboard.rs
git commit -m "feat(dashboard): add account controls (enable, disable, rename, reorder, refresh, switch)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Frontend — row actions, Refresh all, toast, verification dialog

**Files:**
- Modify: `src/dashboard/assets/index.html`
- Modify: `tests/dashboard.rs` (`test_embedded_spa_asset_constraints`)

**Interfaces:**
- Consumes: the HTTP contract from Task 2; `AccountDto.is_cli_base`, `storage_status` (`"disabled"` when disabled); existing JS `agyFetch`, `fetchAccounts`, `fetchStatus`, `escapeHtml`, `accountsData`, `strategySelect`, and the `s.control_enabled` value inside `checkAuthAndStart`.

- [ ] **Step 1: Extend the asset test** — in `test_embedded_spa_asset_constraints`, insert before `let _ = shutdown_tx.send(());`:

```rust
    // Account controls (Phase 2).
    for needle in [
        "/api/v1/accounts/refresh-all",
        "/api/v1/accounts/reorder",
        "data-act=",
        "refresh-all-btn",
        "verify-dialog",
        "toast",
        "Disable the last enabled account?",
    ] {
        assert!(body_str.contains(needle), "missing {needle}");
    }
    assert!(!body_str.contains("alert("), "use the toast instead of alert()");
```

- [ ] **Step 2: Run to verify it fails**

Run: `timeout 900 scripts/test.sh test --test dashboard test_embedded_spa_asset_constraints`
Expected: FAIL with `missing /api/v1/accounts/refresh-all`.

- [ ] **Step 3: CSS** — insert immediately before `  </style>`:

```css
    .row-actions { display: flex; flex-wrap: wrap; gap: 4px; }
    .row-btn, #refresh-all-btn {
      font: inherit;
      font-size: 11px;
      color: var(--text-main);
      background: var(--bg);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 2px 6px;
      cursor: pointer;
    }
    .row-btn:hover, #refresh-all-btn:hover { border-color: var(--accent-border); }
    .row-btn:disabled, #refresh-all-btn:disabled { opacity: 0.5; cursor: wait; }
    #toast {
      position: fixed;
      right: 16px;
      bottom: 16px;
      z-index: 60;
      max-width: min(420px, 90vw);
      padding: 10px 14px;
      border-radius: 8px;
      border: 1px solid var(--border);
      background: var(--card-bg);
      color: var(--text-main);
      font-size: 13px;
    }
    #toast.error { border-color: var(--red-border); color: var(--red-text); }
    #verify-dialog {
      border: 1px solid var(--border);
      border-radius: 10px;
      background: var(--card-bg);
      color: var(--text-main);
      max-width: min(480px, 90vw);
    }
    #verify-dialog a { color: var(--accent-text); word-break: break-all; }
```

- [ ] **Step 4: Markup.**

Replace

```html
          <span class="pill pill-mono" id="accounts-count-badge">0 accounts</span>
        </div>
```

with

```html
          <span class="pill pill-mono" id="accounts-count-badge">0 accounts</span>
        </div>
        <button id="refresh-all-btn" type="button" hidden>↻ Refresh all</button>
```

In the accounts table header, after `<th>Hits / In-Flight</th>` add:

```html
              <th id="actions-col" hidden>Actions</th>
```

Directly after the closing `</div>` of `#token-card` (the line after `    </form>` / `  </div>`), add:

```html
  <div id="toast" role="status" aria-live="polite" hidden></div>
  <dialog id="verify-dialog">
    <p>Google wants this account verified. Open the link while signed in to that account, then refresh again.</p>
    <p><a id="verify-link" href="#" target="_blank" rel="noopener noreferrer"></a></p>
    <form method="dialog"><button class="row-btn" type="submit">Close</button></form>
  </dialog>
```

- [ ] **Step 5: JS helpers.** Directly after the `async function agyFetch(…) { … }` function, add:

```js
    let toastTimer = null;
    function showToast(message, isError) {
      const el = document.getElementById('toast');
      el.textContent = message;
      el.className = isError ? 'error' : '';
      el.hidden = false;
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => { el.hidden = true; }, isError ? 6000 : 3000);
    }

    // POSTs a control action; returns the parsed body on success, or null after showing the error.
    async function control(path, body) {
      try {
        const res = await agyFetch(path, { method: 'POST', body: JSON.stringify(body || {}) });
        let data = null;
        try { data = await res.json(); } catch (_) {}
        if (res.ok) return data || {};
        if (res.status !== 401 && res.status !== 429) {
          showToast((data && data.error && data.error.message) || `HTTP ${res.status}`, true);
        }
      } catch (err) {
        showToast(`Request failed: ${err}`, true);
      }
      return null;
    }

    function showVerifyLink(url) {
      const link = document.getElementById('verify-link');
      link.href = url;
      link.textContent = url;
      document.getElementById('verify-dialog').showModal();
    }
```

- [ ] **Step 6: Replace `alert` in the strategy handler.** Replace the whole existing block `strategySelect.addEventListener('change', async () => { … });` with:

```js
    strategySelect.addEventListener('change', async () => {
      const wanted = strategySelect.value;
      strategySelect.disabled = true;
      try {
        await control('/api/v1/strategy', { strategy: wanted });
      } finally {
        strategySelect.disabled = false;
        fetchStatus();
      }
    });
```

- [ ] **Step 7: Show controls only when allowed.** In `checkAuthAndStart`, replace

```js
      strategySelect.hidden = !s.control_enabled;
```

with

```js
      controlEnabled = Boolean(s.control_enabled);
      strategySelect.hidden = !controlEnabled;
      document.getElementById('refresh-all-btn').hidden = !controlEnabled;
      document.getElementById('actions-col').hidden = !controlEnabled;
```

and add `let controlEnabled = false;` on the line after `let started = false;`.

- [ ] **Step 8: Render row actions.** In `renderAccountsTable`:
  - change both `colspan="6"` to `colspan="7"` (the empty-state row here and the "Loading accounts..." row in the HTML);
  - change `for (const acc of accountsData) {` to `for (const [i, acc] of accountsData.entries()) {`;
  - replace the line `<span class="acc-name">${escapeHtml(acc.name)}</span>` with:

```js
                <span class="acc-name">${escapeHtml(acc.name)}${acc.is_cli_base ? ' <span class="pill pill-mono" title="Used by the agy CLI">CLI</span>' : ''}</span>
```

  - directly after the `</td>` that closes the Hits / In-Flight cell (before `</tr>`), add:

```js
            ${controlEnabled ? `<td><div class="row-actions">
              ${acc.storage_status === 'disabled'
                ? `<button class="row-btn" data-act="enable" data-id="${escapeHtml(acc.id)}">Enable</button>`
                : `<button class="row-btn" data-act="disable" data-id="${escapeHtml(acc.id)}">Disable</button>
                   <button class="row-btn" data-act="refresh" data-id="${escapeHtml(acc.id)}" title="Refresh quota">↻</button>`}
              <button class="row-btn" data-act="rename" data-id="${escapeHtml(acc.id)}" title="Rename">✎</button>
              <button class="row-btn" data-act="up" data-id="${escapeHtml(acc.id)}" title="Move up" ${i === 0 ? 'disabled' : ''}>↑</button>
              <button class="row-btn" data-act="down" data-id="${escapeHtml(acc.id)}" title="Move down" ${i === accountsData.length - 1 ? 'disabled' : ''}>↓</button>
              ${acc.is_cli_base ? '' : `<button class="row-btn" data-act="switch" data-id="${escapeHtml(acc.id)}" title="Use this account in the agy CLI">Use in CLI</button>`}
            </div></td>` : ''}
```

- [ ] **Step 9: Wire the buttons.** Directly before `function init() {`, add:

```js
    document.getElementById('accounts-table-body').addEventListener('click', async (e) => {
      const btn = e.target.closest('button[data-act]');
      if (!btn || btn.disabled) return;
      const act = btn.dataset.act;
      const id = btn.dataset.id;
      const acc = accountsData.find((a) => a.id === id);
      if (!acc) return;
      const base = `/api/v1/accounts/${encodeURIComponent(id)}`;
      let body = {};
      let path = `${base}/${act}`;

      if (act === 'disable') {
        const enabled = accountsData.filter((a) => a.storage_status !== 'disabled').length;
        if (enabled <= 1 && !confirm('Disable the last enabled account? The gateway will have no account to route to.')) return;
      } else if (act === 'rename') {
        const name = prompt('New name for this account', acc.name);
        if (name === null || name.trim() === '' || name.trim() === acc.name) return;
        body = { name: name.trim() };
      } else if (act === 'switch') {
        if (!confirm(`Use "${acc.name}" for the agy CLI? This replaces the CLI's current login token.`)) return;
      } else if (act === 'up' || act === 'down') {
        const ids = accountsData.map((a) => a.id);
        const i = ids.indexOf(id);
        const j = act === 'up' ? i - 1 : i + 1;
        if (j < 0 || j >= ids.length) return;
        [ids[i], ids[j]] = [ids[j], ids[i]];
        path = '/api/v1/accounts/reorder';
        body = { ordered_ids: ids };
      }

      btn.disabled = true;
      const data = await control(path, body);
      btn.disabled = false;
      if (data) {
        if (act === 'refresh' && data.validation_url) {
          showVerifyLink(data.validation_url);
        } else if (act === 'refresh' && data.status === 'validation_required') {
          showToast('Google still wants this account verified; no link was returned. Use `agy-pool verify`.', true);
        } else {
          showToast('Done.', false);
        }
      }
      fetchAccounts();
      fetchStatus();
    });

    document.getElementById('refresh-all-btn').addEventListener('click', async (e) => {
      const btn = e.currentTarget;
      btn.disabled = true;
      const data = await control('/api/v1/accounts/refresh-all', {});
      btn.disabled = false;
      if (data) showToast(`Refreshed ${data.refreshed}, failed ${data.failed}.`, data.failed > 0);
      fetchAccounts();
    });
```

- [ ] **Step 10: Syntax-check and run the tests**

```bash
python3 -I -c "import re;s=open('src/dashboard/assets/index.html').read();open('/tmp/agy-dash.js','w').write(re.findall(r'<script>(.*?)</script>',s,re.S)[-1])" && node --check /tmp/agy-dash.js && echo JS-OK
grep -n "alert(" src/dashboard/assets/index.html || echo NO-ALERT
timeout 900 scripts/test.sh test --test dashboard
```

Expected: `JS-OK`, `NO-ALERT`, then 15 passed.

- [ ] **Step 11: Commit**

```bash
~/.cargo/bin/cargo fmt
git add src/dashboard/assets/index.html tests/dashboard.rs
git commit -m "feat(dashboard): add account row actions, refresh all, and verification link dialog

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## After all tasks (Claude, not agy)

- Opus review of Task 2 (auth-gated mutations, single-flight handling); Sonnet review of Tasks 1 and 3.
- Manual browser check on a throwaway gateway seeded with copies of fake accounts (never the live pool): every row action, Refresh all, the confirm prompts, toast messages.
- Push, PR, CI.
