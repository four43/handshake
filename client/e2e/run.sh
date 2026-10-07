#!/usr/bin/env bash
# End-to-end test: the real server in Docker on 127.0.0.1:18080, two Chromium pages, a real WebRTC connection.
# Usage (from client/): npm run e2e   (first time: npm install && npx playwright install chromium)
set -euo pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
install -m 644 e2e/e2e.toml "$tmp/config.toml" # the container runs as nonroot
docker build -q -t handshake:e2e .. >/dev/null
cid=$(docker run -d --rm -p 127.0.0.1:18080:8080 \
  -e SESSION_SECRET=e2e-session-secret-0123456789abcdef \
  -v "$tmp/config.toml:/etc/handshake/config.toml:ro" handshake:e2e)
trap 'docker stop "$cid" >/dev/null; rm -rf "$tmp"' EXIT
for _ in $(seq 50); do curl -fs http://127.0.0.1:18080/healthz >/dev/null && break; sleep 0.2; done
npx playwright test "$@"
