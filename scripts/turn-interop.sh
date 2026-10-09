#!/usr/bin/env bash
# Interop: coturn's own test client (turnutils_uclient) against Handshake's built-in TURN server, over UDP and TCP,
# relaying to coturn's test peer and between two of its clients (relay to relay). Needs Docker.
# Usage: scripts/turn-interop.sh            (builds the image)
#        IMAGE=<tag> scripts/turn-interop.sh  (tests an image that is already built; CI passes handshake:ci)
set -euo pipefail
cd "$(dirname "$0")/.."
image=${IMAGE:-handshake:interop}
[ -n "${IMAGE:-}" ] || docker build -q -t "$image" . >/dev/null
secret=interop-turn-secret
net=handshake-interop-$$
tmp=$(mktemp -d)
cat > "$tmp/config.toml" <<'TOML'
[turn]
urls = ["turn:hs:3478?transport=udp"]
relay_ports = "49160-49199"
external_ip = "hs"                   # the container's address on the test network, by name
allowed_peers = ["172.16.0.0/12", "192.168.0.0/16", "10.0.0.0/8"]   # Docker networks: the test peer is private

[apps.interop]
origins = []
turn = true
TOML
chmod 644 "$tmp/config.toml"
cleanup() { docker rm -f hs peer >/dev/null 2>&1 || true; docker network rm "$net" >/dev/null 2>&1 || true; rm -rf "$tmp"; }
trap cleanup EXIT
docker network create "$net" >/dev/null
docker run -d --name hs --network "$net" -e SESSION_SECRET=interop-session-secret-0123456789abcdef -e TURN_SECRET=$secret -e RUST_LOG=handshake=debug \
  -v "$tmp/config.toml:/etc/handshake/config.toml:ro" "$image" >/dev/null
docker run -d --name peer --network "$net" --entrypoint turnutils_peer coturn/coturn -L 0.0.0.0 -p 3480 >/dev/null
sleep 1
peer_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' peer)

uclient() { # args: description, then turnutils_uclient flags
  local what=$1; shift
  local out
  out=$(docker run --rm --network "$net" --entrypoint turnutils_uclient coturn/coturn \
    -W "$secret" -u interop -X -c -n 20 -l 120 "$@" hs 2>&1) || true
  local lost; lost=$(sed -n 's/.*Total lost packets \([0-9]*\).*/\1/p' <<<"$out" | tail -1)
  if [ "${lost:-x}" = "0" ]; then
    echo "ok   $what"
  else
    echo "FAIL $what"; echo "$out" | tail -25; docker logs hs 2>&1 | tail -20; exit 1
  fi
}
uclient "UDP, Send indications to a peer"      -s -e "$peer_ip"
uclient "UDP, channels to a peer"                 -e "$peer_ip"
uclient "TCP, channels to a peer"              -t -e "$peer_ip"
uclient "UDP, client to client (relay to relay)" -y
uclient "TCP, client to client (relay to relay)" -t -y
