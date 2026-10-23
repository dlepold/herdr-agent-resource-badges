#!/bin/sh
# herdr build step for agent-resource-badges (runs during `herdr plugin install`).
# Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22 (macOS targets).
#
# Puts the plugin binary at target/release/agent-resource-badges, the path every
# command in herdr-plugin.toml expects:
#   1. Download the prebuilt binary for this OS and CPU (static musl on Linux,
#      native on macOS) from the GitHub
#      release that matches the manifest version, and check it against the
#      release's SHA256SUMS and its own --version output.
#   2. If that fails (offline, other CPU, release not published yet), build from
#      source with cargo, exactly as before.
#   3. If cargo is missing too, stop with a message that says what to do.
#
# AGENT_RESOURCE_BADGES_FROM_SOURCE=1 skips the download and always builds.
# Needs: sh, uname, curl or wget, sha256sum or shasum. No Rust toolchain.

set -eu

REPO="dlepold/herdr-agent-resource-badges"
BIN="agent-resource-badges"
OUT="target/release/$BIN"

log() { printf '%s build: %s\n' "$BIN" "$*" >&2; }

cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' herdr-plugin.toml | head -n 1)

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-amd64) TARGET="x86_64-unknown-linux-musl" ;;
    Linux-aarch64 | Linux-arm64) TARGET="aarch64-unknown-linux-musl" ;;
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
    *) TARGET="" ;;
esac

fetch() { # fetch <url> <file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 --retry 2 --connect-timeout 10 --max-time 120 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --https-only --timeout=30 --tries=3 -O "$2" "$1"
    else
        log "neither curl nor wget is installed"
        return 1
    fi
}

sha256_of() { # sha256_of <file>
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        return 1
    fi
}

# Called as an `if` condition, where `set -e` does not apply; every step that
# can fail therefore returns explicitly.
try_prebuilt() {
    if [ "${AGENT_RESOURCE_BADGES_FROM_SOURCE:-0}" = "1" ]; then
        log "AGENT_RESOURCE_BADGES_FROM_SOURCE=1, skipping the prebuilt download"
        return 1
    fi
    if [ -z "$TARGET" ]; then
        log "no prebuilt binary for $(uname -s) $(uname -m)"
        return 1
    fi
    if [ -z "$VERSION" ]; then
        log "could not read the version from herdr-plugin.toml"
        return 1
    fi

    asset="$BIN-$TARGET"
    base="https://github.com/$REPO/releases/download/v$VERSION"
    mkdir -p target/release || return 1
    tmp=$(mktemp -d target/.download.XXXXXX) || return 1
    trap 'rm -rf "$tmp"' EXIT

    log "downloading $asset from release v$VERSION"
    fetch "$base/$asset" "$tmp/$asset" || { log "download failed: $base/$asset"; return 1; }
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || { log "download failed: $base/SHA256SUMS"; return 1; }

    expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")
    actual=$(sha256_of "$tmp/$asset") || { log "no sha256sum or shasum to verify the download"; return 1; }
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        log "checksum mismatch for $asset (expected '${expected:-none}', got '$actual')"
        return 1
    fi

    chmod 755 "$tmp/$asset" || return 1
    got=$("$tmp/$asset" --version 2>/dev/null) || { log "the downloaded binary does not run here"; return 1; }
    if [ "$got" != "$BIN $VERSION" ]; then
        log "the downloaded binary reports '$got', expected '$BIN $VERSION'"
        return 1
    fi

    mv -f "$tmp/$asset" "$OUT" || return 1
    log "installed the prebuilt binary (SHA-256 $actual)"
    return 0
}

if try_prebuilt; then
    exit 0
fi

if command -v cargo >/dev/null 2>&1; then
    log "building from source with cargo instead (takes 10 to 60 seconds)"
    exec cargo build --release --locked
fi

log "could not install a prebuilt binary, and cargo is not installed."
log "Check the network and run the install again, or install Rust from https://rustup.rs"
exit 1
