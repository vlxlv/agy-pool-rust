# agy-pool Rust Rewrite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Execution in this repo:** each task is delegated to agy (agy-worker `agy_run`) with the task text as instructions and "the listed tests pass" as acceptance; Claude reviews the diff and re-runs the tests before the next task.

**Goal:** Rewrite agy-pool in Rust under `rust/` as a drop-in replacement for the Go binary: same CLI, same port, same on-disk state, same no-replay guarantees.

**Architecture:** One Cargo package `rust/` (library + `agy-pool` binary) with one module per Go package (`config`, `observability`, `storage`, `auth`, `accounts`, `quota`, `scheduler`, `forecast`, `conversation`, `proxy`, `daemon`, `diagnostics`, `installer`, `cli`). The Go code is the behaviour spec: every Go test is ported 1:1, and the language-neutral fixtures in `testdata/` are shared by both implementations. Go stays the released, live binary until the Phase 5 cutover gate passes.

**Tech Stack:** Rust 1.99 (`~/.cargo/bin`), tokio, hyper 1 + hyper-util (server and upstream client), rustls + rustls-native-certs, rusqlite (`bundled`), serde/serde_json, crossterm (top TUI), tempfile (tests).

**Spec:** this plan + `docs/MIGRATION_CONTRACT.md` (the Python→Go contract; its principles 2–10 apply unchanged with "Python" read as "Go" and "Go" read as "Rust") + the Go source of each ported package.

## Global Constraints

