# Web Control Plane Phase 1 (Dashboard Token Auth + Strategy Control) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Protect the embedded dashboard API with an optional Bearer token and add the first mutation, `POST /api/v1/strategy`, plus the browser UI for both.

**Architecture:** `Handler::serve` asks `dashboard::owns()` whether a request belongs to the dashboard, and if so hands it to `dashboard::serve()`. That function runs the existing Host/Origin guard, then a read gate (token when configured) or a control gate (control header + JSON + token), then the handler. Proxy requests never enter dashboard code. The token comes from `AGY_POOL_DASHBOARD_TOKEN` or `config.json` `dashboard.auth_token`; wrong tokens feed an in-memory failure limiter keyed by the raw TCP peer address.

**Tech Stack:** Rust 2024, tokio, hyper 1.12 (`http1`), http-body-util, sha2, serde/serde_json; single-file HTML/JS dashboard. No new crates.

**Spec:** `docs/superpowers/specs/2026-11-12-web-control-plane-design.md`. Section **§9 (Revisions)** overrides the rest of the spec; this plan implements §9's Phase 1 only.

## Global Constraints

- Work in `/home/claude/work/agy-pool-rust-cp` on branch `feat/dashboard-auth`. Never touch `/home/claude/work/agy-pool-rust` (it shares the live host).
- Run tests **only** through the sandbox wrapper: `timeout 900 scripts/test.sh <cargo args>` (cargo is `~/.cargo/bin/cargo`). Never run `cargo test` directly: tests can overwrite the live gateway's state.
- Never start, stop or restart the live `agy-pool` gateway; never run release/LTO builds or benchmarks on this host.
- Format with `~/.cargo/bin/cargo fmt` (repo `rustfmt.toml`: `max_width = 120`) before every commit. `timeout 900 scripts/test.sh clippy --all-targets -- -D warnings` must be clean before the final commit of each task.
- No new dependencies in `Cargo.toml`.
- Token rules (spec §9.7): at least `32` characters, visible ASCII only; per-client limit `5` wrong tokens per `60` s, global limit `100` per `60` s, `429` with `Retry-After: 60`; map capped at `1024` clients.
- Env var name: `AGY_POOL_DASHBOARD_TOKEN`; it overrides `dashboard.auth_token`; empty values count as unset.
- Control header: `X-Agy-Pool-Control: 1`. JSON error body shape: `{"error":{"code":"…","message":"…"}}`. All JSON responses carry `Cache-Control: no-store`.
- Peer trust uses only the TCP peer address (`PeerAddr`), never `X-Forwarded-For` or other headers.
- Do not create backup files (`*.bak*`), do not push, do not open PRs, do not change any branch other than `feat/dashboard-auth`. Commit only the files listed in the task. Commit messages end with the line `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Token typed with surrounding whitespace or non-ASCII characters** (copy-paste from a password manager). Browsers cannot send non-ASCII in headers, so the config loader must reject such a token at startup with a clear message, and the UI trims input. Pinned by `dashboard_token_rules` in Task 1.
2. **Authorization header variants:** `bearer` in lowercase, extra spaces, a repeated `Authorization` header, `Basic …`. The lowercase scheme and extra spaces must work; repeated or non-Bearer headers must count as wrong tokens. Pinned by `parse_bearer_variants` in Task 2.
3. **Requests with no `PeerAddr`** (in-process test servers, `agy-pool eval`) must not panic. They share one `0.0.0.0` bucket. Pinned by `peer_ip_defaults_to_unspecified` in Task 2.
4. **`GET /api/v1/auth/status` with a wrong token used as a free guessing oracle.** Wrong tokens there must count against the limiter like everywhere else. Pinned in `test_auth_failures_are_throttled` in Task 4.
5. **Trailing-slash URLs with a query string** (`/api/v1/accounts/?show_email=true`) must keep the query. Pinned by `test_trailing_slash_and_unknown_api_paths` in Task 3.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/config/mod.rs` | `DashboardConfig`, token validation, env-override resolution |
| `src/daemon/lifecycle.rs` | Fail-closed startup on invalid config or env token |
| `src/dashboard/auth.rs` (new) | `PeerAddr`, Bearer parsing, constant-time compare, failure limiter |
| `src/dashboard/mod.rs` | `owns()`, `serve()`, read/control gates, `auth/status`, `strategy` handler |
| `src/proxy/mod.rs` | `Handler::serve` dispatches dashboard requests before the proxy |
| `src/daemon/server.rs` | Inserts `PeerAddr` into every request |
| `src/dashboard/assets/index.html` | Token prompt, `agyFetch`, fetch-stream SSE, strategy selector |
| `tests/dashboard.rs`, `tests/daemon_child.rs` | Integration tests |
| `README.md` | Dashboard token docs and rollback note |

---

### Task 1: Dashboard config, token rules, fail-closed startup

**Files:**
- Modify: `src/config/mod.rs` (struct `StaticConfig` ~line 225, `default_static_config` ~line 282, `load_config_file` ~line 364, tests module ~line 551)
- Modify: `src/daemon/lifecycle.rs` (`apply_instance_static_config` ~line 55-70 and its two callers ~line 81 and ~line 290)
- Modify: `tests/daemon_child.rs` (`test_daemon_child_reports_invalid_config` ~line 337)
- Modify: `README.md` (Configuration section, after the `logging.audit_max_size_bytes` bullet and in the env var table)

**Interfaces:**
- Produces (used by Tasks 3-4):
  - `pub struct DashboardConfig { pub auth_token: Option<String>, pub allow_unauthenticated_control: bool }` (derives `Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize`)
  - `StaticConfig.dashboard: DashboardConfig`
  - `pub const ENV_DASHBOARD_TOKEN: &str = "AGY_POOL_DASHBOARD_TOKEN";`
  - `pub const MIN_DASHBOARD_TOKEN_LEN: usize = 32;`
  - `pub fn check_dashboard_token(token: &str) -> Result<(), String>`
  - `pub fn resolve_dashboard_token(env_value: Option<String>, cfg: &DashboardConfig) -> Option<String>`
  - `pub fn dashboard_token(cfg: &DashboardConfig) -> Option<String>`

- [ ] **Step 1: Write the failing unit tests** — append inside `mod tests` in `src/config/mod.rs`:

```rust
    #[test]
    fn dashboard_config_parses_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dash.json");
        std::fs::write(
            &p,
            r#"{"version":1,"dashboard":{"auth_token":"0123456789abcdef0123456789abcdef","allow_unauthenticated_control":true}}"#,
        )
        .unwrap();
        let cfg = load_config_file(&p).unwrap();
        assert_eq!(
            cfg.dashboard.auth_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(cfg.dashboard.allow_unauthenticated_control);

        std::fs::write(&p, r#"{"version":1}"#).unwrap();
        assert_eq!(load_config_file(&p).unwrap().dashboard, DashboardConfig::default());
        assert_eq!(default_static_config().dashboard, DashboardConfig::default());

        std::fs::write(&p, r#"{"version":1,"dashboard":{"token":"x"}}"#).unwrap();
        assert!(load_config_file(&p).is_err(), "unknown dashboard keys must be rejected");
    }

    #[test]
    fn dashboard_token_rules() {
        assert!(check_dashboard_token("0123456789abcdef0123456789abcdef").is_ok());
        assert!(check_dashboard_token("short").is_err());
        assert!(check_dashboard_token(" 0123456789abcdef0123456789abcdef").is_err());
        assert!(check_dashboard_token("0123456789abcdef0123456789abcdé").is_err());

        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("dash.json");
        std::fs::write(&p, r#"{"version":1,"dashboard":{"auth_token":"too-short"}}"#).unwrap();
        let err = load_config_file(&p).unwrap_err().to_string();
        assert!(err.contains("dashboard.auth_token"), "error should name the key: {err}");

        // An empty token means "no token", not an error.
        std::fs::write(&p, r#"{"version":1,"dashboard":{"auth_token":""}}"#).unwrap();
        assert!(load_config_file(&p).is_ok());
    }

    #[test]
    fn dashboard_token_env_overrides_config() {
        let cfg = DashboardConfig {
            auth_token: Some("from-config".to_string()),
            allow_unauthenticated_control: false,
        };
        assert_eq!(
            resolve_dashboard_token(Some("from-env".to_string()), &cfg).as_deref(),
            Some("from-env")
        );
        assert_eq!(resolve_dashboard_token(Some(String::new()), &cfg).as_deref(), Some("from-config"));
        assert_eq!(resolve_dashboard_token(None, &cfg).as_deref(), Some("from-config"));
        let empty = DashboardConfig {
            auth_token: Some(String::new()),
            allow_unauthenticated_control: false,
        };
        assert_eq!(resolve_dashboard_token(None, &empty), None);
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `timeout 900 scripts/test.sh test --lib config::tests::dashboard`
Expected: compile errors (`DashboardConfig`, `check_dashboard_token`, `resolve_dashboard_token` not found).

- [ ] **Step 3: Implement the config pieces** in `src/config/mod.rs`.

Add the field as the last field of `StaticConfig`:

```rust
    #[serde(default)]
    pub dashboard: DashboardConfig,
