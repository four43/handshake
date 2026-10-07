---
title: HTTP API
description: The Handshake server's HTTP endpoints, with requests, responses, auth and error codes.
---

The server has four HTTP endpoints and one WebSocket. The [JS client](/reference/client/) calls all of them for you; this page is for writing another client or debugging one. Messages on the WebSocket are described in the [protocol reference](/reference/protocol/).

| Endpoint | Auth | Purpose |
| --- | --- | --- |
| `POST /session` | `Origin` header | Get a session token for an app and game version |
| `POST /turn` | `Authorization: Bearer <token>` | Get short-lived TURN credentials |
| `GET /ws` | `Origin` header, then the token in `hello` | Signaling WebSocket |
| `GET /healthz` | none | Liveness check |
| `GET /metrics` | none; keep it off the public proxy | Prometheus metrics |

## Errors

JSON endpoints report errors as a JSON body with a single `error` code:

```json
{ "error": "origin" }
```

| HTTP status | `error` | Endpoint | Meaning |
| --- | --- | --- | --- |
| 403 | `origin` | `/session`, `/ws` | The `Origin` header is not allowed |
| 404 | `not_found` | `/session` | No app with that ID in the config |
| 429 | `rate_limited` | `/session` | Too many sessions from this IP this minute (`limits.sessions_per_min`) |
| 401 | `bad_token` | `/turn` | Missing, invalid or expired session token |
| 403 | `turn_disabled` | `/turn` | The app has `turn = false` |
| 503 | `turn_unconfigured` | `/turn` | The server has no `[turn]` section or no `TURN_SECRET` |

A `POST /session` body that is not valid JSON, lacks a field, or is sent without `Content-Type: application/json` is rejected by the framework with a 4xx status and a plain-text body, not a JSON `error`. The JS client reports any non-JSON error as `HandshakeError` code `network`.

## CORS

`/session` and `/turn` answer CORS preflights for any origin listed by any app (plus `http://localhost:*` and `http://127.0.0.1:*` when `allow_localhost = true`). Allowed: method `POST`, headers `Content-Type` and `Authorization`. Preflights are cached for 10 minutes.

## POST /session

Mints a session token for one app and one game protocol version. The `Origin` header must match one of the app's `origins` (or be a localhost origin with `allow_localhost = true`). Rate limited per client IP.

```http
POST /session
Origin: https://you.github.io
Content-Type: application/json

{ "app": "my-game", "version": 3 }
```

| Field | Type | Meaning |
| --- | --- | --- |
| `app` | string | The app ID, an `[apps.<id>]` key in the config |
| `version` | integer | The game's own network protocol version (not the signaling version). Rooms only match the same version. |

Response `200`:

```json
{
  "token": "eyJhIjoibXktZ2FtZSIsInYiOjMsImUiOjE3NjcyMjU2MDB9.3q2-7w…",
  "expires_in": 900,
  "turn": true
}
```

| Field | Meaning |
| --- | --- |
| `token` | The session token. Treat it as opaque. It carries the app ID, the version and an expiry, signed with `SESSION_SECRET`. |
| `expires_in` | Seconds until it expires (`limits.session_ttl_secs`, default 900). Fetch a new one before then. |
| `turn` | `true` when `POST /turn` will hand out credentials: the app has `turn = true`, the config has `[turn]`, and `TURN_SECRET` is set. |

## POST /turn

Mints TURN credentials for the app in the session token. The body is empty.

```http
POST /turn
Authorization: Bearer eyJhIjoibXktZ2FtZSIsInYiOjMsImUiOjE3NjcyMjU2MDB9.3q2-7w…
```

Response `200`:

```json
{
  "ice_servers": [
    {
      "urls": [
        "stun:turn.example.com:3478",
        "turn:turn.example.com:3478?transport=udp",
        "turn:turn.example.com:3478?transport=tcp",
        "turns:turn.example.com:443?transport=tcp"
      ],
      "username": "1767229200:my-game",
      "credential": "n12nWdEgEYR9Wpg+vbxX8n3V8VA="
    }
  ],
  "ttl": 3600
}
```

`ice_servers` drops straight into `new RTCPeerConnection({ iceServers })`. `urls` is the `[turn] urls` list from the config. The username is `<expiry unix time>:<app id>` and the credential is base64 HMAC-SHA1 of the username keyed by `TURN_SECRET`, as coturn's `use-auth-secret` expects. `ttl` is `[turn] ttl_secs` (default 3600). See [Connectivity](/guides/connectivity/#credentials).

## GET /ws

The signaling WebSocket. Connect with `wss://` (or `ws://` locally) to the same host.

1. The `Origin` header must be allowed by at least one app; otherwise the upgrade is refused with `403 {"error":"origin"}`.
2. Within 10 seconds, send `hello` with the signaling version and the session token. The token goes in this message, not the URL, so it stays out of proxy logs:

   ```json
   { "t": "hello", "v": 1, "token": "eyJhIjoibXktZ2FtZSIsInYiOjMsImUiOjE3NjcyMjU2MDB9.3q2-7w…" }
   ```

3. The server replies `{ "t": "welcome", "v": 1 }`, and the socket is ready.

If `hello` fails, the server sends one error message and closes the socket:

| `code` | Cause |
| --- | --- |
| `bad_message` | `v` is not a supported signaling version (currently only `1`) |
| `bad_token` | The token is invalid or expired, or the first message was not `hello` |
| `origin` | The `Origin` is not allowed for the token's app |

No `hello` within 10 seconds closes the socket without a message.

Once open:

- Messages are JSON text frames tagged by a `t` field; see the [protocol reference](/reference/protocol/).
- A message may be at most 16 KB.
- The server pings every 20 seconds and closes a socket that has sent nothing (including pongs) for 60 seconds.
- A socket that falls 64 messages behind is disconnected.

## GET /healthz

Returns `200` with the body `ok` while the server runs. The Docker image's health check uses it.

```bash
curl http://localhost:8080/healthz
# ok
```

## GET /metrics

Prometheus text format. Do not expose it publicly; see [Self-hosting](/guides/self-hosting/#metrics) for the metric list.

```text
# TYPE handshake_sessions_total counter
handshake_sessions_total 42
# TYPE handshake_turn_credentials_total counter
handshake_turn_credentials_total 40
# TYPE handshake_joins_rejected_total counter
handshake_joins_rejected_total 3
# TYPE handshake_sockets gauge
handshake_sockets 7
# TYPE handshake_rooms gauge
handshake_rooms{app="my-game"} 2
# TYPE handshake_players gauge
handshake_players{app="my-game"} 7
```