- Location: `rust/` in this repo (Cargo package name `agy-pool`, binary `agy-pool`). Work on local branch `rust-rewrite`; never push it or touch `main`/tags/releases without the user's say-so.
- Drop-in compatibility: same `state.db` schema (`schema_version` = `"1"`, tables `meta`, `accounts`, `runtime_global`, `account_runtime`, `account_quota`, `inflight_leases` incl. `pid_ns`), same pragmas (WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`), same `config.json` keys and validation errors, same files under the data dir (`state.db`, `agy-pool.pid`, `agy-pool.log`, `locks/`, `quota-history.jsonl`, runtime keys such as persisted session affinities), same env vars (`AGY_BIN`, `AGY_CLIENT_ID`, `AGY_CLIENT_SECRET`, `AGY_CONFIG_FILE`, `AGY_DATA_DIR`, `AGY_ENTRYPOINT_PATH`, `AGY_GEMINI_DIR`, `AGY_MANAGED_NATIVE_PATH`, `AGY_POOL_QR`, `AGY_PRIVACY`, `AGY_REAL_GEMINI_DIR`, `AGY_SANDBOX_DIR`, `AGY_SELF_PATH`, `AGY_SHOW_EMAIL`, `AGY_TEST_MODE`, `AGY_TEST_PID_FILE`), same CLI commands, aliases, exit codes and message strings, default port 8899.
- Go and Rust processes must be able to share one `state.db` at the same time (leases, pool writes) — during the candidate phase they will.
- Never write to the live state: the live gateway (systemd user unit `agy-pool`, `~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.gemini`, `~/.local/bin/agy-pool`) serves every agy run, including the ones doing this work. All Rust tests run with `AGY_TEST_MODE=1` and temp dirs; the ported path guards (Task 2) make tests fail closed on protected paths. Candidate instances use port 8901 and a copied data dir.
- No `clap`: argument parsing is hand-ported so messages and golden CLI cases match byte-for-byte.
- No new runtime behaviour: tuning constants (pace slack, harvest windows, affinity floors, slow-pause strikes, cooldowns) are copied verbatim from Go. Improvements wait until after cutover.
- No `unsafe` outside FFI-free std calls that need it (e.g. `libc::kill(pid, 0)`), each with a one-line `// SAFETY:` comment.
- Toolchain: `export PATH=$HOME/.cargo/bin:$HOME/sdk/go1.26.10/bin:$PATH`. The host has 2 cores: run `cargo test -p agy-pool <module>::` filters, not the whole suite, inside a task; the full suite runs once per phase.
- **Running tests on this host:** only through `scripts/rust-test.sh [cargo args]` (e.g. `scripts/rust-test.sh test --lib storage::`, `scripts/rust-test.sh clippy --all-targets -- -D warnings`). It runs cargo in a private mount namespace with `~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.gemini` and `~/.local/bin` read-only and sets PATH and `AGY_TEST_MODE=1`. Never run bare `cargo test` here: on 2026-11-06 a racing test overwrote the live state.db. Test builds also panic if `data_dir()`/`native_agy_dir()` resolve to a live path; test helpers that take `TEST_LOCK` must return the guard so the test body holds it.
- Every task ends with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and its tests passing, then one commit on `rust-rewrite` ending with the `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` line.

## Review Focus

1. **A Rust test or candidate run touching live state** — expected: the path guard rejects it before any write. Pinned in Task 2 (`refuses_live_data_dir_in_test_mode`).
2. **A Go gateway and a Rust process sharing one `state.db`** — expected: each sees the other's in-flight leases and pool writes, and neither corrupts the other's rows or `extra` JSON. Pinned in Task 5 (`go_and_rust_share_leases_and_pool`).
3. **An existing production `state.db` with unknown keys in `accounts.extra` / `account_quota.raw_extra`** — expected: Rust round-trips them unchanged. Pinned in Task 4 (`unknown_extra_keys_round_trip`).
4. **A request that fails after the upstream connection opened** — expected: never replayed on another account. Pinned in Phase 3 (proxy plan), listed here so it is not lost.
5. **Two gateways (Go and Rust) both trying to own the PID file / port** — expected: the second refuses to start, as Go does today. Pinned in Phase 3 (daemon plan).

---

## Phases

Each phase gets its own detailed plan, written after the previous phase is merged into `rust-rewrite` and reviewed. Only Phase 1 is detailed below.

| Phase | Modules (Go packages) | Go lines (src+tests) | Gate |
|---|---|---|---|
| 1 Foundation | config, observability, storage | ~6k | Ported tests + golden pool fixtures pass; Go↔Rust shared-DB interop test passes |
| 2 Domain | auth, accounts, quota, scheduler, forecast, conversation | ~13k | Ported tests + `testdata/quota/capacity_cases.json`, `testdata/scheduler/ordering_cases.json` pass |
| 3 Gateway | proxy, daemon, diagnostics | ~16k | Ported tests pass, incl. no-replay, affinity, cooldown, streaming, PID-lock cases |
| 4 CLI | cli (incl. `top`, `update`), installer | ~13k | Ported tests + `testdata/cli/golden_cases.json` pass against the Rust binary |
| 5 Candidate & cutover | CI/release workflows, systemd, docs | — | Rust candidate on 8901 with a copied data dir serves real agy traffic for an agreed soak period with no no-replay or quota regressions vs Go; user approves the switch; the cutover release is published as an `agy-pool-go` release so the installed Go binary's `agy-pool update` reaches it; rollback = reinstall the Go binary |
| 6 Repo split | move `rust/` to `agy-pool-rust`, archive `agy-pool-go` | — | Rust gateway has served live traffic for a few days after cutover; final Go version tagged `go-final`; `agy-pool-rust` CI green on its own; `agy-pool update` on the installed Rust binary finds `agy-pool-rust` releases |

---

## Phase 1 — Foundation

### File Structure

- `rust/Cargo.toml`, `rust/Cargo.lock`, `rust/rustfmt.toml` — package, pinned deps.
- `rust/src/lib.rs` — module declarations only.
- `rust/src/main.rs` — `fn main()` that prints `agy-pool <version>` for `--version` (real CLI arrives in Phase 4).
- `rust/src/config/mod.rs` — constants, `StaticConfig`, config file load/validate/hash (`internal/config/config.go`).
- `rust/src/config/paths.rs` — data dir / file layout, env overrides, test mode, path guards.
- `rust/src/observability/mod.rs` — counters, snapshot, Prometheus text, in-flight tracking (`internal/observability`).
- `rust/src/storage/db.rs` — `StateDb` open/schema/pragmas/transactions (`internal/storage/db.go`).
- `rust/src/storage/model.rs` — `Pool`, `Account`, `QuotaState`, `QuotaWindow` + JSON (de)serialisation with unknown-key preservation.
- `rust/src/storage/pool.rs` — load/save pool from a transaction, `pool_transaction`, `record_success`, runtime values, legacy JSON migration.
- `rust/src/storage/inflight.rs` — cross-process in-flight leases (`internal/storage/inflight.go`).
- `rust/tests/interop.rs` + `rust/interop/goside/main.go` — Go↔Rust shared-DB test.
- `.github/workflows/rust.yml` — fmt, clippy, test for `rust/`.

### Task 1: Scaffold the Rust package and CI

**Files:**
- Create: `rust/Cargo.toml`, `rust/rustfmt.toml`, `rust/src/lib.rs`, `rust/src/main.rs`, `rust/tests/version.rs`, `.github/workflows/rust.yml`
- Modify: `.gitignore` (add `rust/target/`)

**Interfaces:**
- Produces: crate `agy_pool` (lib) with `pub const VERSION: &str`; binary `agy-pool`.

- [ ] **Step 1: Create branch and package**

```bash
cd /home/claude/work/agy-pool-go && git switch -c rust-rewrite
export PATH=$HOME/.cargo/bin:$PATH
cargo new --lib rust --name agy-pool --vcs none
```

`rust/Cargo.toml`:

```toml
[package]
name = "agy-pool"
version = "0.1.1-alpha.2"
edition = "2024"
rust-version = "1.99"

[lib]
name = "agy_pool"
path = "src/lib.rs"

[[bin]]
name = "agy-pool"
path = "src/main.rs"

[dependencies]
rusqlite = { version = "*", features = ["bundled"] }
serde = { version = "*", features = ["derive"] }
serde_json = { version = "*", features = ["preserve_order"] }
sha2 = "*"
hex = "*"
libc = "*"

[dev-dependencies]
tempfile = "*"

[profile.release]
strip = true
lto = "thin"
```

After the first successful build, replace each `"*"` with the exact version `cargo tree --depth 1` resolved (e.g. `"0.37.0"`), so later tasks never float.

`rust/rustfmt.toml`: `max_width = 120`

`rust/src/lib.rs`:

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

`rust/src/main.rs`:

```rust
fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("-v" | "--version" | "version") => println!("agy-pool {}", agy_pool::VERSION),
        _ => {
            eprintln!("agy-pool (rust): only --version is implemented yet");
            std::process::exit(2);
        }
    }
}
```

- [ ] **Step 2: Write the failing test** — `rust/tests/version.rs`:

```rust
use std::process::Command;

#[test]
fn version_matches_go_golden_case() {
    let out = Command::new(env!("CARGO_BIN_EXE_agy-pool")).arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("agy-pool"), "golden case `version` expects stdout to contain agy-pool, got {stdout:?}");
    assert!(stdout.contains(agy_pool::VERSION));
}
```

- [ ] **Step 3: Run and verify**

Run: `cd rust && cargo test --test version`
Expected: PASS (the test pins the golden `version` case from `testdata/cli/golden_cases.json` from day one).

- [ ] **Step 4: CI** — `.github/workflows/rust.yml`:

```yaml
name: Rust
on:
  push:
    branches: [main, rust-rewrite]
    paths: ['rust/**', 'testdata/**', '.github/workflows/rust.yml']
  pull_request:
    paths: ['rust/**', 'testdata/**', '.github/workflows/rust.yml']
permissions:
  contents: read
jobs:
  test:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: rust
    env:
      AGY_TEST_MODE: '1'
    steps:
      - uses: actions/checkout@v7
      - uses: actions/setup-go@v7
        with:
          go-version-file: 'go.mod'
      - run: rustup toolchain install 1.99 --profile minimal -c rustfmt,clippy && rustup default 1.99
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
```

Add `rust/target/` to `.gitignore`.

- [ ] **Step 5: Commit**

```bash
cd rust && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add rust .gitignore .github/workflows/rust.yml
git commit -m "feat(rust): scaffold Rust package and CI"
```

### Task 2: Port `config` (constants, static config, paths, guards)

**Files:**
- Create: `rust/src/config/mod.rs`, `rust/src/config/paths.rs`
- Modify: `rust/src/lib.rs` (`pub mod config;`)
- Test: `#[cfg(test)] mod tests` in both files; fixture `config.example.json` (repo root) read via `concat!(env!("CARGO_MANIFEST_DIR"), "/../config.example.json")`
- Spec: `internal/config/*.go` and `internal/config/*_test.go`

**Interfaces:**
- Produces (used by every later task; Go name → Rust name, snake_case, same semantics):
  - `pub fn migrate_legacy_strategy(s: &str) -> String`
  - `pub struct StaticConfig` (all Go fields, serde with Go's JSON key names; unknown/forbidden keys rejected with the same messages), `pub fn default_static_config() -> StaticConfig`, `pub fn load_config_file(path: &Path) -> Result<StaticConfig, ConfigError>`, `pub fn compute_config_file_hash(path: &Path) -> io::Result<String>`, `pub fn resolve_default_config_file() -> Result<PathBuf, ConfigError>`, `get_static_config()/set_static_config()`
  - `pub fn data_dir() -> PathBuf`, `configure_data_dir(&Path) -> Result<(), ConfigError>`, `reset_data_dir()`, `configure_default_data_dir()`, `state_db_file()`, `accounts_file()`, `runtime_file()`, `quota_file()`, `pid_file()`, `log_file()`, `locks_dir()`, `lock_file()`, `specific_lock_file(name)`, `presence_dir()`, `presence_lock_file(cid)`, `native_agy_dir()`, `agy_cli_dir()`, `agy_token_file()`, `conversation_db_file()`, `real_host_user_home()`, `real_production_gemini_dir()`
  - `pub fn is_test_mode() -> bool`, `set_test_mode(bool)`, `set_synthetic_sandbox_root(&Path) -> impl Drop` (Go returned a restore func; Rust returns a guard)
  - `pub fn is_protected_production_dir(&Path) -> bool`, `assert_safe_destructive_path`, `assert_safe_write_path`, `assert_safe_native_token_write`, `assert_safe_read_path` — all `(&Path) -> Result<(), ConfigError>`
  - `pub enum ConfigError` with `Display` strings copied from Go's `fmt.Errorf` texts.
- Global mutable state (Go package vars) becomes `static` `RwLock`/`OnceLock` values; tests that mutate them take a shared `static TEST_LOCK: Mutex<()>` so they don't race.

- [ ] **Step 1: Port the 21 Go tests first** — one Rust `#[test]` per Go test in `internal/config/*_test.go`, same name in snake_case (e.g. `TestDestructiveGuard_SymlinkResolution` → `destructive_guard_symlink_resolution`), same inputs and assertions. Add this one, which pins Review Focus #1:

```rust
#[test]
fn refuses_live_data_dir_in_test_mode() {
    let _g = TEST_LOCK.lock().unwrap();
    set_test_mode(true);
    let live = real_host_user_home().join(".local/share/agy-pool");
    assert!(configure_data_dir(&live).is_err(), "test mode must refuse the live data dir");
    assert!(assert_safe_write_path(&live.join("state.db")).is_err());
    let tmp = tempfile::tempdir().unwrap();
    assert!(configure_data_dir(tmp.path()).is_ok());
    reset_data_dir();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd rust && AGY_TEST_MODE=1 cargo test --lib config::`
Expected: FAIL to compile (functions not defined).

- [ ] **Step 3: Port `internal/config` line by line** into the two files, keeping Go's order of checks, error texts, env-var handling and symlink resolution (`std::fs::canonicalize` where Go uses `filepath.EvalSymlinks`).

- [ ] **Step 4: Run to verify they pass**

Run: `cd rust && AGY_TEST_MODE=1 cargo test --lib config::`
Expected: 22 passed.

- [ ] **Step 5: Commit** — `git commit -m "feat(rust): port config package"` (after fmt + clippy).

### Task 3: Port `observability`

**Files:**
- Create: `rust/src/observability/mod.rs`
- Modify: `rust/src/lib.rs` (`pub mod observability;`)
- Spec: `internal/observability/*.go` and its 4 tests

**Interfaces:**
- Produces: `reset()`, `snapshot() -> Snapshot` (serde field names = Go JSON tags, e.g. `no_replay_prevented_total`), `format_prometheus_metrics() -> String` (byte-identical to Go for the same counters), one `record_*()` per Go `Record*` function, `format_number(u64) -> String`, `set_remote_in_flight(HashMap<String, i64>)`, `set_on_in_flight_change(Box<dyn Fn() + Send + Sync>)`, `start_in_flight_generation(&str) -> InFlightGuard` (decrements on `Drop`; Go returned a func), `in_flight_generation(&str) -> i64`, `all_in_flight_generations() -> HashMap<String, i64>`, `reset_in_flight()`.
- Counters are `AtomicU64`; in-flight map is `Mutex<HashMap<String, i64>>`.

- [ ] **Step 1: Port the 4 Go tests** (`TestMetrics_AtomicOperationsAndReset`, `TestFormatNumber`, `TestInFlightTracking_ConcurrentLifecycle` — use `std::thread::scope` with the same thread/iteration counts — and `TestFormatPrometheusMetrics`, asserting the same exact text lines).
- [ ] **Step 2: Run** `cargo test --lib observability::` — expected: compile failure.
- [ ] **Step 3: Port the implementation.**
- [ ] **Step 4: Run** `cargo test --lib observability::` — expected: 4 passed.
- [ ] **Step 5: Commit** — `feat(rust): port observability package`.

### Task 4: Port `storage` database, model and pool

**Files:**
- Create: `rust/src/storage/mod.rs`, `rust/src/storage/db.rs`, `rust/src/storage/model.rs`, `rust/src/storage/pool.rs`
- Modify: `rust/src/lib.rs` (`pub mod storage;`)
- Spec: `internal/storage/*.go` except `inflight.go`, and all their tests; fixtures `testdata/pool/{valid_3_accounts,legacy_alpha9,empty,corrupt}.json`

**Interfaces:**
- Consumes: `config::{state_db_file, data_dir, assert_safe_write_path, is_test_mode}`, `observability::record_persistence_error`.
- Produces:
  - `pub struct StateDb` with `open(path: &Path) -> Result<StateDb, StorageError>` (same fail-closed checks: zero-byte file, corrupt file, wrong `schema_version`), `global() -> Result<&'static StateDb, StorageError>` (Go `GetStateDB`), `checkpoint_passive()`, `view<T>(|tx: &rusqlite::Transaction| -> Result<T>)`, `update<T>(...)` (`BEGIN IMMEDIATE`), `try_update<T>(busy: Duration, ...)`.
  - `model.rs`: `Pool`, `Account`, `QuotaState`, `QuotaWindow`, `Status` with the same JSON shapes as Go's custom `MarshalJSON`/`UnmarshalJSON`, plus `#[serde(flatten)] extra: serde_json::Map<String, Value>` wherever Go preserves unknown keys; `Account::hits()`, `Pool::record_generation_telemetry(&str, i64)`, `Pool::new_empty()`.
  - `pool.rs`: `load_pool_from_tx`, `save_pool_to_tx`, `load_pool()`, `save_pool(&Pool)`, `pool_transaction(|&mut Pool| -> Result<()>)`, `record_success(&Account, is_generation: bool, now: i64)`, `find_account`, `next_account_id`, `load_runtime_value(&str)`, `save_runtime_value(&str, &str)`, `ensure_dirs()`, `with_file_lock(path, exclusive, f)` (`libc::flock`), `atomic_json_write`, `migrate_legacy_pool`, `migrate_split_json_to_db`, `ensure_file_permissions` (0600).
- Use `rusqlite::Connection` per call guarded by a `Mutex` pool of up to 10 connections (Go's `SetMaxOpenConns(10)`); every connection applies the four pragmas.

- [ ] **Step 1: Port every Go test in `internal/storage/*_test.go` except the in-flight ones** (those move to Task 5), same names in snake_case. `TestLoadGoldenPoolFixtures` reads `concat!(env!("CARGO_MANIFEST_DIR"), "/../testdata/pool/")`. Add this one, pinning Review Focus #3:

```rust
#[test]
fn unknown_extra_keys_round_trip() {
    let _g = crate::config::TEST_LOCK.lock().unwrap();
    crate::config::set_test_mode(true);
    let dir = tempfile::tempdir().unwrap();
    let db = StateDb::open(&dir.path().join("state.db")).unwrap();
    db.update(|tx| {
        tx.execute(
            "INSERT INTO accounts (id, name, email, extra) VALUES ('acc_1', 'Main', 'a@b', ?1)",
            [r#"{"future_field":{"x":1},"note":"keep me"}"#],
        )?;
        tx.execute(
            "INSERT INTO account_quota (account_id, gemini_5h_fraction, raw_extra) VALUES ('acc_1', 0.5, ?1)",
            [r#"{"vendor_bucket":[1,2,3]}"#],
        )?;
        Ok(())
    })
    .unwrap();
    let pool = db.view(|tx| load_pool_from_tx(tx)).unwrap();
    db.update(|tx| save_pool_to_tx(tx, &pool)).unwrap();
    let (extra, raw): (String, String) = db
        .view(|tx| {
            Ok(tx.query_row(
                "SELECT a.extra, q.raw_extra FROM accounts a JOIN account_quota q ON q.account_id = a.id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    let extra: serde_json::Value = serde_json::from_str(&extra).unwrap();
    let raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(extra["future_field"]["x"], 1);
    assert_eq!(extra["note"], "keep me");
    assert_eq!(raw["vendor_bucket"], serde_json::json!([1, 2, 3]));
}
```

(If Go stores known fields inside `extra` too, keep them as Go does; the assertion only requires unknown keys to survive.)

- [ ] **Step 2: Run** `AGY_TEST_MODE=1 cargo test --lib storage::` — expected: compile failure.
- [ ] **Step 3: Port the implementation** from `db.go`, the model files and pool/migration files, in Go's order, with identical SQL text.
- [ ] **Step 4: Run** `AGY_TEST_MODE=1 cargo test --lib storage::` — expected: all ported tests + `unknown_extra_keys_round_trip` pass.
- [ ] **Step 5: Commit** — `feat(rust): port storage database and pool`.

### Task 5: Port in-flight leases and prove Go↔Rust share one database

**Files:**
- Create: `rust/src/storage/inflight.rs`, `rust/interop/goside/main.go`, `rust/tests/interop.rs`
- Modify: `rust/src/storage/mod.rs`
- Spec: `internal/storage/inflight.go` and the 5 `TestReserveInFlight*`/`TestEnsureLeaseColumns*` tests

**Interfaces:**
- Consumes: `StateDb` from Task 4, `observability::{set_remote_in_flight, record_lease_fallback}`.
- Produces: `reserve_in_flight(pick: impl FnOnce(&HashMap<String, i64>) -> String) -> Result<(String, LeaseGuard), StorageError>`, `lease_in_flight(account_id: &str) -> Result<LeaseGuard, StorageError>`, `ensure_lease_columns(&Connection)`; `LeaseGuard` deletes its row on `Drop`. PID namespace from `std::fs::read_link("/proc/self/ns/pid")`, liveness via `libc::kill(pid, 0)`, same prune throttle and `leaseMaxAge` as Go.

- [ ] **Step 1: Port the 5 Go lease tests** (same names, snake_case).

- [ ] **Step 2: Write the Go side of the interop test** — `rust/interop/goside/main.go` (built from the repo root module so it can import `internal/storage`):

```go
// Command goside drives the Go storage package against a shared state DB for rust/tests/interop.rs.
package main

import (
	"bufio"
	"fmt"
	"os"

	"github.com/vlxlv/agy-pool-go/internal/config"
	"github.com/vlxlv/agy-pool-go/internal/storage"
)

func main() {
	config.SetTestMode(true)
	if err := config.ConfigureDataDir(os.Args[1]); err != nil {
		fmt.Println("ERR", err)
		os.Exit(1)
	}
	switch os.Args[2] {
	case "seed": // create the DB and two accounts
		err := storage.PoolTransaction(func(p *storage.Pool) error {
			p.Accounts = append(p.Accounts,
				&storage.Account{ID: "acc_1", Name: "Main"},
				&storage.Account{ID: "acc_2", Name: "Work"})
			return nil
		})
		fmt.Println("OK", err)
	case "hold": // hold a lease on acc_1 until stdin closes
		release, err := storage.LeaseInFlight("acc_1")
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		fmt.Println("HELD")
		bufio.NewReader(os.Stdin).ReadString('\n')
		release()
	case "remote": // print what Go sees as other processes' in-flight
		_, release, err := storage.ReserveInFlight(func(remote map[string]int64) string {
			fmt.Printf("REMOTE acc_1=%d\n", remote["acc_1"])
			return "acc_2"
		})
		if err == nil {
			release()
		}
	case "names": // print account names Go loads
		p, err := storage.LoadPool()
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		for _, a := range p.Accounts {
			fmt.Println("NAME", a.ID, a.Name)
		}
	}
}
```

Check the exact `Account` field names and `PoolTransaction`/`LoadPool` signatures against `internal/storage` before running; adjust this file, not the storage package.

- [ ] **Step 3: Write the Rust side** — `rust/tests/interop.rs` (pins Review Focus #2):

```rust
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn goside(dir: &std::path::Path) -> Command {
    let mut c = Command::new("go");
    c.current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .args(["run", "./rust/interop/goside"])
        .arg(dir)
        .env("AGY_TEST_MODE", "1");
    c
}

#[test]
fn go_and_rust_share_leases_and_pool() {
    let dir = tempfile::tempdir().unwrap();
    agy_pool::config::set_test_mode(true);
    agy_pool::config::configure_data_dir(dir.path()).unwrap();

    // Go creates the DB; Rust reads it and renames an account.
    let out = goside(dir.path()).arg("seed").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("OK <nil>"), "{out:?}");
    agy_pool::storage::pool_transaction(|p| {
        p.accounts.iter_mut().find(|a| a.id == "acc_2").unwrap().name = "Renamed".into();
        Ok(())
    })
    .unwrap();
    let names = goside(dir.path()).arg("names").output().unwrap();
    assert!(String::from_utf8_lossy(&names.stdout).contains("NAME acc_2 Renamed"));

    // Go holds a lease; Rust must see it as remote in-flight.
    let mut held = goside(dir.path()).arg("hold").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(held.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "HELD");
    let mut seen = -1;
    let (_, rust_lease) = agy_pool::storage::reserve_in_flight(|remote| {
        seen = remote.get("acc_1").copied().unwrap_or(0);
        "acc_2".into()
    })
    .unwrap();
    assert_eq!(seen, 1, "Rust must see the Go process's lease on acc_1");

    // Reverse direction: release both, then a Rust lease on acc_1 must show up in Go's remote view.
    held.stdin.take().unwrap().write_all(b"\n").unwrap();
    held.wait().unwrap();
    drop(rust_lease);
    let _rust_acc1 = agy_pool::storage::lease_in_flight("acc_1").unwrap();
    let remote = goside(dir.path()).arg("remote").output().unwrap();
    assert!(String::from_utf8_lossy(&remote.stdout).contains("REMOTE acc_1=1"), "{remote:?}");
}
```

- [ ] **Step 4: Run to verify failure, implement `inflight.rs`, run to verify pass**

Run: `cd rust && AGY_TEST_MODE=1 cargo test --lib storage::inflight && AGY_TEST_MODE=1 cargo test --test interop`
Expected before implementing: compile failure. After: 5 lease tests + `go_and_rust_share_leases_and_pool` pass.

- [ ] **Step 5: Phase gate and commit**

```bash
cd rust && cargo fmt --check && cargo clippy --all-targets -- -D warnings && AGY_TEST_MODE=1 cargo test
cd .. && export PATH=$HOME/sdk/go1.26.10/bin:$PATH && go vet ./rust/interop/goside && go test ./internal/storage
git add rust && git commit -m "feat(rust): port in-flight leases with Go interop test"
```

Expected: whole Rust suite green; Go storage tests still green (nothing in `internal/` changed).

---

## After Phase 1

Claude reviews the branch (`git diff main...rust-rewrite`), re-runs the phase gate, and confirms no test wrote outside temp dirs (`find ~/.local/share/agy-pool -newer rust/Cargo.toml` is empty). Then the Phase 2 plan is written in `docs/superpowers/plans/` using the interfaces Phase 1 actually produced.

---

## Phase 6 — Repo split (outline; detailed plan written after Phase 5)

Until cutover the Rust port stays in this repo, because its strongest checks depend on the Go tree: the Go↔Rust differential, interop and daemon-child tests run `go build ./cmd/agy-pool` from the repo root; `rust/interop/goside` imports Go `internal/` packages (only legal inside this module); golden cases are generated by running Go in temporary `internal/*/zz_*_test.go` files; `testdata/` and `go.mod` are shared with CI. A Go fix before cutover lands with its Rust counterpart in one commit, checked by the differential test.

Start only when the Rust gateway has served live traffic for a few days after the Phase 5 cutover with no rollback.

1. Tag the last Go state `go-final` on `main` (after `rust-rewrite` is merged).
2. Create `vlxlv/agy-pool-rust` from `rust/` with history: `git filter-repo --subdirectory-filter rust` on a fresh clone; copy `testdata/` fixtures the Rust tests read into the new repo.
3. Point the Rust `update` command (`releasesAPI` equivalent) at `vlxlv/agy-pool-rust` releases; publish the first release there; one last `agy-pool-go` release notes the move so `agy-pool update` users on Rust get the new channel.
4. Retire or relocate the Go-dependent tests: drop `differential.rs`/`interop.rs` (their job ends at cutover), or keep them in `agy-pool-rust` CI by checking out `agy-pool-go` at `go-final` and building the Go binary and `goside` from that checkout (`goside` moves to `agy-pool-go` as `cmd/interop-goside` before the tag).
5. Move CI (`rust.yml` incl. the stress job) and the systemd unit/docs to the new repo; archive `agy-pool-go` with a README pointing to `agy-pool-rust`.
