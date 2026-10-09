---
title: Self-hosting
description: Run Caddy and Handshake, with its built-in TURN relay, on one Docker host. Secrets, health checks, metrics and logs.
---

Handshake is built to run as a single instance on one small Docker host, next to a reverse proxy. It is the signaling server and the TURN relay in one: no coturn to run. Your games stay static (for example on GitHub Pages); only signaling, TURN credential requests and relayed traffic for players who cannot connect directly reach the server.

## Architecture

| Service | Job |
| --- | --- |
| **Caddy** | Terminates TLS for `https://signal.example.com` with automatic certificates and proxies `/session`, `/turn` and `/ws` to Handshake on port 8080. Nothing else is proxied, `/metrics` included. Optionally also terminates TLS for `turns:` on 443 (see [TURN over TLS on 443](#turn-over-tls-on-443-with-caddy)). |
| **handshake** | One Rust binary in a distroless image, running as a non-root user. Reads `config.toml` and secrets from environment variables. Serves signaling on 8080 and TURN on 3478 (UDP and TCP), relaying through a small UDP port range. |

Players' browsers talk to Caddy over HTTPS and WebSocket, then to each other directly, or through Handshake's TURN relay when they cannot. Handshake never looks inside game data; relayed traffic is DTLS-encrypted end to end.

## Compose file

```yaml
services:
  caddy:
    image: caddy:2            # or the caddy-l4 build below, for turns: on 443
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data

  handshake:
    image: ghcr.io/four43/handshake:v1   # newest 1.x; or pin e.g. :v1.0.0
    restart: unless-stopped
    environment:
      SESSION_SECRET: ${SESSION_SECRET}
      SESSION_SECRET_PREV: ${SESSION_SECRET_PREV:-}
    volumes:
      - ./config.toml:/etc/handshake/config.toml:ro
    ports:
      - "3478:3478/udp"                  # TURN
      - "3478:3478/tcp"
      - "49160-49200:49160-49200/udp"    # relay ports: same numbers inside and out
    # 8080 is not published: only Caddy reaches it, over the compose network

volumes:
  caddy_data:
```

Put the secrets in a `.env` file next to the compose file (mode 600). Image tags are the short commit hash for builds of `main`, or the git tag for releases; there is no `latest`.

:::caution
The container runs as `nonroot`. Make `config.toml` world-readable (`chmod 644 config.toml`), or the server cannot read it and exits.
:::

## Caddyfile

```text
signal.example.com {
	@handshake path /session /turn /ws
	handle @handshake {
		reverse_proxy handshake:8080
	}
	handle {
		respond 404
	}
}
```

`reverse_proxy` handles the WebSocket upgrade on its own and sets `X-Forwarded-For`, which Handshake believes because Caddy reaches it from a private Docker network address (see below). Handshake answers CORS preflights for `/session` and `/turn` itself, for the origins in your config.

## Server config

A production `config.toml`:

```toml
listen = "0.0.0.0:8080"
allow_localhost = false

[turn]
urls = [
  "stun:turn.example.com:3478",
  "turn:turn.example.com:3478?transport=udp",
  "turn:turn.example.com:3478?transport=tcp",
  "turns:turn.example.com:443?transport=tcp",   # only with the Caddy setup below
]
ttl_secs = 3600
relay_ports = "49160-49200"        # starts the built-in TURN server
external_ip = "203.0.113.10"       # the host's public IPv4, or a hostname that resolves to it

[apps.my-game]
origins = ["https://you.github.io"]
max_players = 8
max_rooms = 50
public_rooms = true
turn = true
```

Rate limits and "nearby" are per client IP, so the server must know each player's real address. It reads `X-Forwarded-For` only from peers in `trusted_proxies`, which defaults to loopback and the private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`). That covers Caddy on the same host or Docker network with no configuration. Any other peer's header is ignored, so a client exposed directly cannot spoof their address.

:::caution
If players reach Handshake from private addresses *without* a proxy (a LAN deployment, or Docker's userland proxy rewriting source addresses), they could set `X-Forwarded-For` themselves. Narrow `trusted_proxies` to your proxy's address, or set `trusted_proxies = []`.
:::

The [config reference](/reference/config/) documents every key, including `[limits]` and the rest of `[turn]`.

## TURN

Setting `relay_ports` starts Handshake's TURN server. It checks the credentials `/turn` hands out itself, so there is no `TURN_SECRET` to share: Handshake makes up a fresh one each time it starts. (Set `TURN_SECRET` only if something else must check the credentials too.) It also answers STUN, so the `stun:` URL can point at it instead of a public STUN server.

- **Ports.** Open, and forward from your router if the host is behind one: `3478` over UDP and TCP, and the `relay_ports` range over UDP. Publish the range in Docker with the same numbers on both sides; relay addresses are handed to peers as `external_ip:port`. About 40 ports map fine through Docker; host networking is not needed.
- **`external_ip`** is the public IPv4 address peers reach the relay ports on. On a cloud VM it is the VM's public address. On a home connection whose address changes, give the hostname your dynamic DNS keeps current, for example `external_ip = "home.example.com"`: Handshake resolves it at startup and again every minute.
- **Size the relay range** for the players you expect to relay at once. Each relayed connection takes one port per transport the browser tries (up to three: UDP, TCP, TLS), and a relay-only host takes that per guest. `max_allocations` (500), `allocations_per_ip` (64) and `kbps_per_allocation` (2000) cap use; `allocations_per_ip` counts the real client address, even behind Caddy.
- **The relay never sends into private networks**: loopback, private, carrier-grade NAT, link-local (cloud metadata), special-use (documentation, benchmarking), multicast and reserved addresses are refused, so a TURN user cannot reach your LAN or your cloud's metadata service. Two relayed players on the same server are fine: Handshake delivers between its own relay addresses internally, without depending on your router to loop traffic back in. Nothing else on `external_ip` (the host itself) is ever relayed to. List exceptions in `allowed_peers` (a LAN-only deployment, tests).
- **Not needed for players at home with the server.** Players on the host's own network are [nearby](/guides/connectivity/#nearby-players) and connect directly.

### TURN over TLS on 443 with Caddy

Strict corporate and school networks block UDP and every port but 443, and some check that what goes over 443 is TLS. For those players, offer `turns:` on TCP 443. Caddy already holds 443 and the certificates, so let it terminate TLS for the TURN hostname too and pass plain TURN to Handshake. That takes Caddy's [layer4 app](https://github.com/mholt/caddy-l4), which is not in the stock `caddy` image:

```dockerfile
# Dockerfile next to your compose file; use `build: .` instead of `image: caddy:2`.
FROM caddy:2-builder AS build
RUN xcaddy build --with github.com/mholt/caddy-l4
FROM caddy:2
COPY --from=build /usr/bin/caddy /usr/bin/caddy
```

At the top of the Caddyfile, a global options block puts a layer4 listener wrapper in front of the HTTPS server on :443. Connections for the TURN hostname go to Handshake; everything else goes on to your sites as before:

```text
{
	servers :443 {
		listener_wrappers {
			layer4 {
				@turn tls sni turn.example.com
				route @turn {
					tls                         # terminate TLS with the certificate Caddy manages
					proxy {
						proxy_protocol v2       # tell Handshake who the client is
						upstream tcp/handshake:3478
					}
				}
			}
			tls
		}
	}
}

# Only so Caddy obtains and renews a certificate for the TURN hostname.
turn.example.com {
	respond 404
}

signal.example.com {
	# … as above
}
```

- **Certificate.** The `turn.example.com` site block exists so Caddy gets a certificate for that name; the layer4 `tls` handler uses it. Point the hostname's DNS at the server like your other sites.
- **PROXY protocol.** Tell Handshake where Caddy connects from with `proxy_protocol_from` under `[turn]`, for example `proxy_protocol_from = ["172.16.0.0/12"]` for Caddy on a Docker network. Handshake reads the PROXY v2 header only from those ranges, so per-IP quotas and STUN answers see the player, not Caddy; from anyone else the header is refused. It is empty by default (unlike `trusted_proxies`), because anyone inside a listed range can claim any address and sidestep `allocations_per_ip`: keep it to your proxy's network. A listed peer may also connect without a header, which is how LAN players on that network reach 3478 directly. Without it, every `turns:` player looks like Caddy and they share one address's quota.
- **Other sites are untouched.** Connections whose SNI is not the TURN hostname fall through to the HTTP server, HTTP/2 and all. HTTP/3 (UDP 443) is not affected; `turns:` is TCP only.
- **Verified** with Caddy v2.11.7 and caddy-l4 in Docker: an HTTPS site on the same :443 keeps working, the PROXY header reaches Handshake with the client's address, and a TURN allocation over TLS relays data. `scripts/turns-caddy/check.sh` in the repository runs that check.
- caddy-l4 needs a recent Caddy (the 2.6 series is too old). Upgrading Caddy changes it for every site it serves, so check them after the switch.

Without the layer4 plugin, leave `turns:` out of `urls`: plain `turn:` over UDP and TCP covers cellular and most networks; only the strictest ones need 443.

### Moving from coturn

1. In `[turn]`, add `relay_ports` and `external_ip` (coturn's `min-port`/`max-port` and `external-ip`), point `urls` at Handshake's host, and publish 3478 and the relay range on the Handshake container.
2. Remove the coturn service. `TURN_SECRET` can go too.
3. For `turns:`, replace coturn's TLS listener with the Caddy setup above.

Without `relay_ports`, Handshake keeps minting credentials for an external coturn as before (`TURN_SECRET` must then equal coturn's `static-auth-secret`).

## Secrets

All secrets come from environment variables, never from `config.toml`:

| Variable | Required | Purpose |
| --- | --- | --- |
| `SESSION_SECRET` | yes | Signs session tokens (HMAC-SHA256). At least 32 characters; the server will not start otherwise. Generate one with `openssl rand -hex 32`. |
| `SESSION_SECRET_PREV` | no | Also accepted when verifying tokens, so you can rotate `SESSION_SECRET` without invalidating tokens already handed out. |
| `TURN_SECRET` | no | Signs TURN credentials. With the built-in TURN server, leave it unset and Handshake makes one up at startup. With an external coturn (no `relay_ports`), it must equal coturn's `static-auth-secret`, and without it `/turn` returns `503 turn_unconfigured`. |

To rotate the session secret, move the current value to `SESSION_SECRET_PREV`, set a new `SESSION_SECRET` and restart. Tokens live for `limits.session_ttl_secs` (15 minutes by default), so you can drop `SESSION_SECRET_PREV` after that. The restart itself closes every room (see below), so rotate when nobody is playing.

Two more variables are set in the image and rarely need changing: `CONFIG` (config file path, default `/etc/handshake/config.toml`) and `RUST_LOG` (log filter, default `info`).

## Health check

The image has a `HEALTHCHECK` built in: `handshake --healthcheck` requests `/healthz` on the port from `listen` (distroless images have no `curl`). Docker and Compose use it without extra configuration. `GET /healthz` returns `200 ok` whenever the server is running.

## Metrics

`GET /metrics` returns Prometheus text. Caddy does not proxy it, so scrape it from inside the Docker network (`http://handshake:8080/metrics`).

| Metric | Type | Meaning |
| --- | --- | --- |
| `handshake_sessions_total` | counter | Session tokens issued |
| `handshake_turn_credentials_total` | counter | TURN credentials issued |
| `handshake_joins_rejected_total` | counter | Joins and peeks refused, for any reason |
| `handshake_sockets` | gauge | Live, authenticated WebSockets |
| `handshake_rooms{app}` | gauge | Open rooms per app |
| `handshake_players{app}` | gauge | Members in rooms per app, away members included |
| `handshake_turn_allocations` | gauge | Live TURN allocations (built-in server only, as are the rest) |
| `handshake_turn_allocations_total` | counter | TURN allocations made |
| `handshake_turn_relayed_bytes_total{direction}` | counter | Relayed bytes: `out` client to peer, `in` peer to client |
| `handshake_turn_auth_failures_total` | counter | TURN requests with bad credentials or nonces |
| `handshake_turn_quota_rejections_total` | counter | Allocations refused by `max_allocations`, `allocations_per_ip` or a full port range |

## Logs

The server writes JSON lines to stdout (`docker compose logs handshake`). Room events (created, peer joined, peer removed, room closed) carry the app and room code. Set `RUST_LOG=debug` for more detail.

## State lives in memory

Rooms, resume tokens and rate-limit counters are kept in memory. A restart or redeploy closes every room; clients that try to resume afterwards get their room closed with reason `lost`. Deploy when players are idle.

There is no shared store, so run exactly one instance. One small server handles signaling for many games; game traffic never passes through it.

See also the [HTTP API](/reference/http/) for what Caddy is proxying.

## License

The server is under the [Business Source License 1.1](https://github.com/four43/handshake/blob/main/LICENSE): you may run, modify and self-host it for your own games, commercial ones included, and host it for others free of charge. Charging others to host it is not allowed. Each release becomes GPLv3 four years after it is published. The client, `handshake.js`, is [MIT](https://github.com/four43/handshake/blob/main/client/LICENSE).
