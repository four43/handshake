# Handshake: Signaling Server Specification

| Item | Value |
|---|---|
| Status | Draft |
| Date | 2026-10-05 |
| Owner | Seth Miller |
| Language | ASD-STE100 Simplified Technical English |

## 1. Purpose

Handshake is a WebRTC signaling server for the four43.com browser games.

Two browsers cannot find each other without help. Handshake gives that help.
It sends connection data between two browsers. Then the browsers connect directly.
After that, the game data does not go through Handshake.

Handshake also finds games on the same network.
A player opens a game and sees a list of nearby games.
The player selects a game and joins it. The player does not type a code.

## 2. Scope

### 2.1 In scope

- A WebSocket server that relays WebRTC offers, answers, and ICE candidates.
- Rooms. One player is the host of a room. Other players are guests.
- Discovery of rooms on the same network.
- Short room codes. A player on a different network can join with a code.
- A JavaScript client library for the games.
- A Docker image and a Docker Compose file.

### 2.2 Out of scope

- Game logic. The host browser runs the game.
- Relay of game data through the server.
- User accounts, passwords, and logins.
- A database or other persistent storage.
- Host migration. If the host leaves, the room closes.
- A TURN server. Section 10 gives a possible future design.

## 3. Terms

| Term | Meaning |
|---|---|
| Peer | One browser tab that has a connection to Handshake. |
| Host | The peer that makes a room. The host runs the game simulation. |
| Guest | A peer that joins a room. |
| Room | One game session. A room has one host and zero or more guests. |
| Room code | A 4-letter code that identifies a room. Example: `PGKX`. |
| Network group | All peers that have the same public network address. |
| Signal | WebRTC connection data: an SDP offer, an SDP answer, or an ICE candidate. |
| Signaling | The exchange of signals between two peers before they connect directly. |

## 4. System overview

```text
  Browser A (host)                 Handshake                 Browser B (guest)
  ----------------                 ---------                 -----------------
        | --- host -------------->     |                            |
        |                              | <-------------- hello ---- |
        |                              | ---- rooms (A's room) ---> |
        |                              | <-------------- join ----- |
        | <------ peer-joined -------- |                            |
        | --- signal (offer) -------->  | ---- signal (offer) -----> |
        | <------ signal (answer) ---- | <--- signal (answer) ----- |
        | <======== direct WebRTC data channels (game data) =======> |
```

The topology is a star. Each guest has one direct connection to the host.
Guests do not connect to other guests.

## 5. Requirements

Each requirement has an identifier. Each test must refer to the identifier of its requirement.

### 5.1 Connection

- **HS-CON-1** The server must accept WebSocket connections on the path `/ws`.
- **HS-CON-2** The server must reject a connection from an origin that is not in the allowed list. The server must close the socket with code `4003`.
- **HS-CON-3** The server must give each peer a random peer ID. The ID must have a minimum of 64 bits of randomness.
- **HS-CON-4** The server must send a `welcome` message immediately after the peer connects.
- **HS-CON-5** The server must send a WebSocket ping every 20 seconds. If the peer does not reply in 10 seconds, the server must close the connection.
- **HS-CON-6** When a peer disconnects, the server must remove the peer from all rooms.

### 5.2 Network groups

- **HS-NET-1** The server must put a peer into a network group. The group key is the public IP address of the peer.
- **HS-NET-2** For an IPv6 address, the group key must be the `/64` prefix of the address.
- **HS-NET-3** The server must read the client address from the `X-Forwarded-For` header only when the direct connection comes from a trusted proxy address.
- **HS-NET-4** If the request does not come from a trusted proxy, the server must use the socket address. The server must ignore `X-Forwarded-For`.
- **HS-NET-5** The server must not send the IP address or the group key to any peer.

### 5.3 Rooms

- **HS-ROOM-1** A peer can make a room with a `host` message. The server must reply with a `hosted` message. This message contains the room ID and the room code.
- **HS-ROOM-2** A peer can be the host of a maximum of one room at a time.
- **HS-ROOM-3** A room code must have 4 uppercase letters. The code must not contain the letters `I` or `O`.
- **HS-ROOM-4** A room code must be unique on the server while the room is open.
- **HS-ROOM-5** A room has a `maxPlayers` value. The host is one of the players. The value must be from 2 to 8.
- **HS-ROOM-6** When the host disconnects or sends `unhost`, the server must close the room. The server must send `room-closed` to all guests of the room.
- **HS-ROOM-7** A room has a `game` value, for example `pig-pens`. The server must show a room only to peers that have the same `game` value.

### 5.4 Discovery