```

Add after the `StaticConfig` struct:

```rust
pub const ENV_DASHBOARD_TOKEN: &str = "AGY_POOL_DASHBOARD_TOKEN";
pub const MIN_DASHBOARD_TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Lets dashboard mutations run without a token. Only for networks where every peer is trusted.
    #[serde(default)]
    pub allow_unauthenticated_control: bool,
}

/// Browsers can only send visible ASCII in a header, and short tokens are guessable.
pub fn check_dashboard_token(token: &str) -> Result<(), String> {
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("token must contain only visible ASCII characters (no spaces)".to_string());
    }
    if token.len() < MIN_DASHBOARD_TOKEN_LEN {
        return Err(format!(
            "token must be at least {MIN_DASHBOARD_TOKEN_LEN} characters (generate one with `openssl rand -hex 32`)"
        ));
    }
    Ok(())
}

/// The env var wins over config.json; empty values count as unset.
pub fn resolve_dashboard_token(env_value: Option<String>, cfg: &DashboardConfig) -> Option<String> {
    env_value
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.auth_token.clone().filter(|s| !s.is_empty()))
}

pub fn dashboard_token(cfg: &DashboardConfig) -> Option<String> {
    resolve_dashboard_token(std::env::var(ENV_DASHBOARD_TOKEN).ok(), cfg)
}
```

In `default_static_config()`, add after `model_mapping: …`:

```rust
        dashboard: DashboardConfig::default(),
```

In `load_config_file`, insert immediately after the `logging.backup_count` check (before the `// 4. Resolve relative native_agy.binary` comment):

```rust
    if let Some(token) = cfg.dashboard.auth_token.as_deref()
        && !token.is_empty()
        && let Err(e) = check_dashboard_token(token)
    {
        return Err(ConfigError::Validation(format!("invalid dashboard.auth_token: {e}")));
    }
```

Then run `grep -rn "StaticConfig {" src tests` and add `dashboard: DashboardConfig::default()` (or `crate::config::DashboardConfig::default()`) to any other struct literal that fails to compile. Do not add `..Default::default()`.

- [ ] **Step 4: Run the unit tests**

Run: `timeout 900 scripts/test.sh test --lib config::`
Expected: all `config::` tests pass, including the three new ones.

- [ ] **Step 5: Change the daemon test to expect a refused start** — replace the whole `test_daemon_child_reports_invalid_config` function in `tests/daemon_child.rs` with:

```rust
#[test]
fn test_daemon_child_refuses_invalid_config() {
    let rust_bin = env!("CARGO_BIN_EXE_agy-pool");
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let port = get_ephemeral_port();
    let cfg_file = tmp_dir.path().join("config.json");
    // Typo'd key: deny_unknown_fields rejects the whole file. Falling back to defaults could
    // silently drop dashboard.auth_token, so the daemon must refuse to start instead.
    std::fs::write(
        &cfg_file,
        r#"{"version": 1, "server": {"keep_alive_sec": 10}, "model_mapping": {"a": "b"}}"#,
    )
    .unwrap();

    let ctx = agy_pool::daemon::resolve_instance_context(Some(&data_dir), Some(&cfg_file), Some(port), None)
        .expect("resolve_instance_context");
    let res = agy_pool::daemon::start_instance(
        &ctx,
        agy_pool::daemon::LaunchOptions {
            entrypoint_path: Some(PathBuf::from(rust_bin)),
            ..Default::default()
        },
    );
    let log = std::fs::read_to_string(ctx.log_file()).unwrap_or_default();
    assert!(res.is_err(), "daemon must refuse an invalid config; log:\n{log}");
    assert!(
        log.contains("invalid config"),
        "daemon must name the rejected config file; log:\n{log}"
    );
    assert!(!wait_for_healthz(port, Duration::from_millis(500)), "no gateway may be listening");
}
```

- [ ] **Step 6: Make startup fail closed** — in `src/daemon/lifecycle.rs` replace the doc comment and body of `apply_instance_static_config` with:

```rust
/// Makes get_static_config() (model_mapping, keep_alive_secs, native_agy.binary, dashboard) return this
/// instance's config. Fails closed: falling back to defaults could silently drop the dashboard token.
fn apply_instance_static_config(ctx: &super::instance::InstanceContext) -> Result<(), crate::config::ConfigError> {
    use crate::config::{ConfigError, ENV_DASHBOARD_TOKEN};
    let cfg = match ctx.config_path.as_deref() {
        Some(p) => crate::config::load_config_file(p)
            .map_err(|e| ConfigError::Validation(format!("invalid config {}: {e}", p.display())))?,
        None => crate::config::default_static_config(),
    };
    if let Ok(token) = std::env::var(ENV_DASHBOARD_TOKEN)
        && !token.is_empty()
    {
        crate::config::check_dashboard_token(&token)
            .map_err(|e| ConfigError::Validation(format!("invalid {ENV_DASHBOARD_TOKEN}: {e}")))?;
    }
    crate::config::set_static_config(cfg);
    Ok(())
}
```

Replace the foreground caller (currently `if let Some(e) = apply_instance_static_config(ctx) { eprintln!("warning: {e}"); }`) with:

```rust
        apply_instance_static_config(ctx).map_err(DaemonError::Config)?;
```

(`DaemonError` is `super::DaemonError`; use whatever path the file already uses for it.)

Replace the daemon-child caller (currently `if let Some(e) = apply_instance_static_config(&ctx) { … "daemon child warning: {e}" … }`) with:

```rust
    if let Err(e) = apply_instance_static_config(&ctx) {
        let _ = writeln!(stderr, "daemon child error: {e}");
        return 1;
    }
```

- [ ] **Step 7: Run the daemon tests**

Run: `timeout 900 scripts/test.sh test --test daemon_child`
Expected: all pass, including `test_daemon_child_refuses_invalid_config` and the unchanged `test_daemon_child_applies_config_model_mapping`.

- [ ] **Step 8: Document it in `README.md`**. After the `- \`logging.audit_max_size_bytes\`: …` bullet add:

