# agy-pool Rust Rewrite — Phase 5 (Candidate & Cutover) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Execution in this repo:** code tasks (25–27) are delegated to agy with "the listed tests pass" as acceptance; Claude reviews diffs and re-runs tests. Tasks 28–29 touch the live host and the public release channel: Claude runs them step by step, and every step marked **[user]** waits for the user's explicit go-ahead.

**Goal:** Prove the Rust binary on real agy traffic next to the live Go gateway, then replace the Go binary through a normal `agy-pool-go` release, with a tested rollback.

**Architecture:** The candidate is the Rust release binary in its own directory, with a copy of the live `state.db`, its own `config.json` on port 8901, and its own data dir; it never writes the live data or config dirs. Traffic reaches it through `agy-pool run -c <candidate config> -D <candidate data>` (the same entry point agy-worker and the `agy` shim use). Cutover ships the Rust binary under the Go release's asset names, so the installed Go binary's `agy-pool update` downloads it, verifies its checksum and restarts the systemd unit through it.

**Tech Stack:** Phase 1–4 stack; GitHub Actions (`ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-15`), Rust musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) with `musl-tools`, Apple targets (`aarch64-apple-darwin`, `x86_64-apple-darwin`), `scripts/build-deb.sh`, Python 3 `sqlite3` backup API (no `sqlite3` CLI on this host).

**Spec:** master plan `docs/superpowers/plans/2026-11-05-rust-rewrite.md` (Phase 5 row: "Rust candidate on 8901 with a copied data dir serves real agy traffic for an agreed soak period with no no-replay or quota regressions vs Go; user approves the switch; the cutover release is published as an `agy-pool-go` release so the installed Go binary's `agy-pool update` reaches it; rollback = reinstall the Go binary"), `.github/workflows/release.yml`, `internal/cli/update_cmd.go`, `internal/daemon/{control,systemd}.go`, `~/.config/systemd/user/agy-pool.service`.

## Global Constraints

All Global Constraints of the master, Phase 2, 3 and 4 plans apply. In addition:

- **Live paths stay Go's until Task 29:** nothing in Tasks 25–28 writes `~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.local/bin`, `~/.local/libexec/agy-pool` or the systemd unit, and nothing stops, restarts or signals the live gateway (port 8899). The single shared live file during the soak is the native agy token file under `~/.gemini`, which every `agy-pool run` (Go or Rust) rewrites; Task 26 must prove Rust writes it byte-identically before Task 28 starts.
- **Candidate layout (fixed):** `~/agy-pool-candidate/bin/agy-pool` (binary), `~/agy-pool-candidate/config.json` (copy of the live config with `server.port` = `8901`), `~/agy-pool-candidate/data/` (copied `state.db`; own log, PID file, locks). Port 8901 only.
- **Release asset names unchanged:** `agy-pool-linux-amd64`, `agy-pool-linux-arm64`, `agy-pool-darwin-amd64`, `agy-pool-darwin-arm64`, `agy-pool_<ver>_{amd64,arm64}.deb`, `SHA256SUMS` — exactly what `update` (Go and Rust) looks up.
- **Linux release binaries are statically linked** (musl), like Go's `CGO_ENABLED=0` builds, so they run on Termux and old glibc.
- **The binary reports the release version:** `agy-pool --version` prints the tag version (without `v`), as Go's `-X config.Version=` injection does; `update` relies on it to decide "already up to date".
- **No `main`, tag or release changes without the user's explicit go-ahead in that moment** (Task 29 steps marked **[user]**).
- **Version bump inside the change commit:** the cutover commit bumps `internal/config/config.go` `Version` and `rust/Cargo.toml` `version` together, and that commit is tagged (no separate bump commit).

## Review Focus