- **HS-DISC-1** The server must send a `rooms` message to a peer after the `hello` message.
- **HS-DISC-2** The `rooms` message must contain only the rooms in the same network group and the same game as the peer.
- **HS-DISC-3** When a room in the network group opens, closes, or changes its player count, the server must send a new `rooms` message to all peers in that group and game.
- **HS-DISC-4** A room with the `private` flag set must not be in any `rooms` message. A peer can join a private room only with its room code.

### 5.5 Join

- **HS-JOIN-1** A peer can join a room with the room ID. This is possible only if the peer is in the same network group as the room.
- **HS-JOIN-2** A peer can join a room with the room code from any network.
- **HS-JOIN-3** If the room is full, the server must send `error` with code `room-full`.
- **HS-JOIN-4** If the room does not exist, the server must send `error` with code `room-not-found`.
- **HS-JOIN-5** When a guest joins, the server must send `peer-joined` to the host. The message contains the guest peer ID and the guest name.
- **HS-JOIN-6** When a guest leaves, the server must send `peer-left` to the host.

### 5.6 Signal relay

- **HS-SIG-1** The server must relay a `signal` message from one peer to the peer in the `to` field.
- **HS-SIG-2** The server must relay a signal only between a host and a guest of the same room. Other signals must get `error` with code `not-allowed`.
- **HS-SIG-3** The server must not change, read, or keep the `data` field of a signal.
- **HS-SIG-4** The server must not write signal data to the logs.

### 5.7 Limits

- **HS-LIM-1** The maximum size of one message is 16 KB. The server must close the connection if a message is larger.
- **HS-LIM-2** The maximum rate is 50 messages per second for each peer. The server must drop the excess messages and send `error` with code `rate-limited`.
- **HS-LIM-3** The maximum is 20 connections from one network group.
- **HS-LIM-4** The maximum length of a player name is 32 characters. The maximum length of a room title is 48 characters. The server must cut longer values to the maximum length.
- **HS-LIM-5** The server must reject a message that is not valid JSON. The server must send `error` with code `bad-message`.

### 5.8 Operation

- **HS-OPS-1** The server must reply `200 OK` to `GET /healthz`.
- **HS-OPS-2** The server must serve the client library at `GET /handshake.js`.
- **HS-OPS-3** The server must write logs to stdout in JSON Lines format.
- **HS-OPS-4** The server must stop correctly when it gets `SIGTERM`. It must close all sockets with code `1001`.
- **HS-OPS-5** The server must keep all state in memory. A restart removes all rooms.

## 6. Protocol

All messages are JSON objects. Each message has a `type` field.

### 6.1 Messages from the peer to the server

| Type | Fields | Description |
|---|---|---|
| `hello` | `game`, `name` | Identifies the game and the player name. Send this first. |
| `host` | `title`, `maxPlayers`, `private` | Makes a room. |
| `unhost` | — | Closes the room of this peer. |
| `join` | `roomId` or `code` | Joins a room. |
| `leave` | — | Leaves the current room. |
| `signal` | `to`, `data` | Sends WebRTC data to one peer. |

### 6.2 Messages from the server to the peer

| Type | Fields | Description |
|---|---|---|
| `welcome` | `peerId`, `iceServers` | Sent after the connection opens. |
| `rooms` | `rooms[]` | The list of rooms that the peer can see. |
| `hosted` | `roomId`, `code` | The server made the room. |
| `joined` | `roomId`, `hostId` | The guest is in the room. The guest must wait for an offer from the host. |
| `peer-joined` | `peerId`, `name` | Sent to the host. A guest joined. |
| `peer-left` | `peerId` | Sent to the host. A guest left. |
| `room-closed` | `roomId` | Sent to guests. The host closed the room. |
| `signal` | `from`, `data` | WebRTC data from another peer. |
| `error` | `code`, `message` | A request failed. |

A room in the `rooms[]` list has these fields:
`roomId`, `title`, `hostName`, `players`, `maxPlayers`.

### 6.3 Error codes

| Code | Cause |
|---|---|
| `bad-message` | The message is not valid JSON, or a field is missing or incorrect. |
| `not-allowed` | The peer cannot do this action. |
| `room-full` | The room has `maxPlayers` players. |
| `room-not-found` | The room ID or room code does not exist. |
| `rate-limited` | The peer sent too many messages. |

### 6.4 Connection sequence

1. The guest sends `join`.
2. The server sends `joined` to the guest and `peer-joined` to the host.
3. The host makes an `RTCPeerConnection` for the guest.
4. The host makes the data channels (section 7.3) and an SDP offer.
5. The host sends the offer in a `signal` message.
6. The guest sends an SDP answer in a `signal` message.
7. Both peers send ICE candidates in `signal` messages.
8. The data channels open. The game starts to send data.

The host always makes the offer. This prevents a conflict when two peers make offers at the same time.

