#!/bin/sh
# End-to-end test of agent-resource-badges against a real, isolated herdr server.
# Created 2026-10-22 (David Lepold + Claude Opus 5.5). Last update: 2026-10-22.
#
# Runs in CI on Linux and macOS, and locally without touching your own herdr:
# HOME points at a temporary directory, so the server, its socket, config,
# plugin registry and plugin state all live there and vanish afterwards.
#
#   1. download herdr (or use HERDR_BIN), start `herdr server` in the background
#   2. create a workspace with a busy pane (`while :; do :; done`) and a quiet one
#   3. link this checkout as a plugin, invoke its `enable` action
#   4. expect a badge token on the busy pane, none on the quiet pane
#   5. invoke `disable`, expect the badge to be cleared
#
# Needs: a built target/release/agent-resource-badges, sh, curl, python3.
# Env:   HERDR_VERSION (default 0.9.5), HERDR_BIN (use this binary instead),
#        KEEP=1 keeps the temporary directory for inspection.

set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)
PLUGIN_BIN="$ROOT/target/release/agent-resource-badges"
HERDR_VERSION="${HERDR_VERSION:-0.9.5}"

step() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*"; [ -f "$WORK/server.log" ] && tail -n 40 "$WORK/server.log"; exit 1; }
json() { # json <dotted.path> < JSON  ->  the value at that path
    python3 -c "
import json, sys
v = json.load(sys.stdin)
for key in sys.argv[1].split('.'):
    v = v[key]
print(v)
" "$1"
}

[ -x "$PLUGIN_BIN" ] || { echo "build first: cargo build --release"; exit 1; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/arb-e2e.XXXXXX")
REAL_HOME=$HOME
export HOME="$WORK/home"
mkdir -p "$HOME" "$WORK/bin"
unset XDG_CONFIG_HOME XDG_STATE_HOME XDG_DATA_HOME XDG_RUNTIME_DIR \
      HERDR_SOCKET_PATH HERDR_CONFIG_PATH HERDR_ENV HERDR_PANE_ID HERDR_WORKSPACE_ID 2>/dev/null || true

step "herdr binary"
if [ -n "${HERDR_BIN:-}" ]; then
    ln -s "$HERDR_BIN" "$WORK/bin/herdr"
else
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) asset=herdr-linux-x86_64 ;;
        Linux-aarch64 | Linux-arm64) asset=herdr-linux-aarch64 ;;
        Darwin-arm64) asset=herdr-macos-aarch64 ;;
        Darwin-x86_64) asset=herdr-macos-x86_64 ;;
        *) echo "no herdr build for $(uname -s) $(uname -m)"; exit 1 ;;
    esac
    curl -fsSL --retry 2 --connect-timeout 10 --max-time 120 -o "$WORK/bin/herdr" \
        "https://github.com/herdrdev/herdr/releases/download/v$HERDR_VERSION/$asset"
    chmod 755 "$WORK/bin/herdr"
fi
export PATH="$WORK/bin:$PATH"
herdr --version

SERVER_PID=""
cleanup() {
    herdr server stop >/dev/null 2>&1 || true
    sleep 1
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    if [ "${KEEP:-0}" = "1" ]; then echo "kept $WORK"; else rm -rf "$WORK"; fi
    export HOME="$REAL_HOME"
}
trap cleanup EXIT

step "start an isolated herdr server (HOME=$HOME)"
herdr server >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
i=0
until herdr workspace list >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -gt 100 ] && fail "server did not answer within 20 s"
    sleep 0.2
done
echo "server up (pid $SERVER_PID)"

step "workspace with a busy and a quiet pane"
ws=$(herdr workspace create --cwd "$WORK" --label e2e --no-focus)
BUSY=$(printf '%s' "$ws" | json result.root_pane.pane_id)
QUIET=$(herdr pane split "$BUSY" --direction right --no-focus | json result.pane.pane_id)
herdr pane run "$BUSY" 'while :; do :; done' >/dev/null
echo "busy=$BUSY quiet=$QUIET"

step "link the plugin and enable it"
herdr plugin link "$ROOT" >/dev/null
herdr plugin action invoke agent-resource-badges.enable >/dev/null
# First report comes after the priming scan plus one 5 s interval.
sleep 13
herdr plugin action invoke agent-resource-badges.status >/dev/null
sleep 1

tokens_of() { # tokens_of <pane_id> -> res_* tokens as "name=value" lines
    herdr api snapshot | python3 -c "
import json, sys
d = json.load(sys.stdin)['result']['snapshot']
for p in d.get('panes', []):
    if p.get('pane_id') == sys.argv[1]:
        for k, v in sorted((p.get('tokens') or {}).items()):
            if k.startswith('res_'):
                print(f'{k}={v}')
" "$1"
}

busy_tokens=$(tokens_of "$BUSY")
quiet_tokens=$(tokens_of "$QUIET")
echo "busy pane:  ${busy_tokens:-<none>}"
echo "quiet pane: ${quiet_tokens:-<none>}"
[ -n "$busy_tokens" ] || fail "no badge on the busy pane"
[ -z "$quiet_tokens" ] || fail "unexpected badge on the quiet pane"
case "$busy_tokens" in
    res_hot=* | res_warn=*) echo "PASS busy pane shows a warn/hot badge" ;;
    *) fail "busy pane badge is only '$busy_tokens' (expected warn or hot for ~100% CPU)" ;;
esac
echo "PASS quiet pane shows no badge"

step "disable clears the badge"
herdr plugin action invoke agent-resource-badges.disable >/dev/null
sleep 8
after=$(tokens_of "$BUSY")
[ -z "$after" ] || fail "badge still present after disable: $after"
echo "PASS badge cleared after disable"

herdr pane send-keys "$BUSY" ctrl+c >/dev/null 2>&1 || true
printf '\nE2E passed on %s %s (%s)\n' "$(uname -s)" "$(uname -m)" "$(herdr --version)"
