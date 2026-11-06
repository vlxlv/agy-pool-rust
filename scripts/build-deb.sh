#!/usr/bin/env bash
set -euo pipefail

ARCH="${1:-amd64}"
VERSION="${2:-0.1.0}"
BIN_PATH="${3:-dist/agy-pool-linux-${ARCH}}"
OUT_DIR="${4:-dist}"

# Strip leading 'v' from version if present
VERSION="${VERSION#v}"
# dpkg sorts "0.1.0-beta.37" (revision "beta.37") after "0.1.0"; "~" sorts before it, so the release is an upgrade.
DEB_VERSION="${VERSION/-/\~}"

if [ ! -f "$BIN_PATH" ]; then
  echo "Error: binary not found at $BIN_PATH" >&2
  exit 1
fi

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

PKG_DIR="$TMP_DIR/pkg"
mkdir -p "$PKG_DIR/DEBIAN"
mkdir -p "$PKG_DIR/usr/bin"
mkdir -p "$PKG_DIR/usr/lib/systemd/system"
mkdir -p "$PKG_DIR/usr/lib/systemd/user"

# Install binary
install -m 0755 "$BIN_PATH" "$PKG_DIR/usr/bin/agy-pool"

# Install the contrib/systemd units, pointed at the packaged binary
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
for SCOPE in system user; do
  sed 's|^ExecStart=.*|ExecStart=/usr/bin/agy-pool|' "$ROOT/contrib/systemd/$SCOPE/agy-pool.service" \
    > "$PKG_DIR/usr/lib/systemd/$SCOPE/agy-pool.service"
  chmod 0644 "$PKG_DIR/usr/lib/systemd/$SCOPE/agy-pool.service"
done

# Generate control file
cat << CONTROL > "$PKG_DIR/DEBIAN/control"
Package: agy-pool
Version: ${DEB_VERSION}
Section: net
Priority: optional
Architecture: ${ARCH}
Maintainer: vlxlv <https://github.com/vlxlv/agy-pool-go>
Homepage: https://github.com/vlxlv/agy-pool-go
Description: Antigravity Multi-Account Quota Pool & Reverse Proxy Gateway
 High-performance reverse proxy gateway and multi-account quota pool
 for Antigravity CLI (agy). Includes background daemon supervision,
 intelligent model-level cooldowns, and automatic failover.
CONTROL

# Generate postinst script
cat << 'POSTINST' > "$PKG_DIR/DEBIAN/postinst"
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # The system unit runs as this account; systemd creates its home as the unit's StateDirectory.
    getent group agy-pool >/dev/null || groupadd --system agy-pool
    getent passwd agy-pool >/dev/null || useradd --system --gid agy-pool --home-dir /var/lib/agy-pool \
        --no-create-home --shell /usr/sbin/nologin agy-pool
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload >/dev/null 2>&1 || true
        # On upgrade, move a running system gateway onto the new binary.
        systemctl try-restart agy-pool.service >/dev/null 2>&1 || true
    fi
fi
POSTINST
chmod 0755 "$PKG_DIR/DEBIAN/postinst"

# Generate prerm script
cat << 'PRERM' > "$PKG_DIR/DEBIAN/prerm"
#!/bin/sh
set -e
if [ "$1" = "remove" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl stop agy-pool.service >/dev/null 2>&1 || true
        systemctl disable agy-pool.service >/dev/null 2>&1 || true
        # Per-user gateways from the packaged user unit would otherwise keep running the removed binary.
        for U in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
            systemctl --user -M "$U@" stop agy-pool.service >/dev/null 2>&1 || true
        done
    fi
fi
PRERM
chmod 0755 "$PKG_DIR/DEBIAN/prerm"

mkdir -p "$OUT_DIR"
DEB_NAME="agy-pool_${VERSION}_${ARCH}.deb"
dpkg-deb --build --root-owner-group "$PKG_DIR" "$OUT_DIR/$DEB_NAME"
echo "Built Debian package: $OUT_DIR/$DEB_NAME"
