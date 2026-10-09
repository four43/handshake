#!/usr/bin/env bash
# End-to-end test: the real server in Docker on 127.0.0.1:18080 (TURN on :3478), two Chromium pages, a real WebRTC connection.
# Usage (from client/): npm run e2e   (first time: npm install && npx playwright install chromium)
# IMAGE=<tag> npm run e2e tests an image that is already built (CI tests the image it pushes) instead of building one.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
install -m 644 e2e/e2e.toml "$tmp/config.toml" # the container runs as nonroot
image=${IMAGE:-handshake:e2e}
[ -n "${IMAGE:-}" ] || docker build -q -t "$image" .. >/dev/null
cid=$(docker run -d --rm -p 127.0.0.1:18080:8080 -p 127.0.0.1:3478:3478/udp -p 127.0.0.1:3478:3478/tcp \
  -e SESSION_SECRET=e2e-session-secret-0123456789abcdef \
  -v "$tmp/config.toml:/etc/handshake/config.toml:ro" "$image")
trap 'docker stop "$cid" >/dev/null; rm -rf "$tmp"' EXIT
for _ in $(seq 50); do curl -fs http://127.0.0.1:18080/healthz >/dev/null && break; sleep 0.2; done
npx playwright test "$@"
