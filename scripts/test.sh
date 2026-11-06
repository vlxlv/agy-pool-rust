#!/usr/bin/env bash
# Run the Rust test suite with the live agy-pool state mounted read-only.
#
# This host runs the live gateway; on 2026-11-06 a racy test overwrote its state.db.
# The code-level guard depends on a test-mode flag that tests can flip, so this wrapper
# adds an OS-level guard: a private mount namespace in which every live path is read-only.
# The bind mounts exist only inside this process tree; the running gateway is unaffected.
# Daemon tests also send signals and call systemctl, so the sandbox gets its own PID namespace
# (host processes, including the gateway, are invisible) and a systemctl stub that always fails.
#
# Usage: scripts/test.sh [cargo args...]   (default: cargo test)
#   e.g. scripts/test.sh test --lib storage:: -- --test-threads=16
#        scripts/test.sh clippy --all-targets -- -D warnings
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
home="$(getent passwd "$(id -u)" | cut -d: -f6)" # same source as the code's real host home, not $HOME
uid="$(id -u)"
gid="$(id -g)"

protected=()
# libexec holds agy-native (the real agy the gateway launches); the rc files are what uninstall may edit.
for p in "$home/.local/share/agy-pool" "$home/.config/agy-pool" "$home/.gemini" "$home/.local/bin" \
	"$home/.local/libexec/agy-pool" "$home/.bashrc" "$home/.zshrc"; do
	[[ -e "$p" ]] && protected+=("$p")
done

if [[ "${AGY_RUST_TEST_SANDBOXED:-}" != "1" ]]; then
	# Stage 1 (as mapped root): bind every protected path read-only, then drop back to the real uid/gid.
	mounts=""
	for p in "${protected[@]}"; do
		mounts+="mount --bind '$p' '$p' && mount -o remount,bind,ro '$p' '$p' && "
	done
	# sh stays PID 1 (no exec) so it reaps orphaned daemon children; cargo as PID 1 would leave zombies
	# that kill -0 reports as alive. --kill-child tears the whole namespace down, leaked daemons included.
	exec unshare --user --map-root-user --mount --pid --fork --kill-child --mount-proc sh -c \
		"${mounts}unshare --user --map-user=$uid --map-group=$gid env AGY_RUST_TEST_SANDBOXED=1 '$0' \"\$@\"; exit \$?" \
		sh "$@"
fi

# Stage 2 (inside the sandbox): refuse to run unless every protected path really is read-only.
for p in "${protected[@]}"; do
	if [[ -d "$p" ]]; then
		probe="$p/.rust-test-write-probe"
		if (: >"$probe") 2>/dev/null; then
			rm -f "$probe"
			echo "test.sh: $p is writable inside the sandbox; refusing to run tests" >&2
			exit 1
		fi
	elif [[ -w "$p" ]]; then # a file: access(W_OK) reports EROFS on the read-only bind mount
		echo "test.sh: $p is writable inside the sandbox; refusing to run tests" >&2
		exit 1
	fi
done
echo "test.sh: read-only: ${protected[*]}" >&2

# ...and that the live gateway cannot be signalled from here.
live_pid="$(grep -m1 -o '"pid": *[0-9]*' "$home/.local/share/agy-pool/agy-pool.pid" 2>/dev/null | grep -o '[0-9]*$' || true)"
if [[ -n "$live_pid" ]] && kill -0 "$live_pid" 2>/dev/null; then
	echo "test.sh: live gateway pid $live_pid is visible inside the sandbox; refusing to run tests" >&2
	exit 1
fi

# systemctl reaches the real user manager over D-Bus, which the PID namespace does not isolate.
stub_dir="$repo/target/sandbox-bin"
mkdir -p "$stub_dir"
printf '#!/bin/sh\necho "systemctl is blocked inside test.sh: $*" >&2\nexit 1\n' >"$stub_dir/systemctl"
chmod +x "$stub_dir/systemctl"

export PATH="$stub_dir:$home/.cargo/bin:$home/sdk/go1.26.10/bin:$PATH" AGY_TEST_MODE=1
cd "$repo"
if [[ $# -eq 0 ]]; then
	set -- test
fi
exec cargo "$@"