```markdown
- `dashboard.auth_token`: Token required by the web dashboard API (at least 32 visible ASCII characters; generate one with `openssl rand -hex 32`). When set, every `/api/v1/*` call needs `Authorization: Bearer <token>`; the dashboard page asks for it once per browser tab. When unset, the dashboard stays read-only.
- `dashboard.allow_unauthenticated_control`: Allow dashboard changes (such as switching strategy) without a token. Off by default; only enable it when every host that can reach the listen address is trusted.

The gateway refuses to start if `config.json` or `AGY_POOL_DASHBOARD_TOKEN` is invalid, so a typo cannot silently turn the token off.

**Downgrading:** agy-pool 0.3.5 and older reject unknown config keys. Remove the `dashboard` block from `config.json` before installing an older release.
```

In the environment variable table add the row:

```markdown
| `AGY_POOL_DASHBOARD_TOKEN` | Dashboard API token; overrides `dashboard.auth_token` |
```

- [ ] **Step 9: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/config/mod.rs src/daemon/lifecycle.rs tests/daemon_child.rs README.md
git commit -m "feat(config): add dashboard token settings and refuse invalid config at startup

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

(If Step 3 required edits to other files with `StaticConfig { … }` literals, add those files too.)

---

### Task 2: `dashboard::auth` — Bearer parsing, constant-time compare, failure limiter

**Files:**
- Create: `src/dashboard/auth.rs`
- Modify: `src/dashboard/mod.rs` (top: module declarations)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces (used by Tasks 3-4):
  - `pub struct PeerAddr(pub std::net::SocketAddr)` (`Debug, Clone, Copy`), re-exported as `crate::dashboard::PeerAddr`
  - `pub enum BearerCheck { Ok, Missing, Invalid, Throttled }` (`Debug, PartialEq, Eq`)
  - `pub fn check_bearer(headers: &hyper::HeaderMap, peer: std::net::IpAddr, expected: &str) -> BearerCheck`
  - `pub fn peer_ip(extensions: &hyper::http::Extensions) -> std::net::IpAddr`
  - `pub fn reset_auth_failures()` (test hook)

- [ ] **Step 1: Create `src/dashboard/auth.rs` with the tests first** (the implementation section below the tests is added in Step 3; for Step 2 the file contains only the `#[cfg(test)] mod tests` block):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn auth_headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(hyper::header::AUTHORIZATION, v.parse().unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn limiter(now: Instant) -> FailureLimiter {
        FailureLimiter::new(FAILURE_WINDOW, PER_IP_FAILURE_LIMIT, GLOBAL_FAILURE_LIMIT, now)
    }

    #[test]
    fn token_matches_only_exact_token() {
        assert!(token_matches(TOKEN, TOKEN));
        assert!(!token_matches("0123456789abcdef0123456789abcdee", TOKEN));
        assert!(!token_matches("0123", TOKEN));
        assert!(!token_matches("", TOKEN));
    }

    #[test]
    fn parse_bearer_variants() {
        assert_eq!(parse_bearer(&HeaderMap::new()), Ok(None));
        assert_eq!(parse_bearer(&auth_headers(&["Bearer abc"])), Ok(Some("abc")));
        assert_eq!(parse_bearer(&auth_headers(&["bearer   abc  "])), Ok(Some("abc")));
        assert_eq!(parse_bearer(&auth_headers(&["BEARER abc"])), Ok(Some("abc")));
        assert!(parse_bearer(&auth_headers(&["Basic abc"])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer"])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer   "])).is_err());
        assert!(parse_bearer(&auth_headers(&["Bearer abc", "Bearer abc"])).is_err());
    }

    #[test]
    fn peer_ip_defaults_to_unspecified() {
        let mut ext = hyper::http::Extensions::new();
        assert_eq!(peer_ip(&ext), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        ext.insert(PeerAddr(SocketAddr::from(([100, 64, 0, 1], 5000))));
        assert_eq!(peer_ip(&ext), ip("100.64.0.1"));
    }

    #[test]
    fn per_ip_limit_blocks_then_expires() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            assert!(!l.is_blocked(ip("10.0.0.1"), t0));
            l.record_failure(ip("10.0.0.1"), t0);
        }
        assert!(l.is_blocked(ip("10.0.0.1"), t0));
        assert!(!l.is_blocked(ip("10.0.0.2"), t0), "other clients are unaffected");
        assert!(!l.is_blocked(ip("10.0.0.1"), t0 + FAILURE_WINDOW), "block ends with the window");
    }

    #[test]
    fn ipv6_counts_per_64_and_mapped_v4_matches_v4() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for i in 0..PER_IP_FAILURE_LIMIT {
            l.record_failure(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, i as u16 + 1)), t0);
        }
        assert!(l.is_blocked(ip("2001:db8:1:2::ffff"), t0));
        assert!(!l.is_blocked(ip("2001:db8:1:3::1"), t0));

        let mut l = limiter(t0);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            l.record_failure(ip("::ffff:192.0.2.7"), t0);
        }
        assert!(l.is_blocked(ip("192.0.2.7"), t0));
    }

    #[test]
    fn global_limit_blocks_everyone() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        for i in 0..GLOBAL_FAILURE_LIMIT {
            l.record_failure(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), t0);
        }
        assert!(l.is_blocked(ip("192.0.2.1"), t0));
        assert!(!l.is_blocked(ip("192.0.2.1"), t0 + FAILURE_WINDOW));
    }

    #[test]
    fn tracked_clients_are_capped() {
        let t0 = Instant::now();
        let mut l = FailureLimiter::new(FAILURE_WINDOW, PER_IP_FAILURE_LIMIT, u32::MAX, t0);
        for i in 0..(MAX_TRACKED_IPS as u32 + 50) {
            l.record_failure(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)), t0);
        }
        assert!(l.per_ip.len() <= MAX_TRACKED_IPS);
        // Expired entries are pruned to make room.
        l.record_failure(ip("192.0.2.1"), t0 + FAILURE_WINDOW);
        assert!(l.per_ip.contains_key(&ip("192.0.2.1")));
    }

    #[test]
    fn check_bearer_counts_only_wrong_tokens() {
        let t0 = Instant::now();
        let mut l = limiter(t0);
        let peer = ip("127.0.0.1");
        for _ in 0..(PER_IP_FAILURE_LIMIT * 2) {
            assert_eq!(check_bearer_with(&mut l, &HeaderMap::new(), peer, TOKEN, t0), BearerCheck::Missing);
        }
        let good_value = format!("Bearer {TOKEN}");
        let good = auth_headers(&[good_value.as_str()]);
        assert_eq!(check_bearer_with(&mut l, &good, peer, TOKEN, t0), BearerCheck::Ok);

        let bad = auth_headers(&["Bearer wrong"]);
        for _ in 0..PER_IP_FAILURE_LIMIT {
            assert_eq!(check_bearer_with(&mut l, &bad, peer, TOKEN, t0), BearerCheck::Invalid);
        }
        assert_eq!(check_bearer_with(&mut l, &good, peer, TOKEN, t0), BearerCheck::Throttled);
        let repeated = auth_headers(&[good_value.as_str(), good_value.as_str()]);
        assert_eq!(
            check_bearer_with(&mut l, &repeated, ip("127.0.0.2"), TOKEN, t0),
            BearerCheck::Invalid
        );
    }
}
```

Add to the top of `src/dashboard/mod.rs`, next to `pub mod dto;`:

```rust
pub mod auth;
pub use auth::PeerAddr;
```

- [ ] **Step 2: Run to verify it fails**

Run: `timeout 900 scripts/test.sh test --lib dashboard::auth`
Expected: compile errors (`token_matches`, `FailureLimiter`, … not found).

- [ ] **Step 3: Add the implementation** at the top of `src/dashboard/auth.rs`, above the tests:

```rust
//! Bearer-token checks for the dashboard API: constant-time compare and a wrong-token limiter.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use hyper::HeaderMap;

/// TCP peer of the connection, inserted by the accept loop. Never derived from request headers.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
pub const PER_IP_FAILURE_LIMIT: u32 = 5;
pub const GLOBAL_FAILURE_LIMIT: u32 = 100;
const MAX_TRACKED_IPS: usize = 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum BearerCheck {
    Ok,
    Missing,
    Invalid,
    Throttled,
}

/// Compares SHA-256 digests so timing reveals neither the token length nor a matching prefix.
fn token_matches(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let a = Sha256::digest(presented.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Ok(None): no Authorization header. Err: repeated, non-Bearer or empty.
fn parse_bearer(headers: &HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(hyper::header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let (scheme, token) = value.to_str().map_err(|_| ())?.split_once(' ').ok_or(())?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(());
    }
    Ok(Some(token))
}

/// IPv6 clients usually control a whole /64, so failures are counted per /64.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Fixed-window wrong-token counts per client and overall. The global budget stops guessing
/// spread over many addresses, at the cost of locking everyone out for up to one window.
pub struct FailureLimiter {
    window: Duration,
    per_ip_limit: u32,
    global_limit: u32,
    global: (Instant, u32),
    per_ip: HashMap<IpAddr, (Instant, u32)>,
}

impl FailureLimiter {
    pub fn new(window: Duration, per_ip_limit: u32, global_limit: u32, now: Instant) -> Self {
        Self {
            window,
            per_ip_limit,
            global_limit,
            global: (now, 0),
            per_ip: HashMap::new(),
        }
    }

    pub fn is_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let exhausted =
            |&(start, count): &(Instant, u32), limit: u32| now.duration_since(start) < self.window && count >= limit;
        exhausted(&self.global, self.global_limit)
            || self
                .per_ip
                .get(&client_key(ip))
                .is_some_and(|e| exhausted(e, self.per_ip_limit))
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        let window = self.window;
        let bump = |entry: &mut (Instant, u32)| {
            if now.duration_since(entry.0) >= window {
                *entry = (now, 0);
            }
            entry.1 = entry.1.saturating_add(1);
        };
        bump(&mut self.global);
        let key = client_key(ip);
        if !self.per_ip.contains_key(&key) && self.per_ip.len() >= MAX_TRACKED_IPS {
            self.per_ip.retain(|_, e| now.duration_since(e.0) < window);
            if self.per_ip.len() >= MAX_TRACKED_IPS {
                return; // still counted by the global budget
            }
        }
        bump(self.per_ip.entry(key).or_insert((now, 0)));
    }
}

static LIMITER: LazyLock<Mutex<FailureLimiter>> = LazyLock::new(|| Mutex::new(new_limiter()));

fn new_limiter() -> FailureLimiter {
    FailureLimiter::new(FAILURE_WINDOW, PER_IP_FAILURE_LIMIT, GLOBAL_FAILURE_LIMIT, Instant::now())
}

/// Test hook: forget every recorded failure.
pub fn reset_auth_failures() {
    *LIMITER.lock().unwrap_or_else(|e| e.into_inner()) = new_limiter();
}

/// Requests served without the production accept loop carry no PeerAddr; they share one bucket.
pub fn peer_ip(extensions: &hyper::http::Extensions) -> IpAddr {
    extensions
        .get::<PeerAddr>()
        .map(|p| p.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

/// Checks the request's Bearer token; wrong tokens count against the limiter, missing ones do not.
pub fn check_bearer(headers: &HeaderMap, peer: IpAddr, expected: &str) -> BearerCheck {
    let mut limiter = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    check_bearer_with(&mut limiter, headers, peer, expected, Instant::now())
}

fn check_bearer_with(
    limiter: &mut FailureLimiter,
    headers: &HeaderMap,
    peer: IpAddr,
    expected: &str,
    now: Instant,
) -> BearerCheck {
    if limiter.is_blocked(peer, now) {
        return BearerCheck::Throttled;
    }
    match parse_bearer(headers) {
        Ok(None) => BearerCheck::Missing,
        Ok(Some(token)) if token_matches(token, expected) => BearerCheck::Ok,
        _ => {
            limiter.record_failure(peer, now);
            BearerCheck::Invalid
        }
    }
}
```

If clippy reports an unused import, remove only that import.

- [ ] **Step 4: Run the tests**

Run: `timeout 900 scripts/test.sh test --lib dashboard::auth`
Expected: 8 tests pass.

- [ ] **Step 5: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/auth.rs src/dashboard/mod.rs
git commit -m "feat(dashboard): add bearer token check and wrong-token limiter

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Dashboard dispatch via `owns()`/`serve()` and `PeerAddr` wiring

Behavior must stay identical to 0.3.5 except that every `/api/v1/*` path is now answered by the dashboard (unknown ones return a JSON `404` instead of being proxied upstream).

**Files:**
- Modify: `src/dashboard/mod.rs` (replace `try_handle_request` ~lines 243-397 and `method_not_allowed` ~lines 45-55)
- Modify: `src/proxy/mod.rs` (`Handler::serve` ~line 862 and the first lines of `handle_proxy` ~line 866-869)
- Modify: `src/daemon/server.rs` (accept loop ~lines 593-608)
- Modify: `tests/dashboard.rs` (`start_server` ~line 61-99, new test)

**Interfaces:**
- Consumes: `crate::dashboard::PeerAddr` (Task 2).
- Produces (used by Task 4):
  - `pub fn owns(method: &Method, path: &str) -> bool`
  - `pub async fn serve(req: Request<hyper::body::Incoming>) -> Response<ProxyBody>`
  - private helpers `api_error(status: StatusCode, code: &str, message: &str) -> Response<ProxyBody>`, `method_not_allowed(allow: &'static str)`, `html_response()`, `status_response()`, `accounts_response(query: Option<&str>)`, `forecast_response()`, `events_response()`

- [ ] **Step 1: Write the failing integration test** — append to `tests/dashboard.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_trailing_slash_and_unknown_api_paths() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let mut pool = agy_pool::storage::load_pool().unwrap();
    pool.show_email = Some(true);
    agy_pool::storage::save_pool(&pool).unwrap();
    let (addr, server_task, shutdown_tx) = start_server().await;

    // A trailing slash must not drop the query string.
    let (status, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/accounts/?show_email=true",
        HeaderMap::new(),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["accounts"][0]["email"], "alice@example.com");

    // Unknown /api/v1/* paths are answered locally (JSON 404), never proxied upstream.
    for (method, path) in [(Method::GET, "/api/v1/nope"), (Method::POST, "/api/v1/accounts/acc_1/enable")] {
        let (status, headers, body) = send_request(addr, method, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(headers.get(hyper::header::CONTENT_TYPE).unwrap(), "application/json");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "NOT_FOUND", "{path}");
    }

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `timeout 900 scripts/test.sh test --test dashboard test_trailing_slash_and_unknown_api_paths`
Expected: FAIL — `/api/v1/nope` returns `503` (proxied to the empty pool) instead of `404`.

- [ ] **Step 3: Replace the dispatch in `src/dashboard/mod.rs`.**

Replace `fn method_not_allowed() -> Response<ProxyBody> { … }` with:

```rust
fn method_not_allowed(allow: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(b"Method Not Allowed\n")));
    *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    resp.headers_mut()
        .insert(hyper::header::ALLOW, hyper::header::HeaderValue::from_static(allow));
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}
```

Add after `json_response`:

```rust
fn api_error(status: StatusCode, code: &str, message: &str) -> Response<ProxyBody> {
    let body = serde_json::json!({ "error": { "code": code, "message": message } });
    let mut resp = json_response(serde_json::to_vec(&body).unwrap_or_default());
    *resp.status_mut() = status;
    resp
}
```

Replace the whole `pub async fn try_handle_request(…) { … }` function with the code below. The bodies of `status_response`, `accounts_response`, `forecast_response` and `events_response` are the existing branch bodies moved verbatim, with `return Some(x)` turned into `x` / `return x`:

```rust
/// Requests the dashboard answers; everything else goes to the proxy. `POST /` stays a proxy call.
pub fn owns(method: &Method, path: &str) -> bool {
    (path == "/" && method == Method::GET) || matches!(path, "/dashboard" | "/dashboard/") || path.starts_with("/api/v1/")
}

pub async fn serve(req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
    let path = req.uri().path();
    // `/dashboard/` and `/api/v1/status/` keep working as in 0.3.x.
    let route = match path.strip_suffix('/') {
        Some(p) if !p.is_empty() => p,
        _ => path,
    }
    .to_owned();
    let is_html = matches!(route.as_str(), "/" | "/dashboard");
    if !is_valid_host_and_origin(&req, is_html) {
        return forbidden();
    }

    let is_get = req.method() == Method::GET;
    match route.as_str() {
        "/" | "/dashboard" if is_get => html_response(),
        "/dashboard" | "/api/v1/status" | "/api/v1/accounts" | "/api/v1/forecast" | "/api/v1/events" if !is_get => {
            method_not_allowed("GET")
        }
        "/api/v1/status" => status_response().await,
        "/api/v1/accounts" => accounts_response(req.uri().query()).await,
        "/api/v1/forecast" => forecast_response().await,
        "/api/v1/events" => events_response(),
        _ => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "unknown dashboard API path"),
    }
}

fn html_response() -> Response<ProxyBody> {
    let mut resp = Response::new(full_body(Bytes::from_static(dashboard_html().as_bytes())));
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

async fn status_response() -> Response<ProxyBody> {
    let Ok(status) = tokio::task::spawn_blocking(dto::build_status_response).await else {
        return internal_error();
    };
    json_response(serde_json::to_vec(&status).unwrap_or_default())
}

async fn accounts_response(query: Option<&str>) -> Response<ProxyBody> {
    let request_show_email = query
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "show_email" && (v == "true" || v == "1")))
        .unwrap_or(false);
    let Ok(accounts) = tokio::task::spawn_blocking(move || {
        let pool = crate::storage::load_pool().unwrap_or_default();
        let now_ts = jiff::Timestamp::now().as_second() as f64;
        dto::build_accounts_response(&pool, request_show_email, now_ts)
    })
    .await
    else {
        return internal_error();
    };
    json_response(serde_json::to_vec(&accounts).unwrap_or_default())
}

async fn forecast_response() -> Response<ProxyBody> {
    let Ok(forecast) = tokio::task::spawn_blocking(|| {
        let pool = crate::storage::load_pool().unwrap_or_default();
        let now_ts = jiff::Timestamp::now().as_second() as f64;
        dto::build_forecast_response(&pool, now_ts)
    })
    .await
    else {
        return internal_error();
    };
    json_response(serde_json::to_vec(&forecast).unwrap_or_default())
}

fn events_response() -> Response<ProxyBody> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let (recent, mut audit_rx) = telemetry::subscribe_with_replay(20);

    tokio::spawn(async move {
        let last_replayed_id = recent.iter().map(|e| e.id).max().unwrap_or(0);
        for ev in recent {
            if let Ok(data) = serde_json::to_string(&ev) {
                let msg = format!("id: {}\nevent: audit\ndata: {data}\n\n", ev.id);
                if tx.send(Bytes::from(msg)).await.is_err() {
                    return;
                }
            }
        }

        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(2),
            Duration::from_secs(2),
        );

        loop {
            tokio::select! {
                _ = tx.closed() => {
                    break;
                }
                recv_res = audit_rx.recv() => {
                    match recv_res {
                        Ok(ev) => {
                            if ev.id <= last_replayed_id {
                                continue;
                            }
                            if let Ok(data) = serde_json::to_string(&ev) {
                                let msg = format!("id: {}\nevent: audit\ndata: {data}\n\n", ev.id);
                                if tx.send(Bytes::from(msg)).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let stats = dto::build_sse_stats_event();
                    if let Ok(data) = serde_json::to_string(&stats) {
                        let msg = format!("event: stats\ndata: {data}\n\n");
                        if tx.send(Bytes::from(msg)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let mut resp = Response::new(SseBody::new(rx).boxed());
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    resp.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    resp.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("keep-alive"),
    );
    resp
}
```

(`events_response` is the old `is_events` branch moved verbatim, with `return Some(resp);` turned into `resp`.)

- [ ] **Step 4: Dispatch from `Handler::serve`** in `src/proxy/mod.rs`. Replace:

```rust
    pub async fn serve(&self, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
        self.handle_proxy(req).await
    }
```

with:

```rust
    pub async fn serve(&self, req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
        if crate::dashboard::owns(req.method(), req.uri().path()) {
            return crate::dashboard::serve(req).await;
        }
        self.handle_proxy(req).await
    }
```

and delete these three lines at the start of `handle_proxy`:

```rust
        if let Some(resp) = crate::dashboard::try_handle_request(&req).await {
            return resp;
        }
```

Run `grep -rn try_handle_request src tests`; it must print nothing.

- [ ] **Step 5: Insert `PeerAddr` in the production accept loop** — in `src/daemon/server.rs` change `let (stream, _) = match listener.accept().await {` to `let (stream, peer) = match listener.accept().await {`, and replace the `service_fn` closure with:

```rust
                    let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                        req.extensions_mut().insert(crate::dashboard::PeerAddr(peer));
                        let h = Arc::clone(&h);
                        async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                    });
```

- [ ] **Step 6: Mirror it in the test harness** — in `tests/dashboard.rs` `start_server`, change `if let Ok((stream, _)) = accept_res {` to `if let Ok((stream, peer)) = accept_res {` and the closure to:

```rust
                        let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                            req.extensions_mut().insert(agy_pool::dashboard::PeerAddr(peer));
                            let h = h.clone();
                            async move { Ok::<_, std::convert::Infallible>(h.serve(req).await) }
                        });
```

- [ ] **Step 7: Run the dashboard and proxy tests**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: all 8 tests pass (7 existing, unchanged, plus `test_trailing_slash_and_unknown_api_paths`).

Run: `timeout 900 scripts/test.sh test --lib proxy::`
Expected: all pass.

- [ ] **Step 8: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/mod.rs src/proxy/mod.rs src/daemon/server.rs tests/dashboard.rs
git commit -m "refactor(dashboard): dispatch dashboard requests before the proxy and record the TCP peer

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Auth gates, `GET /api/v1/auth/status`, `POST /api/v1/strategy`

**Files:**
- Modify: `src/dashboard/mod.rs` (`serve` from Task 3; new gate and handler functions)
- Modify: `tests/dashboard.rs` (guard/lock helpers, new tests)

**Interfaces:**
- Consumes: `crate::config::{get_static_config, dashboard_token, DashboardConfig, STRATEGY_SMART, STRATEGY_MAX_QUOTA}` (Task 1), `auth::{check_bearer, peer_ip, BearerCheck, reset_auth_failures}` (Task 2), `serve`, `api_error`, `json_response`, `method_not_allowed` (Task 3), `crate::storage::pool_transaction`.
- Produces (used by Task 5) — HTTP contract:
  - `GET /api/v1/auth/status` → `200 {"auth_required": bool, "authenticated": bool, "control_enabled": bool}`; `429` when throttled. `authenticated` is `true` when no token is configured.
  - `POST /api/v1/strategy` body `{"strategy":"smart"|"max_quota"}` → `200 {"ok":true,"strategy":"…"}`; errors `400 INVALID_ARGUMENT`, `401 UNAUTHORIZED` (+`WWW-Authenticate: Bearer`), `403 CONTROL_HEADER_REQUIRED`, `403 TOKEN_REQUIRED_FOR_CONTROL`, `405` (`Allow: POST`), `415 JSON_REQUIRED`, `429 TOO_MANY_AUTH_FAILURES` (+`Retry-After: 60`).
  - With a token configured, `GET /api/v1/{status,accounts,forecast,events}` → `401` without a valid Bearer token.

- [ ] **Step 1: Extend the test harness** in `tests/dashboard.rs`.

In `impl Drop for DashboardTestGuard`, add as the first line of `drop`:

```rust
        agy_pool::config::set_static_config(agy_pool::config::default_static_config());
```

In `lock_test`, after `agy_pool::config::set_test_mode(true);` add:

```rust
    agy_pool::dashboard::auth::reset_auth_failures();
```

Add helpers after `send_request`:

```rust
const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn set_dashboard_config(token: Option<&str>, allow_unauthenticated_control: bool) {
    let mut cfg = agy_pool::config::default_static_config();
    cfg.dashboard.auth_token = token.map(str::to_string);
    cfg.dashboard.allow_unauthenticated_control = allow_unauthenticated_control;
    agy_pool::config::set_static_config(cfg);
}

fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(*k, v.parse().unwrap());
    }
    h
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn error_code(body: &Bytes) -> String {
    let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    json["error"]["code"].as_str().unwrap_or_default().to_string()
}

async fn post_strategy(addr: SocketAddr, headers: HeaderMap, body: &'static str) -> (StatusCode, HeaderMap, Bytes) {
    send_request(addr, Method::POST, "/api/v1/strategy", headers, Bytes::from_static(body.as_bytes())).await
}
```

- [ ] **Step 2: Write the failing tests** — append to `tests/dashboard.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_reads_without_token_stay_open() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;

    let (status, _, _) = send_request(addr, Method::GET, "/api/v1/status", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) =
        send_request(addr, Method::GET, "/api/v1/auth/status", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"auth_required": false, "authenticated": true, "control_enabled": false})
    );

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reads_require_token_when_configured() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;

    // The HTML shell carries no account data and stays reachable so the browser can ask for the token.
    let (status, _, _) = send_request(addr, Method::GET, "/dashboard", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);

    for path in ["/api/v1/status", "/api/v1/accounts", "/api/v1/forecast", "/api/v1/events"] {
        let (status, headers, body) = send_request(addr, Method::GET, path, HeaderMap::new(), Bytes::new()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(headers.get(hyper::header::WWW_AUTHENTICATE).unwrap(), "Bearer", "{path}");
        assert_eq!(error_code(&body), "UNAUTHORIZED", "{path}");
    }
    for path in ["/api/v1/status", "/api/v1/accounts", "/api/v1/forecast"] {
        let (status, _, _) = send_request(
            addr,
            Method::GET,
            path,
            hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
            Bytes::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    let (_, _, body) = send_request(addr, Method::GET, "/api/v1/auth/status", HeaderMap::new(), Bytes::new()).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"auth_required": true, "authenticated": false, "control_enabled": true})
    );
    let (_, _, body) = send_request(
        addr,
        Method::GET,
        "/api/v1/auth/status",
        hdrs(&[("authorization", bearer(TEST_TOKEN).as_str())]),
        Bytes::new(),
    )
    .await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["authenticated"], true);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_strategy_control_matrix() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    let (addr, server_task, shutdown_tx) = start_server().await;
    let auth = bearer(TEST_TOKEN);
    let good = || {
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json"),
            ("authorization", auth.as_str()),
        ])
    };
    let body = r#"{"strategy":"max_quota"}"#;
    let strategy = || agy_pool::storage::load_pool().unwrap().strategy;

    // No token configured: controls stay off even from loopback.
    let (status, _, resp) = post_strategy(addr, good(), body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&resp), "TOKEN_REQUIRED_FOR_CONTROL");
    assert_eq!(strategy(), "smart");

    set_dashboard_config(Some(TEST_TOKEN), false);
    let (status, _, resp) = post_strategy(
        addr,
        hdrs(&[("content-type", "application/json"), ("authorization", auth.as_str())]),
        body,
    )
    .await;
    assert_eq!((status, error_code(&resp).as_str()), (StatusCode::FORBIDDEN, "CONTROL_HEADER_REQUIRED"));
    let (status, _, resp) = post_strategy(
        addr,
        hdrs(&[("x-agy-pool-control", "1"), ("content-type", "text/plain"), ("authorization", auth.as_str())]),
        body,
    )
    .await;
    assert_eq!((status, error_code(&resp).as_str()), (StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON_REQUIRED"));
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[("x-agy-pool-control", "1"), ("content-type", "application/json")]),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[
            ("x-agy-pool-control", "1"),
            ("content-type", "application/json"),
            ("authorization", "Bearer wrong"),
        ]),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Cross-site browsers are stopped by the Host/Origin guard before auth.
    let mut cross_site = good();
    cross_site.insert(hyper::header::ORIGIN, "http://evil.example.com".parse().unwrap());
    let (status, _, _) = post_strategy(addr, cross_site, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(strategy(), "smart");

    for bad in [r#"{"strategy":"round_robin"}"#, r#"{"strategy":"smart","x":1}"#, "not json"] {
        let (status, _, resp) = post_strategy(addr, good(), bad).await;
        assert_eq!((status, error_code(&resp).as_str()), (StatusCode::BAD_REQUEST, "INVALID_ARGUMENT"), "{bad}");
    }

    let (status, headers, resp) = post_strategy(addr, good(), body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(hyper::header::CACHE_CONTROL).unwrap(), "no-store");
    let json: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    assert_eq!(json, serde_json::json!({"ok": true, "strategy": "max_quota"}));
    assert_eq!(strategy(), "max_quota");
    let (_, _, resp) = send_request(addr, Method::GET, "/api/v1/status", hdrs(&[("authorization", auth.as_str())]), Bytes::new()).await;
    let json: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    assert_eq!(json["strategy"], "max_quota");

    let (status, headers, _) = send_request(addr, Method::GET, "/api/v1/strategy", HeaderMap::new(), Bytes::new()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers.get(hyper::header::ALLOW).unwrap(), "POST");

    // Explicit opt-out: no token, controls allowed.
    set_dashboard_config(None, true);
    let (status, _, _) = post_strategy(
        addr,
        hdrs(&[("x-agy-pool-control", "1"), ("content-type", "application/json; charset=utf-8")]),
        r#"{"strategy":"smart"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(strategy(), "smart");

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_auth_failures_are_throttled() {
    let _guard = lock_test().await;
    let _tmp = setup_test_env();
    setup_sample_pool();
    set_dashboard_config(Some(TEST_TOKEN), false);
    let (addr, server_task, shutdown_tx) = start_server().await;
    let get = |path: &'static str, token: Option<&'static str>| {
        let headers = match token {
            Some(t) => hdrs(&[("authorization", bearer(t).as_str())]),
            None => HeaderMap::new(),
        };
        send_request(addr, Method::GET, path, headers, Bytes::new())
    };

    // Missing tokens (a fresh page load) never count.
    for _ in 0..10 {
        assert_eq!(get("/api/v1/status", None).await.0, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(get("/api/v1/status", Some(TEST_TOKEN)).await.0, StatusCode::OK);

    // Five wrong tokens lock this client out, even with the right token afterwards.
    for _ in 0..5 {
        assert_eq!(get("/api/v1/status", Some("wrong-token")).await.0, StatusCode::UNAUTHORIZED);
    }
    let (status, headers, body) = get("/api/v1/status", Some(TEST_TOKEN)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get(hyper::header::RETRY_AFTER).unwrap(), "60");
    assert_eq!(error_code(&body), "TOO_MANY_AUTH_FAILURES");

    // /auth/status is not a free guessing oracle.
    agy_pool::dashboard::auth::reset_auth_failures();
    for _ in 0..5 {
        assert_eq!(get("/api/v1/auth/status", Some("wrong-token")).await.0, StatusCode::OK);
    }
    assert_eq!(get("/api/v1/auth/status", Some(TEST_TOKEN)).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(get("/api/v1/status", Some(TEST_TOKEN)).await.0, StatusCode::TOO_MANY_REQUESTS);

    let _ = shutdown_tx.send(());
    let _ = server_task.await;
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: the 4 new tests FAIL (e.g. `/api/v1/auth/status` returns `404`, reads return `200` without a token); the 8 earlier tests still pass.

- [ ] **Step 4: Implement the gates and handlers** in `src/dashboard/mod.rs`.

Add `use auth::BearerCheck;` and `use crate::config::DashboardConfig;` to the imports.

Add after `api_error`:

```rust
fn unauthorized() -> Response<ProxyBody> {
    let mut resp = api_error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "missing or invalid dashboard token");
    resp.headers_mut().insert(
        hyper::header::WWW_AUTHENTICATE,
        hyper::header::HeaderValue::from_static("Bearer"),
    );
    resp
}

fn too_many_failures() -> Response<ProxyBody> {
    let mut resp = api_error(
        StatusCode::TOO_MANY_REQUESTS,
        "TOO_MANY_AUTH_FAILURES",
        "too many invalid dashboard tokens; retry in 60 seconds",
    );
    resp.headers_mut()
        .insert(hyper::header::RETRY_AFTER, hyper::header::HeaderValue::from_static("60"));
    resp
}

/// None = allowed. Without a configured token, reads stay open as in 0.3.x.
fn read_gate(req: &Request<hyper::body::Incoming>, token: Option<&str>) -> Option<Response<ProxyBody>> {
    let token = token?;
    match auth::check_bearer(req.headers(), auth::peer_ip(req.extensions()), token) {
        BearerCheck::Ok => None,
        BearerCheck::Throttled => Some(too_many_failures()),
        BearerCheck::Missing | BearerCheck::Invalid => Some(unauthorized()),
    }
}

/// Mutations need the custom header (forces a CORS preflight, which is never granted),
/// a JSON content type, and the token unless the operator opted out.
fn control_gate(
    req: &Request<hyper::body::Incoming>,
    cfg: &DashboardConfig,
    token: Option<&str>,
) -> Option<Response<ProxyBody>> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
    if header("x-agy-pool-control") != Some("1") {
        return Some(api_error(
            StatusCode::FORBIDDEN,
            "CONTROL_HEADER_REQUIRED",
            "missing X-Agy-Pool-Control: 1 header",
        ));
    }
    let is_json = header(hyper::header::CONTENT_TYPE.as_str())
        .is_some_and(|v| v.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Some(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "JSON_REQUIRED",
            "Content-Type must be application/json",
        ));
    }
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

fn auth_status_response(
    req: &Request<hyper::body::Incoming>,
    cfg: &DashboardConfig,
    token: Option<&str>,
) -> Response<ProxyBody> {
    let authenticated = match token {
        None => true,
        Some(t) => match auth::check_bearer(req.headers(), auth::peer_ip(req.extensions()), t) {
            BearerCheck::Ok => true,
            BearerCheck::Throttled => return too_many_failures(),
            BearerCheck::Missing | BearerCheck::Invalid => false,
        },
    };
    let body = serde_json::json!({
        "auth_required": token.is_some(),
        "authenticated": authenticated,
        "control_enabled": token.is_some() || cfg.allow_unauthenticated_control,
    });
    json_response(serde_json::to_vec(&body).unwrap_or_default())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StrategyRequest {
    strategy: String,
}

async fn set_strategy(req: Request<hyper::body::Incoming>) -> Response<ProxyBody> {
    let invalid = || {
        api_error(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "body must be {\"strategy\": \"smart\" | \"max_quota\"}",
        )
    };
    let Ok(body) = http_body_util::Limited::new(req.into_body(), 4096).collect().await else {
        return invalid();
    };
    let strategy = match serde_json::from_slice::<StrategyRequest>(&body.to_bytes()) {
        Ok(r) if matches!(
            r.strategy.as_str(),
            crate::config::STRATEGY_SMART | crate::config::STRATEGY_MAX_QUOTA
        ) =>
        {
            r.strategy
        }
        _ => return invalid(),
    };
    let to_save = strategy.clone();
    let saved = tokio::task::spawn_blocking(move || {
        crate::storage::pool_transaction(move |p| {
            p.strategy = to_save;
            Ok(())
        })
    })
    .await;
    match saved {
        Ok(Ok(())) => json_response(
            serde_json::to_vec(&serde_json::json!({ "ok": true, "strategy": strategy })).unwrap_or_default(),
        ),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "STORAGE_ERROR", &e.to_string()),
        Err(_) => internal_error(),
    }
}
```

Replace the `let is_get = …` line and the whole `match route.as_str() { … }` in `serve` (from Task 3) with:

```rust
    let cfg = crate::config::get_static_config().dashboard;
    let token = crate::config::dashboard_token(&cfg);
    let token = token.as_deref();
    let is_get = req.method() == Method::GET;
    let is_post = req.method() == Method::POST;

    match route.as_str() {
        "/" | "/dashboard" if is_get => html_response(),
        "/dashboard"
        | "/api/v1/auth/status"
        | "/api/v1/status"
        | "/api/v1/accounts"
        | "/api/v1/forecast"
        | "/api/v1/events"
            if !is_get =>
        {
            method_not_allowed("GET")
        }
        "/api/v1/auth/status" => auth_status_response(&req, &cfg, token),
        "/api/v1/strategy" if !is_post => method_not_allowed("POST"),
        "/api/v1/strategy" => match control_gate(&req, &cfg, token) {
            Some(denied) => denied,
            None => set_strategy(req).await,
        },
        "/api/v1/status" | "/api/v1/accounts" | "/api/v1/forecast" | "/api/v1/events" => {
            if let Some(denied) = read_gate(&req, token) {
                return denied;
            }
            match route.as_str() {
                "/api/v1/status" => status_response().await,
                "/api/v1/accounts" => accounts_response(req.uri().query()).await,
                "/api/v1/forecast" => forecast_response().await,
                _ => events_response(),
            }
        }
        _ => api_error(StatusCode::NOT_FOUND, "NOT_FOUND", "unknown dashboard API path"),
    }
```

- [ ] **Step 5: Run the dashboard tests**

Run: `timeout 900 scripts/test.sh test --test dashboard`
Expected: all 12 tests pass.

- [ ] **Step 6: Run the full suite once**

Run: `timeout 900 scripts/test.sh`
Expected: 0 failed. If anything outside `tests/dashboard.rs`, `tests/daemon_child.rs`, `config::`, `dashboard::` or `proxy::` fails, stop and report the failing test names and output; do not change unrelated tests.

- [ ] **Step 7: Format, lint, commit**

```bash
~/.cargo/bin/cargo fmt
timeout 900 scripts/test.sh clippy --all-targets -- -D warnings
git add src/dashboard/mod.rs tests/dashboard.rs
git commit -m "feat(dashboard): require the token for API access and add strategy control

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Frontend — token prompt, `agyFetch`, fetch-stream SSE, strategy selector

**Files:**
- Modify: `src/dashboard/assets/index.html`
- Modify: `tests/dashboard.rs` (`test_embedded_spa_asset_constraints`)

**Interfaces:**
- Consumes: the HTTP contract from Task 4.
- Produces: nothing for other tasks.

- [ ] **Step 1: Extend the asset test** — in `tests/dashboard.rs`, inside `test_embedded_spa_asset_constraints`, insert right before `let _ = shutdown_tx.send(());`:

```rust
    // Auth: the token stays per tab and goes out as a Bearer header; EventSource cannot send headers.
    for needle in [
        "agy_dashboard_token",
        "Authorization",
        "X-Agy-Pool-Control",
        "/api/v1/auth/status",
        "/api/v1/strategy",
        "token-card",
        "strategy-select",
        "getReader()",
    ] {
        assert!(body_str.contains(needle), "missing {needle}");
    }
    assert!(!body_str.contains("new EventSource("), "EventSource cannot send the Bearer token");
    for line in body_str.lines().filter(|l| l.contains("sessionStorage.")) {
        assert!(line.contains("try {"), "unguarded sessionStorage access: {}", line.trim());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `timeout 900 scripts/test.sh test --test dashboard test_embedded_spa_asset_constraints`
Expected: FAIL with `missing agy_dashboard_token`.

- [ ] **Step 3: Add the CSS** — insert immediately before `  </style>` (~line 593):

```css
    .token-card {
      position: fixed;
      inset: 0;
      z-index: 50;
      display: flex;
      align-items: center;
      justify-content: center;
      background: var(--bg);
    }
    .token-card[hidden] { display: none; }
    .token-card form {
      width: min(380px, 90vw);
      display: flex;
      flex-direction: column;
      gap: 10px;
      padding: 20px;
      border: 1px solid var(--border);
      border-radius: 10px;
      background: var(--card-bg);
      color: var(--text-main);
    }
    .token-card h2 { margin: 0; font-size: 16px; }
    .token-card p { margin: 0; font-size: 12px; color: var(--text-muted); }
    .token-card input, .token-card button, #strategy-select {
      font: inherit;
      color: var(--text-main);
      background: var(--bg);
      border: 1px solid var(--border);
      border-radius: 6px;
      padding: 6px 8px;
    }
    .token-card button { cursor: pointer; border-color: var(--accent-border); }
    .token-card .token-error { color: var(--red-text); min-height: 1em; }
```

- [ ] **Step 4: Add the markup.** Insert directly after `<body>`:

```html
  <div id="token-card" class="token-card" hidden>
    <form id="token-form">
      <h2>Dashboard token</h2>
      <p>This gateway requires its dashboard token (<code>dashboard.auth_token</code> or <code>AGY_POOL_DASHBOARD_TOKEN</code>). It is kept only for this browser tab.</p>
      <input id="token-input" type="password" autocomplete="current-password" aria-label="Dashboard token" required>
      <button type="submit">Unlock</button>
      <p id="token-error" class="token-error" role="alert"></p>
    </form>
  </div>
```

Insert as the first child of `<div class="header-controls">`:

```html
      <select id="strategy-select" aria-label="Load balancing strategy" title="Load balancing strategy" hidden>
        <option value="smart">smart</option>
        <option value="max_quota">max_quota</option>
      </select>
```

- [ ] **Step 5: Add auth state and `agyFetch`.** Replace the two lines

```js
    let sseSource = null;
    let reconnectTimer = null;
```

with:

```js
    let sseAbort = null;
    let reconnectTimer = null;

    // Dashboard auth: the token is kept per tab (session storage) and sent as a Bearer header.
    const TOKEN_KEY = 'agy_dashboard_token';
    let authToken = '';
    try { authToken = sessionStorage.getItem(TOKEN_KEY) || ''; } catch (_) {}
    let authOk = false;
    let started = false;

    function saveToken(t) {
      authToken = t;
      try { if (t) { sessionStorage.setItem(TOKEN_KEY, t); } else { sessionStorage.removeItem(TOKEN_KEY); } } catch (_) {}
    }

    function showTokenCard(message) {
      authOk = false;
      saveToken('');
      if (sseAbort) { sseAbort.abort(); sseAbort = null; }
      document.getElementById('token-error').textContent = message || '';
      document.getElementById('token-card').hidden = false;
      document.getElementById('token-input').focus();
    }

    async function agyFetch(url, opts = {}) {
      const headers = Object.assign({}, opts.headers);
      if (authToken) headers['Authorization'] = `Bearer ${authToken}`;
      if ((opts.method || 'GET').toUpperCase() !== 'GET') {
        headers['X-Agy-Pool-Control'] = '1';
        headers['Content-Type'] = 'application/json';
      }
      const res = await fetch(url, Object.assign({}, opts, { headers }));
      if (res.status === 401) showTokenCard('Token rejected. Enter the dashboard token again.');
      if (res.status === 429) showTokenCard('Too many wrong tokens. Wait a minute, then try again.');
      return res;
    }
```

- [ ] **Step 6: Route the REST calls through `agyFetch`.** Make exactly these three replacements:
  - In `fetchStatus`: `const res = await fetch('/api/v1/status');` → `const res = await agyFetch('/api/v1/status');`
  - In `fetchAccounts`: `const res = await fetch(url);` → `const res = await agyFetch(url);`
  - In `fetchForecast`: `const res = await fetch('/api/v1/forecast');` → `const res = await agyFetch('/api/v1/forecast');`

In `fetchStatus`, directly after the line `endpointPill.textContent = \`${window.location.host || statusData.listen} · ${statusData.strategy}\`;` add:

```js
        if (!strategySelect.disabled) strategySelect.value = statusData.strategy;
```

- [ ] **Step 7: Replace the SSE client.** Replace everything from the line `    // SSE Stream` through the closing `    }` of `function connectSSE()` (the line before `    // Initialization`) with:

```js
    // SSE Stream
    function handleAuditEvent(data) {
      try {
        const ev = JSON.parse(data);
        const isDup = eventList.some(
          (item) => (ev.id && item.id === ev.id && item.timestamp === ev.timestamp)
            || (!ev.id && item.timestamp === ev.timestamp && item.account === ev.account && item.model === ev.model)
        );
        if (isDup) {
          return;
        }
        eventList.unshift(ev);
        if (eventList.length > 20) {
          eventList.pop();
        }
        if (!streamPaused) {
          renderStream();
        }
      } catch (err) {
        console.error('Failed to parse SSE audit event:', err);
      }
    }

    function handleStatsEvent(data) {
      try {
        const stats = JSON.parse(data);
        if (stats) {
          document.getElementById('metric-inflight').textContent = stats.in_flight;
          document.getElementById('metric-qps').textContent = (stats.qps || 0).toFixed(2);
          document.getElementById('metric-rss').textContent = (stats.rss_mb || 0).toFixed(1);
        }
      } catch (err) {
        console.error('Failed to parse SSE stats event:', err);
      }
    }

    function scheduleReconnect() {
      sseDot.className = 'status-dot red';
      sseStatusText.textContent = 'Reconnecting...';
      reconnectBanner.style.display = 'block';
      if (!reconnectTimer) {
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          connectSSE();
        }, 3000);
      }
    }

    // EventSource cannot send an Authorization header, so the event stream is read through fetch.
    async function connectSSE() {
      if (sseAbort) sseAbort.abort();
      const ctrl = new AbortController();
      sseAbort = ctrl;
      sseDot.className = 'status-dot yellow';
      sseStatusText.textContent = 'Connecting...';
      try {
        const res = await agyFetch('/api/v1/events', { signal: ctrl.signal });
        if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
        sseDot.className = 'status-dot pulse';
        sseStatusText.textContent = 'Connected';
        reconnectBanner.style.display = 'none';
        if (reconnectTimer) {
          clearTimeout(reconnectTimer);
          reconnectTimer = null;
        }
        if (!streamPaused) {
          eventList = [];
          renderStream();
        }
        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buf = '';
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buf += decoder.decode(value, { stream: true });
          let idx;
          while ((idx = buf.indexOf('\n\n')) >= 0) {
            const block = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            let event = 'message';
            const data = [];
            for (const line of block.split('\n')) {
              if (line.startsWith('event:')) event = line.slice(6).trim();
              else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
            }
            if (event === 'audit') handleAuditEvent(data.join('\n'));
            else if (event === 'stats') handleStatsEvent(data.join('\n'));
          }
        }
      } catch (err) {
        if (!ctrl.signal.aborted) console.error('SSE stream error:', err);
      }
      // Stop when replaced, aborted, or waiting for a token; otherwise the stream dropped.
      if (ctrl.signal.aborted || sseAbort !== ctrl || !authOk) return;
      scheduleReconnect();
    }
```

- [ ] **Step 8: Replace initialization.** Replace the whole `function init() { … }` (keep the following `init();` call) with:

```js
    const strategySelect = document.getElementById('strategy-select');
    strategySelect.addEventListener('change', async () => {
      const wanted = strategySelect.value;
      strategySelect.disabled = true;
      try {
        const res = await agyFetch('/api/v1/strategy', { method: 'POST', body: JSON.stringify({ strategy: wanted }) });
        if (!res.ok && res.status !== 401 && res.status !== 429) {
          let msg = `HTTP ${res.status}`;
          try { msg = (await res.json()).error.message || msg; } catch (_) {}
          alert(`Could not change strategy: ${msg}`);
        }
      } catch (err) {
        alert(`Could not change strategy: ${err}`);
      } finally {
        strategySelect.disabled = false;
        fetchStatus();
      }
    });

    document.getElementById('token-form').addEventListener('submit', (e) => {
      e.preventDefault();
      saveToken(document.getElementById('token-input').value.trim());
      checkAuthAndStart();
    });

    async function checkAuthAndStart() {
      let s;
      try {
        const res = await fetch('/api/v1/auth/status', { headers: authToken ? { Authorization: `Bearer ${authToken}` } : {} });
        if (res.status === 429) { showTokenCard('Too many wrong tokens. Wait a minute, then try again.'); return; }
        if (!res.ok) { showTokenCard(`Dashboard refused the request (HTTP ${res.status}).`); return; }
        s = await res.json();
      } catch (err) {
        console.error('Auth check failed:', err);
        setTimeout(checkAuthAndStart, 3000);
        return;
      }
      if (!s.authenticated) {
        showTokenCard(authToken ? 'Token rejected.' : '');
        return;
      }
      document.getElementById('token-card').hidden = true;
      document.getElementById('token-input').value = '';
      authOk = true;
      strategySelect.hidden = !s.control_enabled;
      start();
    }

    function start() {
      Promise.all([fetchStatus(), fetchAccounts(), fetchForecast()]).catch(console.error);
      connectSSE();
      if (started) return;
      started = true;

      // Periodic 5s polling for background updates
      setInterval(() => {
        if (!authOk) return;
        fetchStatus();
        fetchAccounts();
        fetchForecast();
      }, 5000);

      // 1s local tick for countdowns
      setInterval(updateLocalCountdowns, 1000);
    }

    function init() {
      checkAuthAndStart();
    }
```

Then run `grep -n "sseSource\|EventSource(" src/dashboard/assets/index.html`. The only match allowed is the comment line containing `EventSource cannot send`.

- [ ] **Step 9: Syntax-check the script and run the tests**

```bash
python3 -I -c "import re;s=open('src/dashboard/assets/index.html').read();open('/tmp/agy-dash.js','w').write(re.findall(r'<script>(.*?)</script>',s,re.S)[-1])" && node --check /tmp/agy-dash.js && echo JS-OK
timeout 900 scripts/test.sh test --test dashboard
```

Expected: `JS-OK`, then all 12 dashboard tests pass.

- [ ] **Step 10: Commit**

```bash
~/.cargo/bin/cargo fmt
git add src/dashboard/assets/index.html tests/dashboard.rs
git commit -m "feat(dashboard): ask for the token, stream events via fetch, add strategy selector

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

## After all tasks (Claude, not agy)

- Opus review of Tasks 2 and 4 (auth, limiter, gates); Sonnet review of Tasks 1, 3 and 5.
- Manual browser check (token prompt, wrong token, strategy switch, SSE reconnect) against a test daemon on an ephemeral port with a temporary data dir — never against the live gateway.
- Push the branch, open a PR, and wait for CI.