1. **The installed Go binary runs `agy-pool update` and receives the Rust binary** — expected: download, checksum check, replace, then `restart` through the new Rust binary, which restarts the systemd unit (not a detached second daemon), and the gateway comes back on 8899. Pinned in Task 26 (`go_daemon_then_rust_restart_takes_over`, the unit test `rust_restart_uses_systemctl_when_unit_active`) and rehearsed in Task 29 Step 3 on a temp instance.
2. **Rollback after cutover** — expected: putting the Go binary back and restarting the unit gives a working Go gateway on the Rust-written `state.db`, PID file and logs. Pinned in Task 26 (`rust_daemon_then_go_restart_takes_over`) and rehearsed in Task 29 Step 3.
3. **`agy-pool run` through the Rust binary writes the native agy token file** — expected: the same bytes, mode and path as Go for the same pool. Pinned in Task 26 (`run_writes_same_token_file_and_env_as_go`).
4. **A Termux/Android or old-glibc host running the release binary** — expected: it starts without a dynamic loader error. Pinned in Task 25 (`file` reports "statically linked" for both Linux assets; the release smoke step runs the arm64 binary on the arm runner); the Termux device check itself is a **[user]** item in Task 29 Step 1 if the user runs agy-pool on Termux.
5. **The candidate during the soak hits a fault** (panic, gateway error, quota stays unknown) — expected: it is detected the same day, traffic goes back to Go with one command, and the live gateway is unaffected. Pinned in Task 27 (`scripts/rust-candidate.sh compare` reports panics, gateway errors and unknown-quota counts; `route go` restores routing) and Task 28's abort rules.

---

## File Structure

- `rust/build.rs` (new) or `rust/src/lib.rs` — `VERSION` from build-time `AGY_POOL_VERSION`, else `CARGO_PKG_VERSION` (Task 25).
- `.github/workflows/rust-release-dry-run.yml` (new) — builds all release assets from `rust/`, verifies them, uploads as a workflow artifact; never publishes (Task 25). Its build steps are moved into `release.yml` at cutover (Task 29).
- `rust/tests/lifecycle_differential.rs` (new) — `run`/`start`/`restart`/`stop` across Go and Rust binaries on temp instances (Task 26).
- `scripts/rust-candidate.sh` (new) — `setup | start | stop | status | compare | route go|rust` for the candidate (Task 27), with `scripts/rust-candidate-test.sh` exercising it against a fake HOME.
- `~/agy-worker/server.py` — outside the repo; a one-line, env-controlled switch of the run command, only with the user's approval (Task 28).
- `.github/workflows/release.yml`, `internal/config/config.go`, `rust/Cargo.toml` — cutover commit (Task 29).

### Task 25: Release build of the Rust binary (dry run)

**Files:**
- Modify: `rust/src/lib.rs` (the `VERSION` constant)
- Create: `.github/workflows/rust-release-dry-run.yml`
- Test: `rust/tests/version.rs` (extend)

**Interfaces:**
- Produces: `agy_pool::VERSION` = `option_env!("AGY_POOL_VERSION")` if set at build time, else `env!("CARGO_PKG_VERSION")`; the dry-run workflow artifact `rust-release-dist` containing the asset set from Global Constraints.

- [ ] **Step 1: Write the failing test** — in `rust/tests/version.rs` add:

```rust
#[test]
fn version_matches_build_time_override_or_cargo_version() {
    let want = option_env!("AGY_POOL_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agy-pool"))
        .arg("--version")
        .env("AGY_TEST_MODE", "1")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("agy-pool {want}\n"));
}
```

- [ ] **Step 2: Run** `AGY_POOL_VERSION=9.9.9-test timeout 900 scripts/rust-test.sh test --test version` — expected: FAIL (binary still prints the Cargo version).
- [ ] **Step 3: Implement** in `rust/src/lib.rs`:

```rust
/// Release builds inject the tag version (like Go's `-X config.Version=`); `update` compares against it.
pub const VERSION: &str = match option_env!("AGY_POOL_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
```

- [ ] **Step 4: Run** Step 2's command and the plain `timeout 900 scripts/rust-test.sh test --test version` — both PASS.
- [ ] **Step 5: Write `.github/workflows/rust-release-dry-run.yml`** (trigger: `workflow_dispatch` with input `version`, default `0.0.0-dryrun`; `permissions: contents: read`). Jobs:
  - `linux` matrix `{runner: ubuntu-24.04, target: x86_64-unknown-linux-musl, asset: agy-pool-linux-amd64, deb: amd64}` and `{runner: ubuntu-24.04-arm, target: aarch64-unknown-linux-musl, asset: agy-pool-linux-arm64, deb: arm64}`: `sudo apt-get install -y musl-tools`, `rustup target add $target`, `AGY_POOL_VERSION=${{ inputs.version }} cargo build --release --locked --target $target` in `rust/`, copy to `dist/$asset`, assert `file dist/$asset | grep -q 'statically linked'`, assert `dist/$asset --version` prints `agy-pool ${{ inputs.version }}`, run `dist/$asset --help`, then `./scripts/build-deb.sh $deb ${{ inputs.version }} dist/$asset dist`; upload `dist/` as artifact `linux-$deb`.
  - `darwin` on `macos-15`: targets `aarch64-apple-darwin` and `x86_64-apple-darwin` (both via `rustup target add`), assets `agy-pool-darwin-arm64` / `agy-pool-darwin-amd64`, run `--version` on the arm64 one; upload artifact `darwin`.
  - `sums` (needs both): download all artifacts into `dist/`, `sha256sum agy-pool-linux-amd64 agy-pool-linux-arm64 agy-pool-darwin-amd64 agy-pool-darwin-arm64 agy-pool_*_amd64.deb agy-pool_*_arm64.deb > SHA256SUMS`, `sha256sum -c SHA256SUMS`, assert the file list equals the Go release's list, upload `rust-release-dist`.
