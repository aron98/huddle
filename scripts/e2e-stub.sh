#!/usr/bin/env bash
# Phase-1 e2e stub: boot the real server against a real trunk-built web
# bundle and verify that headless Chrome renders it. This is the seed of the
# multi-tab call harness that becomes a permanent CI gate in phase 2.
#
# Requires on PATH: cargo, trunk, curl, jq, and a Chrome/Chromium binary
# (override with CHROME_BIN; defaults to google-chrome, as on GitHub runners).
set -euo pipefail

port="${HUDDLE_E2E_PORT:-3123}"
base="http://127.0.0.1:${port}"
root="$(cd "$(dirname "$0")/.." && pwd)"
chrome="${CHROME_BIN:-google-chrome}"

# 1. Web bundle and server binary.
(cd "$root/crates/web" && trunk build)
(cd "$root" && cargo build -q -p huddle-server)

# 2. Boot the server against the freshly built bundle.
data_dir="$(mktemp -d)"
HUDDLE_BIND="127.0.0.1:${port}" \
HUDDLE_DATA_DIR="$data_dir" \
HUDDLE_WEB_DIR="$root/crates/web/dist" \
"$root/target/debug/huddle" &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    if curl -fsS "$base/api/health" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
curl -fsS "$base/api/health" >/dev/null # fail loudly if it never came up

# 3. Create a meeting that is open right now.
starts_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
ends_at="$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)"
code="$(curl -fsS -X POST "$base/api/meetings" \
    -H 'content-type: application/json' \
    -d "{\"title\":\"e2e\",\"starts_at\":\"$starts_at\",\"ends_at\":\"$ends_at\"}" \
    | jq -r '.meeting.code')"
if [ -z "$code" ] || [ "$code" = "null" ]; then
    echo "e2e stub: could not create a meeting" >&2
    exit 1
fi

# 4. Render checks. --virtual-time-budget lets the wasm app mount before the
#    DOM is dumped; the fake-media flags keep getUserMedia from blocking.
dump() {
    "$chrome" --headless=new --no-sandbox --disable-gpu \
        --use-fake-device-for-media-stream --use-fake-ui-for-media-stream \
        --virtual-time-budget=10000 --dump-dom "$1" 2>/dev/null
}

landing="$(dump "$base/")"
grep -q "Lightweight self-hosted video meetings" <<<"$landing" \
    || { echo "e2e stub: landing DOM missing expected marker" >&2; exit 1; }

prejoin="$(dump "$base/m/$code")"
grep -q "Join meeting" <<<"$prejoin" \
    || { echo "e2e stub: prejoin DOM missing expected marker" >&2; exit 1; }
grep -q 'id="preview"' <<<"$prejoin" \
    || { echo "e2e stub: prejoin DOM missing preview element" >&2; exit 1; }

echo "e2e stub: landing + prejoin render checks passed (meeting $code)"
