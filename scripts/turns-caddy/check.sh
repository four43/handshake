#!/usr/bin/env bash
# Check the self-hosting guide's turns: setup: Caddy built with caddy-l4 terminates TLS on :443 for the TURN hostname
# and hands plain TURN to Handshake with a PROXY v2 header, while another site on :443 keeps working. Needs Docker.
# Usage: scripts/turns-caddy/check.sh            (builds the Handshake image)
#        IMAGE=<tag> scripts/turns-caddy/check.sh
set -euo pipefail
cd "$(dirname "$0")"
export IMAGE=${IMAGE:-handshake:turns-caddy}
[ "$IMAGE" != handshake:turns-caddy ] || docker build -q -t "$IMAGE" ../.. >/dev/null
CONFIG_DIR=$(mktemp -d); export CONFIG_DIR
install -m 644 handshake.toml "$CONFIG_DIR/config.toml" # the container runs as nonroot
project=turns-caddy-$$
dc() { docker compose -p "$project" "$@"; }
trap 'dc --profile check down -v >/dev/null 2>&1; rm -rf "$CONFIG_DIR"' EXIT
dc up -d --build --quiet-pull >/dev/null 2>&1
sleep 3
dc --profile check run --rm check
site=$(dc --profile check run --rm --entrypoint python check -c \
  "import ssl,socket;c=ssl._create_unverified_context();s=c.wrap_socket(socket.create_connection(('caddy',443)),server_hostname='site.test');s.sendall(b'GET / HTTP/1.1\r\nHost: site.test\r\nConnection: close\r\n\r\n');print(s.recv(4096).decode().split('\r\n\r\n')[-1])")
[ "$site" = "site ok" ] || { echo "FAIL: site.test answered ${site@Q}"; exit 1; }
echo "ok   other site on :443 still served"