- [ ] **Step 6: Gate** (fmt, clippy, full suite as in Phase 4) and commit — `build(rust): release version injection and static release dry run`. After the user approves the push: push, run the workflow with `gh workflow run rust-release-dry-run.yml -f version=0.0.0-dryrun --ref rust-rewrite`, and confirm all jobs green and the artifact lists all 7 files.

### Task 26: Lifecycle and `run` differential (Go ↔ Rust)

**Files:**
- Create: `rust/tests/lifecycle_differential.rs`
- Modify: `rust/src/daemon/{systemd,control}.rs` (test hooks + unit test); other files under `rust/src/{cli,daemon,accounts}/` only if a test exposes a Rust difference

**Interfaces:**
- Consumes: the Go-binary build helper pattern of `rust/tests/differential.rs`; `env!("CARGO_BIN_EXE_agy-pool")`.

Every test builds an isolated instance: temp `HOME`, `AGY_DATA_DIR`, `AGY_CONFIG_FILE` (with `server.port` = a free port from `TcpListener::bind("127.0.0.1:0")`, never 8899/8901), `AGY_GEMINI_DIR`, `PATH` = a temp `bin/` (holding the fake `agy` and fake `systemctl` below) + `/usr/bin:/bin`, `AGY_BIN` = the fake agy, `AGY_TEST_MODE` unset where the scenario needs real daemon behaviour but `AGY_ENTRYPOINT_PATH` set to the binary under test. A `Drop` guard stops every daemon the test started (by PID from that instance's PID file only) and kills leftovers spawned by the test.

Fake `agy` (shell script written by the test): prints `ARGV:` + its args, `CLOUD_CODE_URL=` + that env var, and the contents of the token file path given in its env, then exits 0. Fake `systemctl` in the temp `bin/`: exits 1 with a message, so a black-box test that reaches it fails loudly.

- [ ] **Step 1: Write the tests:**
  - `run_writes_same_token_file_and_env_as_go`: seed both instances with the same pool (one account, access token valid for 1 h so no refresh), run `<bin> run -c <cfg> -D <data> -- --model x` with each binary; assert identical fake-agy output (argv, `CLOUD_CODE_URL`), identical token-file bytes and mode, both exit 0, and each started its own daemon listening on its port.
  - `go_daemon_then_rust_restart_takes_over`: start the instance with the Go binary (`start`), then run `restart` with the Rust binary; assert the port answers, the PID file names a live Rust process (`/proc/<pid>/exe` = Rust binary), the old Go PID is gone, and Go's `status` on the same instance reports running.
  - `rust_daemon_then_go_restart_takes_over`: the reverse (rollback path).
  - `rust_restart_uses_systemctl_when_unit_active` — a unit test in `rust/src/daemon/control.rs`, because supervision is detected from `/proc/<pid>/cgroup` (Go `systemdScope`), which a black-box test cannot fake. Add two `cfg(test)` hooks in `rust/src/daemon/systemd.rs`: `set_systemd_scope_override(Option<fn(i32) -> &'static str>)` (consulted by `systemd_scope`) and `set_systemctl_recorder(Option<fn(&str, &str)>)` (when set, `systemctl` records `(scope, verb)` instead of panicking; unset keeps today's panic). The test (under `test_env()`) spawns `sleep 30`, writes that PID into the instance PID file, sets the scope override to `"--user"` and a recorder, calls the restart path Go's `RestartInstance` takes, and asserts the recorder saw exactly `("--user", "restart")` and no new daemon process was spawned; the same for the stop path with `("--user", "stop")`. Both hooks are reset in the test's guard.
  - `run_with_daemon_down_and_agy_missing_matches_go`: `AGY_BIN` pointing to a missing file; same exit code and stderr (after ANSI strip) for both, and the fake agy never ran.
- [ ] **Step 2: Run** `timeout 900 scripts/rust-test.sh test --test lifecycle_differential -- --test-threads=1`. Any failure is a Rust bug unless Go itself behaves differently than the test expects (then fix the test's expectation to Go's actual behaviour and say so in the report).
- [ ] **Step 3: Fix** Rust until all pass; do not change Go code.
- [ ] **Step 4: Gate** (fmt, clippy, full suite) and commit — `test(rust): Go/Rust lifecycle and run differential`.

### Task 27: Candidate script

**Files:**
- Create: `scripts/rust-candidate.sh`, `scripts/rust-candidate-test.sh`

**Interfaces:**
- Produces: `scripts/rust-candidate.sh setup|start|stop|status|compare|route go|route rust`. All paths derive from `$HOME`; the script refuses (exit 1) if `CAND_DIR` (default `$HOME/agy-pool-candidate`) resolves inside any live path from Global Constraints.
  - `setup`: `cargo build --release --locked` in `rust/` (with `AGY_POOL_VERSION=<cargo version>-candidate`), copy to `$CAND_DIR/bin/agy-pool`; copy the live config with `server.port` set to 8901 (Python `json`, never editing the live file); copy the live `state.db` with the SQLite online backup API, opening the source read-only: `python3 -I -c 'import sqlite3,sys; s=sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close(); s.close()' <live state.db> <cand state.db>`. Refuses if `$CAND_DIR/data/state.db` exists unless `--fresh` (which moves the old data dir to `data.<timestamp>` rather than deleting it).
  - `start` / `stop` / `status`: `$CAND_DIR/bin/agy-pool {start|stop|status} -c $CAND_DIR/config.json -D $CAND_DIR/data`.
  - `compare [--since <hours>]`: prints, for Go (live, read-only) and the candidate side by side: `stats --json` effectiveness summary, the number of accounts whose Gemini 5H or weekly quota is unknown (`quota` output), `panicked` and `ERROR` line counts in each log since the window start, and agy-worker run outcomes in the window grouped by gateway (from `~/agy-worker/runs/*.json`, keyed by the run command recorded there).
  - `route go|rust`: writes `$CAND_DIR/route` (`go` or `rust`); the agy-worker switch of Task 28 reads this file, so rollback of soak traffic is `route go`.
- [ ] **Step 1: Write `scripts/rust-candidate-test.sh`:** creates a fake HOME with a fake live tree (`.local/share/agy-pool/state.db` made with Python `sqlite3` holding one row in `meta`, `.config/agy-pool/config.json` from `testdata` or a minimal valid one), records `sha256sum` of every file in the fake live tree, runs `HOME=<fake> scripts/rust-candidate.sh setup` with a prebuilt binary override `CAND_BIN=<path>` (skips cargo), then asserts: candidate config port is 8901 and other keys equal; candidate `state.db` has the `meta` row; live tree hashes unchanged; `setup` again without `--fresh` exits 1; `CAND_DIR=<fake>/.local/share/agy-pool/x scripts/rust-candidate.sh setup` exits 1; `route rust` then `route go` leaves `go` in the route file.
- [ ] **Step 2: Run** `bash scripts/rust-candidate-test.sh` — FAIL (script missing).
- [ ] **Step 3: Implement** `scripts/rust-candidate.sh` (bash, `set -euo pipefail`, `shellcheck`-clean).
- [ ] **Step 4: Run** Step 2's command — PASS; `shellcheck scripts/rust-candidate*.sh` clean if shellcheck is installed.
- [ ] **Step 5: Commit** — `feat(scripts): Rust candidate setup, compare and routing`.

### Task 28: Soak (live, Claude + user)

Not a code task: each step is run by Claude, checked, and reported. Steps marked **[user]** wait for approval.

- [ ] **Step 1:** `scripts/rust-candidate.sh setup` then `start`; check `status` reports running on 8901, `curl -s 127.0.0.1:8901/` answers like the Go gateway, and `systemctl --user is-active agy-pool` is still `active` with the same PID as before.
- [ ] **Step 2: Gemini quota with the agy User-Agent:** `~/agy-pool-candidate/bin/agy-pool quota -c ~/agy-pool-candidate/config.json -D ~/agy-pool-candidate/data` after a refresh — every enabled account shows numeric 5H and WK Gemini quota (none unknown), matching the Go gateway's `agy-pool quota` within normal drift.
- [ ] **Step 3: [user] route traffic.** Proposed: a one-line change in `~/agy-worker/server.py` (backup first as `server.py.bak-<timestamp>`) so the run command is `["agy-pool", "run"]` when `~/agy-pool-candidate/route` says `go` (or is missing) and `[<cand bin>, "run", "-c", <cand config>, "-D", <cand data>]` when it says `rust`; then `route rust`. The user may instead (or also) use the candidate interactively via an alias to that same command.
- [ ] **Step 4: Soak window** — at least 48 h and at least 30 agy-worker runs through the candidate. Run `scripts/rust-candidate.sh compare --since 24` once a day and report it.
- [ ] **Abort rules (any one → `route go`, `stop`, report):** a `panicked` line in the candidate log; an agy run failing with a gateway error (5xx from the gateway, connection refused, or a request replayed after the upstream connection opened); any account's Gemini quota unknown for more than one refresh cycle; the live gateway's PID or status changing because of the candidate.
- [ ] **Step 5: Gate:** zero aborts in the window; candidate run failure rate not above Go's for the same days; quota and effectiveness numbers in `compare` within normal drift. Report the evidence; **[user]** decides whether to cut over.

### Task 29: Cutover release and rollback

- [ ] **Step 1: [user] pre-release checks:** CI green on `rust-rewrite`, the Task 25 dry run green for the exact commit; if the user runs agy-pool on Termux, they run the dry-run `agy-pool-linux-arm64` artifact there with `--version` and `doctor` and report the result.
- [ ] **Step 2: Cutover commit** on `rust-rewrite`: replace the Go build steps of `release.yml` (`Build cross-platform release binaries`, `Build Debian packages`, the native verify step) with the Task 25 dry-run jobs (same asset names; `AGY_POOL_VERSION` = tag without `v`), make `test-gate` run the Rust gate (`cargo fmt --check`, `clippy -D warnings`, `cargo test` in `rust/`) in addition to the Go tests, keep `Generate and verify unified SHA256SUMS` and `Publish GitHub Release` unchanged; bump `internal/config/config.go` `Version` and `rust/Cargo.toml` `version` to the release version (proposed `0.2.0`); update README/install docs to say the binary is now Rust. Commit — `release: ship the Rust binary`.
- [ ] **Step 3: Rehearsal on a temp instance** (no live paths): download nothing; build the release binary locally, run the Go binary's `update` against a loopback releases server serving that binary + `SHA256SUMS`, with `update_target_path` = a temp copy of the Go binary and a temp instance running on a free port; assert the temp binary is now Rust, the temp instance restarted under Rust, and putting the Go binary back + `restart` returns to Go. (Automate as an `#[ignore]`d test in `rust/tests/lifecycle_differential.rs`, run explicitly with `-- --ignored`.)
- [ ] **Step 4: [user] merge and tag:** merge `rust-rewrite` into `main` (fast-forward or merge commit as the user prefers), push, tag `v0.2.0` on that commit, push the tag; watch the release workflow until the release is published with the 7 assets.
- [ ] **Step 5: [user] live cutover:** `cp ~/.local/bin/agy-pool ~/.local/bin/agy-pool.go-backup`; `scripts/rust-candidate.sh route go` and `stop`; then `agy-pool update` with the installed Go binary. Verify: `agy-pool --version` prints `agy-pool 0.2.0`; `systemctl --user is-active agy-pool` is `active` and `/proc/<MainPID>/exe` is the new binary; `agy-pool status` and `agy-pool quota` look right; one agy-worker run succeeds.
- [ ] **Rollback (if any check fails or the user asks):** `cp ~/.local/bin/agy-pool.go-backup ~/.local/bin/agy-pool && systemctl --user restart agy-pool`; verify `agy-pool --version` shows the Go version and an agy run succeeds.
- [ ] **Step 6:** keep `agy-pool.go-backup` and the candidate dir until Phase 6 starts; record the cutover date in the master plan.

---

## After Phase 5

Once the Rust gateway has run live for a few days with no rollback, write the Phase 6 (repo split) plan from the master plan's outline.
