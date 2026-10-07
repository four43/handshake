---
title: Self-hosting
description: Run Caddy, Handshake and coturn on one Docker host, with secrets, health checks, metrics and logs.
---

Handshake is built to run as a single instance on one small Docker host, next to a reverse proxy and a TURN server. Your games stay static (for example on GitHub Pages); only signaling and TURN credential requests reach the server.

## Architecture

| Service | Job |
| --- | --- |
| **Caddy** | Terminates TLS for `https://signal.example.com` with automatic certificates and proxies `/session`, `/turn` and `/ws` to Handshake on port 8080. Nothing else is proxied, `/metrics` included. |
| **handshake** | One Rust binary in a distroless image, running as a non-root user. Reads `config.toml` and secrets from environment variables. |
| **coturn** | The TURN relay, on the host network because large UDP port ranges map badly through Docker. |

Players' browsers talk to Caddy over HTTPS and WebSocket, then to each other or to coturn for game traffic. Handshake never sees game data.

## Compose file

```yaml
services:
  caddy:
    image: caddy:2
    restart: unless-stopped
    ports: ["80:80", "443:443"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data

  handshake:
    image: ghcr.io/four43/handshake:<tag>
    restart: unless-stopped
    environment:
      SESSION_SECRET: ${SESSION_SECRET}
      SESSION_SECRET_PREV: ${SESSION_SECRET_PREV:-}
      TURN_SECRET: ${TURN_SECRET}
    volumes:
      - ./config.toml:/etc/handshake/config.toml:ro
    # no ports: only Caddy reaches it, over the compose network

  coturn:
    image: coturn/coturn
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./turnserver.conf:/etc/coturn/turnserver.conf:ro
      - caddy_data:/caddy:ro   # to reuse Caddy's certificate for turns:
    command: ["-c", "/etc/coturn/turnserver.conf", "--static-auth-secret=${TURN_SECRET}"]

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
  "turns:turn.example.com:5349?transport=tcp",
]
ttl_secs = 3600

[apps.my-game]
origins = ["https://you.github.io"]
max_players = 8
max_rooms = 50
public_rooms = true
turn = true
```

Rate limits and "nearby" are per client IP, so the server must know each player's real address. It reads `X-Forwarded-For` only from peers in `trusted_proxies`, which defaults to loopback and the private ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`). That covers Caddy on the same host or Docker network with no configuration. Any other peer's header is ignored, so a client exposed directly cannot spoof its address.

:::caution
If players reach Handshake from private addresses *without* a proxy (a LAN deployment, or Docker's userland proxy rewriting source addresses), they could set `X-Forwarded-For` themselves. Narrow `trusted_proxies` to your proxy's address, or set `trusted_proxies = []`.
:::

The [config reference](/reference/config/) documents every key, including `[limits]`.

## coturn

A starting `turnserver.conf`:

```text
listening-port=3478
tls-listening-port=5349
realm=turn.example.com
use-auth-secret
# static-auth-secret comes from the command line (TURN_SECRET)
min-port=49160
max-port=49200
# external-ip=<public ip>/<private ip>   # when the box is behind NAT
cert=/caddy/caddy/certificates/acme-v02.api.letsencrypt.org-directory/turn.example.com/turn.example.com.crt
pkey=/caddy/caddy/certificates/acme-v02.api.letsencrypt.org-directory/turn.example.com/turn.example.com.key
fingerprint
no-cli
```

- **`use-auth-secret`** with `static-auth-secret` equal to Handshake's `TURN_SECRET` makes coturn accept the credentials that `/turn` mints. See [Connectivity](/guides/connectivity/#credentials).
- **Relay ports.** Keep the range narrow (here 49160 to 49200) and open it for UDP in your firewall, along with 3478 (UDP and TCP) and the TLS port.
- **`external-ip`** is needed when the host sits behind NAT, as on most cloud VMs with a private address.
- **Certificate.** For `turns:` reuse Caddy's certificate: give `turn.example.com` a site block in the Caddyfile so Caddy obtains a certificate for it, and point `cert` and `pkey` at the files in Caddy's data volume (the path depends on the issuer). Make sure the user coturn runs as can read the key file, and restart coturn after Caddy renews the certificate.
- **Port 443.** `turns:` on 443 gets through the strictest firewalls, but Caddy already holds 443. Give coturn a second IP address, or use 5349 as above and accept that some networks block it.
- **Quotas.** coturn's `user-quota`, `total-quota` and `max-bps` limit relay use. Usernames carry the app ID, so coturn's logs show usage per app.
- **Private networks.** Use coturn's `denied-peer-ip` to stop the relay from reaching addresses inside your own network.

## Secrets

All secrets come from environment variables, never from `config.toml`:

| Variable | Required | Purpose |
| --- | --- | --- |
| `SESSION_SECRET` | yes | Signs session tokens (HMAC-SHA256). At least 32 characters; the server will not start otherwise. Generate one with `openssl rand -hex 32`. |
| `SESSION_SECRET_PREV` | no | Also accepted when verifying tokens, so you can rotate `SESSION_SECRET` without invalidating tokens already handed out. |
| `TURN_SECRET` | for TURN | Must equal coturn's `static-auth-secret`. Without it, `/turn` returns `503 turn_unconfigured`. |

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

## Logs

The server writes JSON lines to stdout (`docker compose logs handshake`). Room events (created, peer joined, peer removed, room closed) carry the app and room code. Set `RUST_LOG=debug` for more detail.

## State lives in memory

Rooms, resume tokens and rate-limit counters are kept in memory. A restart or redeploy closes every room; clients that try to resume afterwards get their room closed with reason `lost`. Deploy when players are idle.

There is no shared store, so run exactly one instance. One small server handles signaling for many games; game traffic never passes through it.

See also the [HTTP API](/reference/http/) for what Caddy is proxying.
