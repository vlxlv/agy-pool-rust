# agy-pool Rust Rewrite — Phase 4 (CLI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Execution in this repo:** each task is delegated to agy (agy-worker) with the task text as instructions and "the listed tests pass" as acceptance; Claude reviews the diff and re-runs the tests. Task 17 runs alone first; Tasks 18–23 then run in parallel git worktrees (at most 3 at a time on this 2-CPU host); Task 24 integrates.

**Goal:** Port the command-line interface — `internal/cli` (every command, including `run`, `top`, `update` and the `eval-*` modes) and `internal/installer` — so the Rust binary is a drop-in `agy-pool` for every command the Go binary accepts.

**Architecture:** `rust/src/cli/mod.rs` is the dispatcher (`cli.go`): it parses the command name and routes to one handler function per command, each living in its own file owned by exactly one task, so parallel tasks never edit the same file. Handlers take an `Io` value (stdin/stdout/stderr plus the real file descriptors behind them, standing in for Go's `w.(*os.File)` TTY checks) and return Go's exit code. Every process-wide hook Go tests swap (`ExecHandler`, `LoginOpener`, `EntrypointProvider`, `releasesAPI`, installer roots) becomes a guarded setter whose default refuses dangerous actions under `cfg(test)`. One shared test helper, `cli::test_support::cli_test_env()`, reproduces Go's `setupCLITestEnv` and is the only way CLI tests set up state.

**Tech Stack:** Phase 1–3 stack + `qrcode` (`default-features = false`, `render::unicode::Dense1x2` for the half-block QR that `go-qrcode`'s `ToSmallString` prints), `libc` for terminal raw mode / window size / `openpty` in tests (replacing `golang.org/x/term`), `std::os::unix::process::CommandExt::exec` (replacing `syscall.Exec`), `tokio::signal` (SIGWINCH for `top`).

**Spec:** `docs/superpowers/plans/2026-11-05-rust-rewrite.md` (master: Global Constraints, Review Focus, Phase 6 note on the release channel), the Phase 2 and Phase 3 plans (Global Constraints apply), the Go source and tests of `internal/cli` (101 tests) and `internal/installer` (22 tests), `cmd/agy-pool/main.go`, and `testdata/cli/golden_cases.json` (17 cases).

## Global Constraints

All Global Constraints of the master, Phase 2 and Phase 3 plans apply — in particular: run every Rust command on this host through `scripts/rust-test.sh` (wrapped in `timeout 900`); never bare `cargo test`/`cargo run`; never run a built agy-pool binary directly from the shell. In addition:

- **Output parity:** stdout/stderr text is byte-identical to Go (wording, spacing, ANSI colours, column widths) and exit codes are identical. Every Go assertion on output is ported as an assertion on the same text.
- **One owner per file:** a task creates or edits only the files in its own **Files** block. Nobody but Task 17 edits `rust/src/cli/mod.rs`, `rust/src/main.rs` or `rust/Cargo.toml`; nobody but Task 24 edits `rust/PORTING_NOTES.md`. Each task writes its Go-test→Rust-test mapping to its own `rust/porting-notes/phase4-task-NN.md` (one line per Go test: `GoTestName -> rust::path::test_name` or `GoTestName -> NOT PORTED: <reason>`).
- **No real side effects from tests** (this host runs the live gateway, its installed binaries and agy):
  - tests never execute a host `agy`, `agy-native` or installed `agy-pool`; the default `exec_handler` and the default browser opener panic under `cfg(test)`;
  - `start`/`restart` tests set `daemon::set_entrypoint_provider` to `/bin/true` (via `cli_test_env`);
  - `update` tests point `releases_api` at a loopback test server and `update_target_path` at a temp file; the default `update_target_path` refuses under `cfg(test)`;
  - installer tests always use per-test temp roots via `installer::set_test_installer_roots` (never Go's shared `/tmp/agy_test_sandbox` fallback); under `cfg(test)` `get_installer_roots()` passes every root through `config::deny_live_path`;
  - no network except loopback listeners the test created; no port 8899/8901.
- **Shared state:** every CLI test calls `crate::cli::test_support::cli_test_env()` first and keeps the returned guard alive for the whole test (it holds `config::TEST_LOCK`). Tests that only exercise pure functions (width/ANSI helpers, parsers) may skip it.
- **Async:** handlers are `async fn` (domain calls such as `accounts::switch_account` are async); tests are `#[tokio::test]`.

## Review Focus

1. **Output piped or redirected (not a TTY), or a narrow terminal** — expected: the same layout Go picks for that width (no QR code, no ANSI cursor control, no panic on width 0). Pinned in Task 18 by the ported responsive-width tests plus `list_and_quota_on_non_tty_use_go_default_width`.
2. **`agy-pool run` when the gateway is down, the native agy is missing, or the token sync fails** — expected: Go's error text and exit code and no `exec`. Pinned in Task 19 by the ported `run_cmd_test.go` and `TestCP1DRunSyncFailureDoesNotExec`.
3. **`agy-pool update` with a checksum mismatch or a download that fails midway** — expected: the installed binary is byte-for-byte unchanged and the exit code is non-zero. Pinned in Task 19 (`update_checksum_mismatch_leaves_binary_untouched`).
4. **`agy-pool uninstall` when `~/.local/bin/agy` or the managed native copy is not ours** (user binary, symlink swapped, hard link swapped) — expected: left in place, refusal message as Go. Pinned in Task 22 by the ported ownership/TOCTOU uninstall tests.
5. **`agy-pool top` exits by `q`, Ctrl-C, or an error while the terminal is in raw mode** — expected: the terminal mode is restored on every exit path. Pinned in Task 21 (`top_restores_terminal_mode_on_every_exit`, using a pty from `libc::openpty`).

---

## File Structure

- `rust/src/cli/mod.rs` — dispatcher `main`, `print_help`, colours, `Io`, `parse_instance_flags`, `resolve_context_for_cmd`, `redact_user_url`, the inline commands of `cli.go` (version, help, doctor, import-current, verify, usage errors), `exec_handler`/`login_opener` hooks (Task 17).
- `rust/src/cli/{width,term,render,qrcode}.rs` — `width.go`; libc terminal helpers; shared render helpers from `accounts_cmd.go` used by `top` too; `qrcode.go` (Task 17).
- `rust/src/cli/test_support.rs` — `cli_test_env()` (Task 17).
- `rust/src/cli/{accounts,status,daemon_cmd,run,update,order,forecast,stats,top,install,eval}.rs` — one per command group; created as stubs by Task 17, filled by Tasks 18–23.
- `rust/src/installer/{mod,roots}.rs` — `internal/installer` (`roots.rs` by Task 17, the rest by Task 22).
- `rust/src/daemon/pid.rs` — modified by Task 17: `set_entrypoint_provider`.
- `rust/src/main.rs` — modified by Task 17: dispatch to `cli::main`.
- `rust/tests/cli_golden.rs` — golden cases against both binaries (Task 24). `rust/tests/eval_differential.rs` — eval modes Go vs Rust (Task 23).
- `rust/porting-notes/phase4-task-NN.md` — per-task test mapping, merged into `PORTING_NOTES.md` by Task 24.

### Task 17: CLI foundation — dispatcher, shared helpers, hooks, test env

Runs alone, before every other Phase 4 task. Its commit is the base of every Phase 4 worktree.

**Files:**
- Create: `rust/src/cli/{mod,width,term,render,qrcode,test_support}.rs`, stub files `rust/src/cli/{accounts,status,daemon_cmd,run,update,order,forecast,stats,top,install,eval}.rs`, `rust/src/installer/{mod,roots}.rs`, `rust/porting-notes/phase4-task-17.md`
- Modify: `rust/src/lib.rs` (`pub mod cli; pub mod installer;`), `rust/src/main.rs`, `rust/src/daemon/pid.rs`, `rust/Cargo.toml` (`qrcode = { version = "<latest>", default-features = false }`)
- Spec: `internal/cli/cli.go`, `width.go`, `qrcode.go`, the shared helpers in `accounts_cmd.go` (`terminalSize`, `terminalWidth`, `chooseBarWidth`, `truncateRunes`, `getAccountState`, `extractResetTimes`, `weeklyBlocked`, `gatedBar`), `internal/installer/roots.go`, `cmd/agy-pool/main.go`
- Tests to port: `cli_test.go` (all 6), `width_test.go` (4), `qrcode_test.go` (3), `cp1d_test.go`: `TestRedactUserURL`, `TestCP1DVerifyRedactsSensitiveURL`, `TestCP1DRequiredCommandFailures`; `accounts_cmd_test.go`: `TestChooseBarWidth`, `TestGetAccountState_Protected`, `TestWeeklyBlockedDimsOtherBars`

**Interfaces (Produces — every later task relies on these exact names):**

```rust
// rust/src/cli/mod.rs
pub struct Io<'a> {
    pub stdin: &'a mut dyn std::io::BufRead,
    pub stdout: &'a mut dyn std::io::Write,
    pub stderr: &'a mut dyn std::io::Write,
    /// fd behind stdout when it is the process's real stdout (Go's `w.(*os.File)`); None for buffers.
    pub stdout_fd: Option<i32>,
    /// fd behind stdin when it is the process's real stdin; None for buffers.
    pub stdin_fd: Option<i32>,
}
pub const CLR_RESET: &str = "\x1b[0m"; // + CLR_BOLD, CLR_DIM, CLR_GREEN, CLR_YELLOW, CLR_BLUE, CLR_MAGENTA, CLR_CYAN, CLR_RED exactly as cli.go
pub async fn main(args: &[String], io: &mut Io<'_>) -> i32;
pub struct InstanceFlags { pub config_file: String, pub data_dir: String, pub port: i64, pub rest: Vec<String> }
pub fn parse_instance_flags(args: &[String]) -> Result<InstanceFlags, String>;
pub fn resolve_context_for_cmd(args: &[String]) -> Result<(crate::daemon::InstanceContext, Vec<String>), String>;
pub fn redact_user_url(raw: &str) -> (String, bool);
pub type ExecFn = fn(binary: &std::path::Path, argv: &[String], env: &[(String, String)]) -> std::io::Error;
pub fn set_exec_handler(f: Option<ExecFn>);      // None = default
pub fn exec_handler() -> ExecFn;                 // default: CommandExt::exec; under cfg(test) the default panics
pub type OpenerFn = fn(url: &str) -> Result<(), String>;
pub fn set_login_opener(f: Option<OpenerFn>);
pub fn login_opener() -> Option<OpenerFn>;

// rust/src/daemon/pid.rs
pub fn set_entrypoint_provider(f: Option<fn() -> std::path::PathBuf>); // None = get_entrypoint_path
pub fn entrypoint() -> std::path::PathBuf; // used everywhere Go calls EntrypointProvider()

// rust/src/installer/roots.rs — port of roots.go
pub struct InstallerRoots { pub user_home: PathBuf, pub bin_dir: PathBuf, pub config_dir: PathBuf, pub data_dir: PathBuf, pub libexec_dir: PathBuf, pub system_root: PathBuf }
pub fn set_test_installer_roots(roots: Option<InstallerRoots>);
pub fn get_installer_roots() -> InstallerRoots; // cfg(test): every non-empty root passed through config::deny_live_path
```

Handler stubs (Task 17 writes each with this exact signature and the body `let _ = writeln!(io.stderr, "agy-pool (rust): command not ported yet"); 2` or `false`/`Err(..)` as the return type needs; the owning task replaces the body):

| File | Owner | Signatures |
|---|---|---|
| `accounts.rs` | Task 18 | `list_cmd(io, args) -> i32`, `quota_cmd(io, args) -> i32`, `rename_account(io, target: &str, name: &str) -> bool`, `remove_account(io, target) -> bool`, `disable_account(io, target) -> bool`, `enable_account(io, target) -> bool`, `switch_account(io, target) -> bool`, `handle_export(io, args) -> i32`, `handle_import(io, args) -> i32`, `handle_migrate_legacy(io, args) -> i32`, `login_cmd(io) -> i32` |
| `status.rs` | Task 19 | `manage_strategy(io, target: &str) -> bool`, `status_cmd(io, args) -> i32` |
| `daemon_cmd.rs` | Task 19 | `handle_logs`, `start_daemon_cmd`, `stop_daemon_cmd`, `restart_daemon_cmd` — all `(io, args) -> i32` |
| `run.rs` | Task 19 | `run_agy_with_lb(io, extra: &[String]) -> i32`, `run_agy_direct(io, extra: &[String]) -> i32` |
| `update.rs` | Task 19 | `update_cmd(io, args) -> i32` |
| `order.rs` | Task 20 | `move_cmd`, `swap_cmd`, `sort_cmd`, `reorder_cmd`, `config_cmd` — all `(io, args) -> i32` |
| `forecast.rs` | Task 20 | `forecast_cmd(io, args) -> Result<(), String>` |
| `stats.rs` | Task 20 | `stats_cmd(io, args) -> i32` |
| `top.rs` | Task 21 | `top_cmd(io, args) -> i32` |
| `install.rs` | Task 22 | `handle_install`, `handle_repair`, `handle_uninstall` — all `(io, args) -> i32` |
| `eval.rs` | Task 23 | `handle_eval(cmd: &str, args: &[String], io: &mut Io<'_>) -> bool` |

All handlers are `pub async fn` taking `io: &mut Io<'_>` first and `args: &[String]` (the arguments after the command name, as Go passes them).

- [ ] **Step 1: Write `test_support.rs` exactly as below** (the port of Go's `setupCLITestEnv`; field order matters — guards that restore global state are declared before `_lock`, so they drop while the lock is still held):

```rust
#![cfg(test)]
use std::path::PathBuf;

pub struct CliTestEnv {
    pub dir: tempfile::TempDir,
    pub state_dir: PathBuf,
    pub test_home: PathBuf,
    _sandbox: crate::config::SyntheticSandboxGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for CliTestEnv {
    fn drop(&mut self) {
        // Runs before the fields drop, i.e. while _lock is still held.
        crate::installer::set_test_installer_roots(None);
        crate::daemon::set_entrypoint_provider(None);
        crate::cli::set_exec_handler(None);
        crate::cli::set_login_opener(None);
        crate::config::set_native_agy_dir(std::path::Path::new(""));
        crate::observability::reset_in_flight();
        crate::proxy::model_cooldown::reset_model_cooldowns();
        crate::proxy::affinity::affinity_manager().clear();
        crate::proxy::clear_all_verification_confirmation_claims();
        crate::config::reset_data_dir();
    }
}

/// Port of Go's setupCLITestEnv. Hold the returned value for the whole test.
pub fn cli_test_env() -> CliTestEnv {
    let lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::config::set_test_mode(true);
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join(".gemini");
    std::fs::create_dir_all(&state_dir).unwrap();
    let sandbox = crate::config::set_synthetic_sandbox_root(dir.path());
    crate::config::set_native_agy_dir(&state_dir);
    crate::config::configure_state_dir(&state_dir).unwrap();
    let test_home = dir.path().join("test_home");
    for sub in [".local/bin", ".config/agy-pool", ".local/share/agy-pool", ".local/libexec/agy-pool"] {
        std::fs::create_dir_all(test_home.join(sub)).unwrap();
    }
    crate::installer::set_test_installer_roots(Some(crate::installer::InstallerRoots {
        user_home: test_home.clone(),
        bin_dir: test_home.join(".local/bin"),
        config_dir: test_home.join(".config/agy-pool"),
        data_dir: test_home.join(".local/share/agy-pool"),
        libexec_dir: test_home.join(".local/libexec/agy-pool"),
        system_root: dir.path().join("test_system"),
    }));
    crate::daemon::set_entrypoint_provider(Some(|| PathBuf::from("/bin/true")));
    crate::observability::reset_in_flight();
    crate::proxy::model_cooldown::reset_model_cooldowns();
    crate::proxy::affinity::affinity_manager().clear();
    CliTestEnv { dir, state_dir, test_home, _sandbox: sandbox, _lock: lock }
}

/// Runs the dispatcher with in-memory streams; returns (exit code, stdout, stderr).
pub async fn run_cli(args: &[&str], stdin: &str) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = {
        let mut io = crate::cli::Io { stdin: &mut input, stdout: &mut out, stderr: &mut err, stdout_fd: None, stdin_fd: None };
        crate::cli::main(&args, &mut io).await
    };
    (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
}
```

If a name used above does not exist yet (e.g. `crate::proxy::clear_all_verification_confirmation_claims` path, `SyntheticSandboxGuard` export), fix the path to the existing item — do not drop the reset.

- [ ] **Step 2: Port the listed tests** into `#[cfg(test)] mod tests` blocks of the files they test, each starting with `let _env = crate::cli::test_support::cli_test_env();` (pure helper tests excepted). Add `entrypoint_provider_and_exec_default_are_safe_in_tests`: assert `daemon::entrypoint()` is `/bin/true` inside `cli_test_env`, and that calling the default `exec_handler()` panics (`std::panic::catch_unwind`).
- [ ] **Step 3: Run** `timeout 900 scripts/rust-test.sh test --lib cli:: -- --test-threads=16` — expected: compile failure.
- [ ] **Step 4: Implement** the Files above: dispatcher with every `case` of `cli.go` (aliases included) routing to the stub handlers or the inline logic `cli.go` has; `main.rs` mirroring `cmd/agy-pool/main.go` (daemon-child first, then `eval-*`/`proxy-test-server` via `cli::eval::handle_eval` — keep the existing `proxy-test-server` code by moving it into `eval.rs` — then `cli::main` with the real stdio and `stdout_fd: Some(1)`, `stdin_fd: Some(0)`, exiting with the code when non-zero); `term.rs` with `is_terminal(fd)`, `get_size(fd) -> Option<(u16, u16)>` (`ioctl TIOCGWINSZ`), `RawModeGuard::enable(fd)` (`tcgetattr`/`cfmakeraw`/`tcsetattr`, restores in `Drop`).
- [ ] **Step 5: Run** the Step 3 command — all pass. Then the gate: `timeout 900 scripts/rust-test.sh test -- --test-threads=16`, `timeout 900 scripts/rust-test.sh clippy --all-targets -- -D warnings`, `(cd rust && cargo fmt --check)`.
- [ ] **Step 6: Commit** — `feat(rust): CLI dispatcher, shared helpers and test env`.

### Task 18: Account and listing commands

**Files:** Modify `rust/src/cli/accounts.rs`, `rust/src/accounts/mod.rs` (only: `default_browser_opener` panics under `cfg(test)`); Create `rust/porting-notes/phase4-task-18.md`
- Spec: `internal/cli/accounts_cmd.go` (all but the Task 17 helpers), `status_cmd.go`: `ListCmd`, `QuotaCmd`, `refreshAllQuotas`, `formatLastUsed`
- Tests to port: `accounts_cmd_test.go` (the 17 not in Task 17), `status_cmd_test.go`: `TestListCmd_CompactAndPrivacy`, `TestListCmd_ShowEmail`, `TestQuotaUnknownIsExplicitAcrossLayouts`, `TestQuotaPartialWindowsDisplayIndependently`, `TestQuotaCmd_ContainsProgressBars`, `TestQuotaCmd_ShowEmail`, `TestQuotaCmd_AutoHidesUnusedThirdPartyWindows`, `TestQuotaCmd_SkipsLiveFetchForFreshSnapshots`, `TestResolveNextCandidateID_SkipsDaemonPausedAccounts`, `TestResolveNextCandidateID_FollowsDaemonSmartRotation`
- Login tests use a loopback mock OAuth server (as Go's `setupMockOAuthTokenServer`) and `cli::set_login_opener`; never the default opener.

- [ ] **Step 1: Port the tests** (each starts with `cli_test_env()`), plus `list_and_quota_on_non_tty_use_go_default_width`: run `list` and `quota` through `run_cli` (stdout is a buffer, so `stdout_fd: None`) on a 3-account pool and assert the output equals what Go's `ListAccounts` prints for its non-TTY default width (generate the expected text by running the Go test helper once in a temporary `internal/cli/zz_*_test.go`, then delete that file).
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --lib cli::accounts -- --test-threads=16` — compile failure / failures.
- [ ] **Step 3: Implement** `accounts.rs`.
- [ ] **Step 4: Run** Step 2's command — all pass; then the full gate (as Task 17 Step 5).
- [ ] **Step 5: Commit** — `feat(rust): port account and listing commands`.

### Task 19: Status, daemon control, run and update

**Files:** Modify `rust/src/cli/{status,daemon_cmd,run,update}.rs`; Create `rust/porting-notes/phase4-task-19.md`
- Spec: `status_cmd.go` (`ManageStrategy`, `StatusCmd`), `daemon_cmd.go` (`handleLogs`, `Start/Stop/RestartDaemonCmd`; `runDaemonChild` is already `daemon::run_daemon_child`), `run_cmd.go`, `update_cmd.go`
- Tests to port: `status_cmd_test.go`: `TestStrategyCommand`, `TestStatusCmd_NoQuotaBarsAndDashboard`, `TestStatusCmd_RunningDaemonWithRuntimeStats`, `TestStatusCmd_RunningDaemonStatsFromDaemonNotCLI`, `TestStatusCmd_RunningDaemonStatsUnavailable`, `TestAuditRouting_RemovedFromCLI`; `multi_instance_cli_test.go` (2); `observability_test.go` (1); `daemon_cmd_test.go` (2); `run_cmd_test.go` (11); `update_cmd_test.go` (1); `cp1d_test.go`: `TestCP1DRunSyncFailureDoesNotExec`
- `update.rs` exposes `set_releases_api(Option<String>)` and `set_update_target_path(Option<fn() -> PathBuf>)`; the default target is `daemon::entrypoint()`, which under `cfg(test)` is refused (panic) unless a test target is set. Replacing the binary writes a temp file in the target's directory and renames it over the target only after the checksum matched.
- `run.rs` calls `cli::exec_handler()` for the final exec; tests install a recording fake.

- [ ] **Step 1: Port the tests**, plus `update_checksum_mismatch_leaves_binary_untouched`: loopback server serves a release whose asset bytes do not match its checksum entry; the temp target file's bytes and mode are unchanged afterwards and the exit code is non-zero; and `update_download_failure_leaves_binary_untouched` (server closes the connection mid-body).
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --lib cli::status cli::daemon_cmd cli::run cli::update -- --test-threads=16` — failures.
- [ ] **Step 3: Implement** the four files.
- [ ] **Step 4: Run** Step 2's command — all pass; then the full gate.
- [ ] **Step 5: Commit** — `feat(rust): port status, daemon control, run and update commands`.

### Task 20: Order, forecast and stats commands

**Files:** Modify `rust/src/cli/{order,forecast,stats}.rs`; Create `rust/porting-notes/phase4-task-20.md`
- Spec: `order_cmd.go`, `forecast_cmd.go`, `stats_cmd.go`
- Tests to port: `order_cmd_test.go` (6), `forecast_cmd_test.go` (4), `stats_cmd_test.go` (2)

- [ ] **Step 1: Port the tests.**
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --lib cli::order cli::forecast cli::stats -- --test-threads=16` — failures.
- [ ] **Step 3: Implement** the three files (domain logic already exists: `accounts::order`, `forecast`, `diagnostics::compute_effectiveness_report`).
- [ ] **Step 4: Run** Step 2's command — all pass; then the full gate.
- [ ] **Step 5: Commit** — `feat(rust): port order, forecast and stats commands`.

### Task 21: `top` dashboard

**Files:** Modify `rust/src/cli/top.rs`; Create `rust/porting-notes/phase4-task-21.md`
- Spec: `top_cmd.go`, `sigwinch_posix.go`
- Tests to port: `top_cmd_test.go` (15). `RenderTopFrame*` are pure renderers — port them as `render_top_frame*` and compare full frame text with Go where Go does.
- Raw mode only via `cli::term::RawModeGuard` (restores on `Drop`); key reading on a blocking thread; SIGWINCH via `tokio::signal::unix`.

- [ ] **Step 1: Port the tests**, plus `top_restores_terminal_mode_on_every_exit`: open a pty with `libc::openpty`, run `top_cmd` with `stdin_fd`/`stdout_fd` set to the pty's slave fd, record `tcgetattr` before; exit once by writing `q` to the master, once by Ctrl-C (`\x03`), once by making the render fail (pool load error); after each, `tcgetattr` equals the recorded state.
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --lib cli::top -- --test-threads=16` — failures.
- [ ] **Step 3: Implement** `top.rs`.
- [ ] **Step 4: Run** Step 2's command — all pass; then the full gate.
- [ ] **Step 5: Commit** — `feat(rust): port top dashboard`.

### Task 22: Installer and install/repair/uninstall commands

**Files:** Modify `rust/src/installer/mod.rs`, `rust/src/cli/install.rs`; Create `rust/porting-notes/phase4-task-22.md`
- Spec: `internal/installer/installer.go`, its 7 test files (22 tests), `internal/cli/install_cmd.go`, `install_cmd_test.go` (3)
- Every test uses `cli_test_env()` (which sets per-test installer roots) or sets its own temp `InstallerRoots`; `TestIntegration_AgyManagedShimAndRawBypass` runs the generated shims only against fake `agy` scripts inside those roots.
- Destructive operations go through `config::assert_safe_destructive_path` exactly where Go calls `AssertSafeDestructivePath`; the uninstall ownership and TOCTOU checks are ported line by line.

- [ ] **Step 1: Port the tests.**
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --lib installer:: cli::install -- --test-threads=16` — failures.
- [ ] **Step 3: Implement** the two files.
- [ ] **Step 4: Run** Step 2's command — all pass; then the full gate.
- [ ] **Step 5: Commit** — `feat(rust): port installer and install commands`.

### Task 23: `eval-*` modes and their Go↔Rust differential test

**Files:** Modify `rust/src/cli/eval.rs`; Create `rust/tests/eval_differential.rs`, `rust/porting-notes/phase4-task-23.md`
- Spec: `internal/cli/eval.go` (modes `eval-scheduler`, `eval-capacity`, `eval-account-target`, `eval-display-name`, `eval-jwt`, `eval-error-classifier`, `eval-crypto`, `eval-daemon` with actions `format_size`/`read_pid`/`get_daemon_info`/`rotate_log`/`clear_log`/`stop_daemon`, `eval-conversation`, `eval-resolve-continue`; `proxy-test-server` already ported and moved here by Task 17)
- Each mode reads JSON on stdin and prints JSON on stdout. The differential test builds the Go binary once (`go build -o <tmp>/agy-pool-go ./cmd/agy-pool`, as `rust/tests/differential.rs` does), runs both binaries with `AGY_TEST_MODE=1`, a temp `AGY_DATA_DIR` and the same stdin, and compares stdout as parsed JSON plus exit codes. At least two inputs per mode (one normal, one edge/invalid); `eval-crypto` uses a fixed salt/nonce input if the mode accepts one, otherwise compares only decrypt round-trips. `eval-daemon` paths are temp files only; `stop_daemon` only against a PID file that names a process the test spawned (`sleep 30`).

- [ ] **Step 1: Write `eval_differential.rs`** with one `#[test]` per mode.
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --test eval_differential -- --test-threads=1` — failures.
- [ ] **Step 3: Implement** `eval.rs`. A mismatch is a Rust bug unless the cause is unclear — then mark that test `#[ignore = "mismatch: ..."]` and describe the exact difference in the report; do not change Go code.
- [ ] **Step 4: Run** Step 2's command — all pass or ignored-with-reason; then the full gate.
- [ ] **Step 5: Commit** — `feat(rust): port eval modes with Go/Rust differential test`.

### Task 24: Integration, golden CLI cases and Phase 4 gate

Done by Claude after Tasks 18–23 are reviewed: cherry-pick each task's commits onto `rust-rewrite` in task order (they touch disjoint files), then:

**Files:** Create `rust/tests/cli_golden.rs`; Modify `rust/PORTING_NOTES.md` (append every `rust/porting-notes/phase4-task-NN.md`, then delete that directory)

- [ ] **Step 1: Write `cli_golden.rs`:** for every case in `testdata/cli/golden_cases.json`, run the Go binary and the Rust binary (`env!("CARGO_BIN_EXE_agy-pool")`) with `args`, empty stdin, `AGY_TEST_MODE=1`, and a fresh temp `HOME`, `AGY_DATA_DIR`, `AGY_CONFIG_FILE`, `AGY_GEMINI_DIR` per run; assert `exit_code`, `stdout_contains`, `stderr_contains`, and the `*_go` keys (they describe the Go family, which Rust must match) on both; ignore `exempt_py_parity`. Additionally assert Go and Rust stdout are identical after `StripANSI` for every case except `doctor_check` and `status_summary` (environment-dependent lines).
- [ ] **Step 2: Phase 4 gate**

```bash
(cd rust && cargo fmt --check)
timeout 900 scripts/rust-test.sh clippy --all-targets -- -D warnings
timeout 900 scripts/rust-test.sh test -- --test-threads=16
export PATH=$HOME/sdk/go1.26.10/bin:$PATH
go test -count=1 ./internal/cli ./internal/installer
git status --short   # must print nothing
if ls internal/*/zz_* 2>/dev/null; then echo "LEFTOVER zz FILE"; exit 1; fi
```

- [ ] **Step 3: Commit** — `test(rust): golden CLI cases against Go and Rust; Phase 4 notes`; push; check CI including the stress job.

---

## After Phase 4

Claude reviews `git diff 78f8605..rust-rewrite`, reads the merged `PORTING_NOTES.md` entries, and writes the Phase 5 plan (candidate gateway on port 8901 with a copied data dir, soak, cutover via an `agy-pool-go` release).