## 7. Client library

The server serves the library at `/handshake.js`. It is one ES module. It has no dependencies.

### 7.1 Example: host

```js
import { Handshake } from 'https://handshake.four43.com/handshake.js';

const hs = new Handshake({ game: 'pig-pens', name: 'Seth' });
const room = await hs.host({ title: "Seth's farm", maxPlayers: 4 });

room.on('peer', (peer) => {
  peer.on('message', (channel, data) => { /* guest input */ });
});

room.broadcast('state', gameState);
```

### 7.2 Example: guest

```js
const hs = new Handshake({ game: 'pig-pens', name: 'Ada' });

hs.on('rooms', (rooms) => showLobby(rooms));

const room = await hs.join({ roomId });   // or: hs.join({ code: 'PGKX' })
room.host.on('message', (channel, data) => { /* game state */ });
room.host.send('input', { x: 1, y: 0 });
```

### 7.3 Data channels

The library makes two data channels for each connection:

| Channel | Settings | Use |
|---|---|---|
| `state` | `ordered: false`, `maxRetransmits: 0` | Positions and inputs. A lost message is not important. |
| `events` | `ordered: true`, reliable | Scores, gates, and other events. Each message must arrive. |

`send()` and `broadcast()` use JSON by default.
They send an `ArrayBuffer` without a change.

### 7.4 Library requirements

- **HS-LIB-1** The library must connect to the server URL `wss://handshake.four43.com/ws` by default. A `url` option changes it.
- **HS-LIB-2** If the signaling connection stops, the library must connect again. The wait time must start at 1 second and increase to a maximum of 30 seconds.
- **HS-LIB-3** When the signaling connection stops, the open WebRTC connections must continue.
- **HS-LIB-4** The library must use the `iceServers` value from the `welcome` message.
- **HS-LIB-5** The library must send an event when a peer connection opens and when it closes.

## 8. Security

- The server keeps no personal data. It does not keep messages after it relays them.
- The server does not show IP addresses to peers (HS-NET-5).
- A peer can relay signals only inside its room (HS-SIG-2).
- Room codes are not secret. Do not use a room code as a password.
- A peer can set a false name. The game must not trust the name.
- The host must check all guest inputs. Guests can send false data.

## 9. Deployment

### 9.1 Technology

| Item | Choice |
|---|---|
| Runtime | Node.js 22 LTS |
| WebSocket library | `ws` |
| Image | `node:22-alpine`, run as a non-root user |
| Port | `8080` (HTTP and WebSocket) |

### 9.2 Configuration

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8080` | The listen port. |
| `ALLOWED_ORIGINS` | `https://four43.com,http://localhost:4000` | Origins that can connect. Separate the values with commas. |
| `TRUSTED_PROXIES` | `127.0.0.1/32,::1/128` | Proxy addresses. The server reads `X-Forwarded-For` only from these addresses. |
| `STUN_URLS` | `stun:stun.l.google.com:19302` | STUN servers for the `welcome` message. |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn`, or `error`. |

### 9.3 Network

1. Put Handshake behind a reverse proxy with TLS. Use `handshake.four43.com`.
2. Make sure that the proxy sends WebSocket upgrade requests to port `8080`.
3. Make sure that the proxy sets the `X-Forwarded-For` header.
4. Make the hostname available from the internet. Use a port forward or a Cloudflare Tunnel.

> **CAUTION:** Do not point the hostname to a private address (for example `192.168.x.x`). Chrome blocks or asks about requests from a public site to a private address. Phones do not show this prompt clearly.

> **NOTE:** The site four43.com uses HTTPS. Browsers then allow only `wss://` connections. A connection with `ws://` fails.

## 10. Possible future work

- **TURN.** Players on different networks sometimes cannot connect directly. A `coturn` server can relay their data. Handshake can give time-limited TURN credentials in the `welcome` message. Use the coturn shared-secret (REST API) method.
- **Host migration.** When the host leaves, a guest can become the new host.
- **Metrics.** A `/metrics` endpoint for Prometheus with the counts of peers and rooms.

## 11. Tests

| Level | Tool | What to test |
|---|---|---|
| Unit | `node:test` | Network group keys, room codes, limits, message validation. |
| Integration | `node:test` and `ws` clients | The full protocol with 2 or more clients. Each `HS-*` server requirement. |
| Browser | Playwright | Two pages in one browser. One page hosts, one page joins. Data goes through both channels. |

## 12. Open questions

1. What runs the home servers: Docker Compose, k3s, or a different system?
2. Which reverse proxy is in use: Caddy, Traefik, nginx, or a different proxy?
3. Is Node.js correct, or is Go better for this environment?
4. Do the games need player names, or is a random farm-animal name sufficient?
