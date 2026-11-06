# agy-pool-rust

[![CI](https://github.com/vlxlv/agy-pool-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/vlxlv/agy-pool-rust/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Termux-green.svg)](#)

A local gateway that pools several Google accounts behind the **Antigravity CLI (`agy`)**. `agy` talks to `127.0.0.1:8899`; agy-pool forwards each request upstream on the account with the most usable quota and moves on to the next account when one runs out.

This is the Rust rewrite of [agy-pool-go](https://github.com/vlxlv/agy-pool-go) and a drop-in replacement for it: same `state.db`, `config.json`, environment variables, command line and port. Replacing the binary is the whole migration, and putting the Go binary back is the whole rollback (see [Migrating from agy-pool-go](#migrating-from-agy-pool-go)). Known behaviour differences are listed in [`PORTING_NOTES.md`](PORTING_NOTES.md).

Release binaries and `.deb` packages are on the [releases page](https://github.com/vlxlv/agy-pool-rust/releases) from v0.2.0 on, and `agy-pool update` installs them. agy-pool-go installs keep updating from agy-pool-go until you switch them over (see [Migrating from agy-pool-go](#migrating-from-agy-pool-go)).

## Features

- **Quota-aware routing.** The `smart` strategy (default) prefers accounts whose quota renews soonest and keeps a low-quota reserve for last; `max_quota` always picks the account with the most remaining quota.
- **Automatic failover.** On HTTP 429 or exhausted quota the request moves to the next account, honouring upstream `Retry-After` and reset times. Model capacity errors (503) pause only that model, not the account.
- **No duplicate generations.** A generation request is retried on another account only if it failed before any byte reached upstream (DNS, connect, TLS). Once a request may have reached Google, it is never replayed.
- **Prompt-cache affinity.** A conversation stays on the same account while that account is healthy, so upstream prompt caching keeps working.
- **Streaming.** Server-sent events are passed through as they arrive.
- **Dashboards.** `quota` shows per-account quota bars and reset times, `top` is a live terminal dashboard, `stats` and `forecast` report routing effectiveness and quota runway.
- **Static release binaries** for Linux (amd64, arm64; musl, so they also run on Termux and old glibc) and macOS (amd64, arm64), plus `.deb` packages with systemd units.
- **Encrypted backups.** `export` / `import` move the account pool between machines, optionally encrypted (PBKDF2-HMAC-SHA256).

## Install

### Release binary

Download the binary for your platform from the [releases page](https://github.com/vlxlv/agy-pool-rust/releases) (`agy-pool-linux-amd64`, `agy-pool-linux-arm64`, `agy-pool-darwin-amd64`, `agy-pool-darwin-arm64`), check it against `SHA256SUMS`, then run `install` as below:

```bash
sha256sum --ignore-missing -c SHA256SUMS
chmod +x agy-pool-linux-amd64
./agy-pool-linux-amd64 install
```

### From source

Requires Rust 1.99 or newer.

```bash
git clone https://github.com/vlxlv/agy-pool-rust.git
cd agy-pool-rust
cargo build --release --locked
./target/release/agy-pool install          # user mode: ~/.local/bin, recommended
sudo ./target/release/agy-pool install --system
```

`install` puts `agy-pool` on your `PATH`, moves the native `agy` binary to `~/.local/libexec/agy-pool/agy-native`, and replaces `~/.local/bin/agy` with a small shim that runs `agy-pool run`. It never overwrites the only copy of native `agy`.

### Debian / Ubuntu package

Download `agy-pool_<version>_<arch>.deb` from the [releases page](https://github.com/vlxlv/agy-pool-rust/releases) and install it:

```bash
sudo dpkg -i agy-pool_<version>_amd64.deb
systemctl --user enable --now agy-pool     # per user
# or system-wide, as the agy-pool system user (state in /var/lib/agy-pool, config in /etc/agy-pool/config.json):
sudo systemctl enable --now agy-pool
```

Upgrade by installing a newer `.deb`; `agy-pool update` refuses to replace a package-owned binary.

### systemd without the package

```bash
cp contrib/systemd/user/agy-pool.service ~/.config/systemd/user/
systemctl --user enable --now agy-pool     # runs ~/.local/bin/agy-pool
```

When the gateway runs under one of these units, `agy-pool stop` and `agy-pool restart` go through `systemctl`.

## Quick start

```bash
agy-pool login             # add a Google account (repeat for each account)
agy-pool list              # accounts in the pool
agy-pool start             # start the gateway (agy-pool run also starts it on demand)
agy -p "explain this code" # agy now goes through the pool
agy-pool quota             # remaining quota per account
```

After `install`, plain `agy` goes through the pool with no flags or environment changes. To bypass the pool and run native `agy` with its own credentials, use `agy-raw` (or its alias `agy-orig`).

## Commands

| Command | What it does |
|---|---|
| `login`, `import-current` | Add an account by browser login, or import the account native `agy` is signed in with |
| `list`, `quota`, `top` | Show the pool, quota bars, or a live dashboard (`-e` shows emails) |
| `status`, `stats`, `forecast` | Gateway process summary, routing effectiveness, quota runway |
| `start`, `stop`, `restart`, `logs` | Control the gateway and read its log |
| `run [-- agy args]` | Start the gateway if needed and run native `agy` through it |
| `strategy`, `switch` | Choose the routing strategy, or the account native `agy` uses directly |
| `rename`, `enable`, `disable`, `remove`, `verify` | Manage single accounts |
| `move`, `swap`, `sort`, `reorder` | Change account order |
| `export`, `import` | Back up and restore the pool |
| `doctor` | Check binaries, gateway, database and connectivity |
| `install`, `uninstall`, `update` | Manage the installation |

`agy-pool --help` lists every command with examples.

## Configuration

| Path | Contents |
|---|---|
| `~/.config/agy-pool/config.json` | Static settings: listen address and port, strategy, native `agy` path, log size. See [`config.example.json`](config.example.json). |
| `~/.local/share/agy-pool/state.db` | Account pool and runtime state (SQLite). Managed by agy-pool; don't edit it. |
| `~/.local/share/agy-pool/agy-pool.log` | Gateway log. It and `quota-history.jsonl` rotate at `logging.max_size_bytes` (5 MB by default). |

Environment variables:

| Variable | Effect |
|---|---|
| `AGY_CONFIG_FILE`, `AGY_DATA_DIR` | Use another config file or data directory (same as `-c` / `-D`) |
| `AGY_BIN` | Path of the native `agy` binary to run |
| `AGY_SHOW_EMAIL=1`, `AGY_PRIVACY=0` | Show account emails instead of labels |
| `GH_TOKEN`, `GITHUB_TOKEN` | Optional token for `agy-pool update` (raises GitHub API rate limits) |

## Updating

```bash
agy-pool update --check    # is there a newer release?
agy-pool update            # download, verify against SHA256SUMS, replace, restart the gateway
```

A failed download or checksum mismatch leaves the installed binary untouched. If an official Antigravity update overwrites `~/.local/bin/agy`, `agy-pool doctor` reports it and `agy-pool install` repairs the integration, keeping the new native binary. `agy-pool uninstall` removes the shims, the binary and the managed native copy, and leaves any other `agy` binary alone.

## Migrating from agy-pool-go

State, config and the systemd unit are shared, so only the binary changes. Use a release binary or `target/release/agy-pool` from a source build:

```bash
cp ~/.local/bin/agy-pool ~/.local/bin/agy-pool.go-backup
install -m 755 agy-pool-linux-amd64 ~/.local/bin/agy-pool.new
mv -f ~/.local/bin/agy-pool.new ~/.local/bin/agy-pool
systemctl --user restart agy-pool          # or: agy-pool restart
```

From then on `agy-pool update` follows this repository's releases. Roll back the same way:

```bash
cp ~/.local/bin/agy-pool.go-backup ~/.local/bin/agy-pool
systemctl --user restart agy-pool
```

## Development

Run tests only through the sandbox script, never with bare `cargo test` on a machine where agy-pool is in use:

```bash
scripts/test.sh                                      # cargo test
scripts/test.sh test --lib storage::                 # any cargo arguments
scripts/test.sh clippy --all-targets -- -D warnings
```

`scripts/test.sh` runs cargo in a user, mount and PID namespace in which the real state (`~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.gemini`, `~/.local/bin`, `~/.local/libexec/agy-pool`, shell rc files) is read-only, the running gateway is invisible, and `systemctl` is stubbed out. It refuses to run if any of that does not hold.

CI ([`ci.yml`](.github/workflows/ci.yml)) runs `cargo fmt --check`, `cargo clippy -D warnings`, the test suite, a concurrency race stress job, and dependency security audit (`rustsec/audit-check`) on every push to `main` and pull request, plus weekly on schedule.

### Releasing

Bump `version` in `Cargo.toml` (and `Cargo.lock`) in the change commit, wait for CI on `main`, then tag that commit `v<version>` (e.g. `v0.2.2`) and push the tag. [`release.yml`](.github/workflows/release.yml) triggers only on `v*` tags, verifies that the tag matches `Cargo.toml`, gates on the test suite and security audit before starting builds, packages and verifies all assets, and publishes the GitHub release titled with the version number (without leading `v`).

## License

MIT
