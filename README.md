# agy-pool-rust: Antigravity Multi-Account Quota Pool & Reverse Proxy Gateway

[![CI](https://github.com/vlxlv/agy-pool-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/vlxlv/agy-pool-rust/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Darwin%20%7C%20Termux-green.svg)](#)

A local reverse proxy gateway that pools Google Antigravity accounts for **Antigravity CLI (`agy`)** on Linux, Termux, and macOS.

This is the Rust rewrite of [agy-pool-go](https://github.com/vlxlv/agy-pool-go) and a drop-in replacement for it: same `state.db`, `config.json`, environment variables, CLI and port (`127.0.0.1:8899`). Swapping the binary is enough; the Go binary can be put back the same way. Behaviour differences found while porting are listed in [`PORTING_NOTES.md`](PORTING_NOTES.md).

Releases are still published from agy-pool-go, so `agy-pool update` and the `.deb` links below point there until this repository publishes its first release.

---

## Building

Requires Rust 1.99+.

```bash
git clone https://github.com/vlxlv/agy-pool-rust.git
cd agy-pool-rust
cargo build --release --locked          # target/release/agy-pool
```

Release binaries for Linux are static musl builds (they run on Termux and old glibc); see [`.github/workflows/release-dry-run.yml`](.github/workflows/release-dry-run.yml) for the exact build of all four platform assets and the `.deb` packages.

### Installation & Native `agy` Integration

#### Via Debian/Ubuntu Package (.deb)
Download the `.deb` package from [Releases](https://github.com/vlxlv/agy-pool-go/releases) and install:
```bash
sudo dpkg -i agy-pool_<version>_amd64.deb
```
The package automatically installs `/usr/bin/agy-pool` and systemd service files. Run the gateway per user with `systemctl --user enable --now agy-pool`, or system-wide with `sudo systemctl enable --now agy-pool` (runs as the `agy-pool` system user created by the package, state in `/var/lib/agy-pool`, optional config in `/etc/agy-pool/config.json`). Without the package, a user install can use the same unit: copy `contrib/systemd/user/agy-pool.service` to `~/.config/systemd/user/` and run `systemctl --user enable --now agy-pool` (it runs `~/.local/bin/agy-pool`). `agy-pool stop` and `agy-pool restart` act through `systemctl` when the gateway runs under one of these units. Upgrade with a newer `.deb`; `agy-pool update` refuses to replace a package-owned binary.

If upgrading from an earlier beta package, install the official `v0.1.0` release package with `sudo apt install --allow-downgrades ./agy-pool_<version>_<arch>.deb` or `sudo dpkg -i agy-pool_<version>_<arch>.deb`.

#### Via agy-pool installer
To install `agy-pool` and set up transparent integration with native Antigravity CLI:

```bash
# User-mode installation (recommended)
./target/release/agy-pool install

# Or system-mode installation
sudo ./target/release/agy-pool install --system
```

#### Intended User Experience

Once installed, **normal operation requires zero special flags, aliases, or manual environment exports**:

```bash
# Normal use: automatically routes through agy-pool quota load balancing
agy -p "explain this code"
agy -c
agy --mode accept-edits --dangerously-skip-permissions
```

To bypass the pool and execute native Antigravity directly against upstream Google:

```bash
# Explicit bypass: runs native Antigravity directly with its current native credentials
# No CLI Base synchronization or agy-pool routing is performed.
agy-raw -p "direct prompt"

# Or using the agy-orig alias
agy-orig -p "direct prompt"
```

#### Managed Installation Layout

The installer establishes a deterministic, robust layout:

| Path | Description |
|------|-------------|
| `~/.local/bin/agy-pool` | `agy-pool` binary |
| `~/.local/bin/agy` | Thin POSIX managed shim (default entrypoint invoking `agy-pool run`) |
| `~/.local/bin/agy-raw` | Managed shim executing direct native bypass via `agy-pool raw` |
| `~/.local/bin/agy-orig` | Symlink alias to `agy-raw` |
| `~/.local/libexec/agy-pool/agy-native` | Preserved real native Antigravity executable |
| `~/.config/agy-pool/config.json` | Static configuration (see tracked [`config.example.json`](config.example.json)) |
| `~/.local/share/agy-pool/state.db` | Authoritative dynamic account pool state database |

The installer safely validates and preserves the native `agy` binary into `~/.local/libexec/agy-pool/agy-native` before publishing or updating the entrypoint shim. It never overwrites the only copy of native `agy`.

#### Updating `agy-pool`

```bash
agy-pool update --check   # Report whether a newer release exists
agy-pool update           # Download, verify, replace, and restart a running gateway
```

`update` (alias `upgrade`) fetches the newest GitHub release (prereleases included), verifies the platform binary against the release `SHA256SUMS`, atomically replaces the installed `agy-pool` binary, and restarts the gateway if it was running. A failed download or checksum mismatch leaves the current binary untouched. The repository is private, so a token is required: `GH_TOKEN`, `GITHUB_TOKEN`, or an existing `gh auth login` session. System-mode installs (`/usr/local/bin`) need `sudo` with `GH_TOKEN` set.

#### Official `agy` Upgrades & Repair Workflow

If an official Google Antigravity update overwrites `~/.local/bin/agy` with a new native ELF binary:

1. Running `agy-pool doctor` will detect that `~/.local/bin/agy` is a native binary bypassing the pool.
2. Re-running `agy-pool install` repairs the integration:
   - Detects the new native binary at `~/.local/bin/agy`.
   - Atomically updates `~/.local/libexec/agy-pool/agy-native` with the upgraded binary.
   - Restores the managed shim at `~/.local/bin/agy`.

#### Uninstallation

To remove `agy-pool` integration:

```bash
agy-pool uninstall
```

Uninstall safely removes `agy-pool`-owned shims (`agy`, `agy-raw`, `agy-orig`), the `agy-pool` binary, and the managed native copy in `libexec/agy-pool`, while leaving any external native binaries completely untouched.


---

---

## Testing

Run tests only through the sandbox script, never bare `cargo test` on a machine with a live gateway:

```bash
scripts/test.sh                          # cargo test
scripts/test.sh test --lib storage::     # any cargo arguments
scripts/test.sh clippy --all-targets -- -D warnings
```

`scripts/test.sh` runs cargo in a user, mount and PID namespace where the live state (`~/.local/share/agy-pool`, `~/.config/agy-pool`, `~/.gemini`, `~/.local/bin`, `~/.local/libexec/agy-pool`, shell rc files) is read-only, the live gateway is invisible, and `systemctl` is stubbed out. It refuses to run if any of that does not hold.

CI (`ci.yml`) runs `cargo fmt --check`, `cargo clippy -D warnings` and the suite on every push and pull request to `main`, plus a stress job that repeats the suite at 4, 16 and 64 threads. `release-dry-run.yml` builds and verifies every release asset without publishing.

---

## License

MIT
