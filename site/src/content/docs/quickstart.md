---
title: Quickstart
description: Run a Handshake server locally, add the client to a game, and connect two browser tabs.
---

This page gets two browser tabs talking over WebRTC through a local Handshake server. You need Docker and a game (or any page) served from `http://localhost`.

## 1. Write a config

Create `config.toml`. Each game is an `[apps.<id>]` entry; `allow_localhost` lets pages on `http://localhost:*` and `http://127.0.0.1:*` use every app, so you can develop without listing your dev server's origin.

```toml
listen = "0.0.0.0:8080"
allow_localhost = true   # development only

[apps.my-game]
origins = ["https://you.github.io"]   # where the game is deployed
max_players = 4
public_rooms = true
```

The container runs as a non-root user, so the file must be world-readable:

```bash
chmod 644 config.toml
```

No `[turn]` section is needed for two tabs on one machine. See [Connectivity](/guides/connectivity/) before you play across networks, and the [config reference](/reference/config/) for every key.

## 2. Run the server

Images are published to the GitHub Container Registry. A push to `main` publishes `ghcr.io/four43/handshake:<short commit hash>`; a git tag publishes `ghcr.io/four43/handshake:<tag>`. There is no `latest` tag, so pick one from the [package page](https://github.com/four43/handshake/pkgs/container/handshake).

```bash
docker run --rm -p 8080:8080 \
  -e SESSION_SECRET=$(openssl rand -hex 32) \
  -v "$PWD/config.toml:/etc/handshake/config.toml:ro" \
  ghcr.io/four43/handshake:<tag>
```

Or build the image from a checkout of the repository:

```bash
docker build -t handshake .
docker run --rm -p 8080:8080 \
  -e SESSION_SECRET=$(openssl rand -hex 32) \
  -v "$PWD/config.toml:/etc/handshake/config.toml:ro" \
  handshake
```

`SESSION_SECRET` is required and must be at least 32 characters. Check that the server is up:

```bash
curl http://localhost:8080/healthz   # ok
```

## 3. Add the client to your game

The client is one ES module with no dependencies and no build step. Copy [`client/handshake.js`](https://github.com/four43/handshake/blob/main/client/handshake.js) into your game and import it:

```js
import { Handshake, HandshakeError } from './handshake.js';

const hs = new Handshake({
  server: 'http://localhost:8080',   // https://signal.example.com in production
  app: 'my-game',                    // the [apps.<id>] key
  version: 1,                        // your game's network protocol version
  name: 'Seth',                      // your display name, shown to other players
});
```

`version` is your game's own netcode version, an integer. Players only meet players on the same version, so bump it whenever an old build could not talk to a new one.

## 4. Host a room

```js
const room = await hs.createRoom({ name: "Seth's farm", maxPlayers: 4 });

console.log('Share this:', room.shareUrl);   // this page + ?r=<code>&k=<key>: a QR code, or navigator.share()

room.on('peer', peer => {
  // The data channels to this guest are open.
  peer.send({ type: 'welcome' }, { reliable: true });   // objects go as JSON, reliable only
  peer.on('message', (data, { reliable }) => console.log(peer.name, reliable, data));
});
room.on('peerLeft', (id, reason) => console.log('left', id, reason));
room.on('closed', reason => console.log('room closed', reason));
```

Rooms are private by default: guests need both the code and the key, and the share link carries both.

## 5. Join as a guest

Open the share link in a second tab. `joinFromUrl()` reads `r` and `k` from the page's URL and joins; it resolves `null` when there is no `r`, so you can call it on every load:

```js
const room = await hs.joinFromUrl();   // or hs.joinRoom(code, key) for a code the player typed

room?.on('peer', host => {
  // A guest has exactly one peer: the host.
  const input = new Uint8Array([1, 0, 255]);
  host.send(input);                                  // unreliable channel: binary only
  host.send({ type: 'chat', text: 'hi' }, { reliable: true });
  host.on('message', data => console.log('from', host.name, data));
});
room?.on('hostAway', graceSecs => console.log(`host dropped; waiting up to ${graceSecs}s`));
room?.on('closed', reason => console.log('room closed', reason));
```

`peer.send()` returns `false` if the channel is not open yet, and throws a `TypeError` if you pass a plain object without `{ reliable: true }`.

## 6. Handle errors

Every promise from the client rejects with a `HandshakeError` whose `code` is a server error code or one of `network`, `timeout` or `closed`:

```js
let room;
try {
  room = await hs.joinRoom(code, key);
} catch (e) {
  if (!(e instanceof HandshakeError)) throw e;
  switch (e.code) {
    case 'not_found':        show('No room with that code.'); break;
    case 'bad_key':          show('That link is not valid any more.'); break;
    case 'full':             show('The room is full.'); break;
    case 'locked':           show('The game has already started.'); break;
    case 'version_mismatch': show('Update the game to join this room.'); break;
    case 'rate_limited':     show('Too many tries. Wait a minute.'); break;
    default:                 show(`Could not join (${e.code}).`);
  }
}
```

When the player is done, call `room.leave()`. Call `hs.close()` to leave and shut the client down for good.

## Next steps

- [Rooms](/guides/rooms/): public and private rooms, peeking, host controls and what happens when a phone drops its connection.
- [Connectivity](/guides/connectivity/): TURN, which you need for players on different networks.
- [Game networking](/guides/game-networking/): how to structure the traffic your game sends.
- [Self-hosting](/guides/self-hosting/): Caddy, Handshake and coturn on one Docker host.
- [JS client API](/reference/client/) and [config reference](/reference/config/).
