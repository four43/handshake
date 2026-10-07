---
title: Rooms
description: Creating and joining rooms, codes and keys, public and private rooms, host controls, and what happens when a connection drops.
---

A room is one host plus the guests who joined it. It belongs to one app and one game protocol version. Rooms live in the server's memory only: a server restart closes them all.

The topology is a star. Each guest connects to the host and only the host; guests never connect to each other, and the server only relays signaling between the host and each guest.

## Creating a room

```js
const hs = new Handshake({ server, app: 'pig-pens', version: 3, name: 'Seth' }); // your display name

const room = await hs.createRoom({
  name: 'Seth\'s farm', // shown in listings and peeks; default "Room"
  public: false,         // default false
  maxPlayers: 4,         // includes the host
  meta: { map: 'farm' }  // optional, any JSON up to 1 KB
});
room.code;     // 'K7MX2'
room.key;      // the private key
room.shareUrl; // this page's URL with ?r=K7MX2&k=...
room.isHost;   // true
room.you;      // your peer id
```

Names are cut to 32 characters. Without a `name`, players show as `"Host"` and `"Player"`.

The server checks a few things first:

| Error code | Cause |
| --- | --- |
| `public_disabled` | `public: true`, but the app has `public_rooms = false` |
| `too_many_rooms` | The app already has `max_rooms` rooms open |
| `meta_too_large` | `meta` is more than 1 KB as JSON |
| `already_in_room` | This client is already in a room |

`maxPlayers` is clamped between 2 and the app's `max_players`; leave it out to get the app's limit. See the [config reference](/reference/config/) for the app keys.

## Codes and keys

Every room gets a **code** and a **key**:

- The code is 5 characters from an alphabet without look-alikes (no `0`/`O`, `1`/`I`/`L`), unique within the app. The client and server trim and uppercase what the player types, so `k7mx2 ` works.
- The key is a random, URL-safe string. The server gives it to the host only; a guest's `room.key` is the key it joined with.

`room.shareUrl` is the current page's URL with `r=<code>` and `k=<key>` added (other parameters are kept). Show it as a QR code for players in the same room, or pass it to `navigator.share()` for remote friends. On load, `joinFromUrl()` joins the room such a link points to, and resolves `null` when the page has no `r` parameter:

```js
const room = (await hs.joinFromUrl()) ?? (await showLobby());
```

`shareUrl` is `null` outside a browser page. To build a link to another page, use the exported `shareUrl(href, code, key)` helper.

## Private and public rooms

- **Private rooms** (the default) need the code and the key. A wrong or missing key fails with `bad_key`.
- **Public rooms** need only the code, which players can type or read aloud. The app must allow them with `public_rooms = true`.

The server can also list an app's public rooms: those on the caller's version that are unlocked, not full and whose host is connected, nearby rooms first, then newest, at most 50. Set `list = "none"` on an app to make every listing empty, so a public room is joinable with its code but its code is never handed out:

```toml
[apps.tractor-pickup]
origins = ["https://four43.com"]
public_rooms = true   # joinable with the code alone
list = "none"         # but never listed
```

```js
const rooms = await hs.listRooms();
// [{ code, name, players, maxPlayers, meta, nearby }]
```

:::note
A "knock to join" listing mode is planned.
:::

## Joining

```js
const room = await hs.joinRoom(code, key); // key only for a private room
```

| Error code | Cause |
| --- | --- |
| `not_found` | No room with that code in this app |
| `bad_key` | Private room, wrong or missing key |
| `version_mismatch` | The room runs a different game version |
| `locked` | The host locked the room |
| `full` | The room has `maxPlayers` members |
| `rate_limited` | Too many join attempts from this IP, or too many failed attempts for this app |
| `already_in_room` | This client is already in a room |

Join attempts are rate limited per IP (`joins_per_min`), and failed joins (`not_found`, `bad_key`) are also counted per app across all IPs (`app_failed_joins_per_min`), so codes and keys cannot be brute-forced from many addresses.

## Peeking before you join

`peek` looks at a room without joining it, so the game can show "Join this room?" first:

```js
const info = await hs.peek(code, key);
// { code, name, players, maxPlayers, locked, full }
```

Peek follows the same rules as join: a public room needs only the code, a private room needs its key (`bad_key` otherwise), and a different version gives `version_mismatch`. A peek counts as a join attempt for rate limits, and its failures count as failed joins. You cannot peek while you are in a room.

## Version pinning

The `version` you pass to `new Handshake()` is baked into the session token. A join or peek from a different version fails with `version_mismatch`, and listings only include rooms on the caller's version. This keeps a cached old build from joining a room that runs newer netcode. Bump `version` whenever a change to your messages would break an older client.

## Host controls

Only the host can use these. Each returns a promise that resolves once the server has applied the change:

```js
await room.lock(true);                         // stop new joins, e.g. when the match starts
await room.setMeta({ map: 'farm', round: 2 }); // replace the meta blob (JSON, max 1 KB)
await room.kick(peerId);                       // remove a guest
```

They reject with a `HandshakeError`:

| Code | Cause |
| --- | --- |
| `not_host` | You are a guest (checked before anything is sent) |
| `meta_too_large` | `meta` is more than 1 KB as JSON |
| `peer_unavailable` | No such peer to kick |
| `network` | The socket is down, for example while the client resumes. Nothing is queued: try again after the room's `members` event. |
| `timeout` | No answer within 10 seconds |
| `closed` | The room ended first |

Catch the rejection if you call them without `await`, or the browser reports an unhandled rejection.

- `lock` and `setMeta` reach every member, the host included, as a `meta` event with `{ meta, locked }`. `room.meta` and `room.locked` update to match.
- The server never reads `meta`. Use it for whatever your lobby shows: map, mode, whether a match is in progress.
- A kicked guest's room closes with reason `kicked`; everyone else gets `peerLeft` with reason `kicked`. Kicked players can rejoin unless the room is locked.

## Members and events

`room.members` is the list of everyone in the room, host included: `{ id, name, nearby, away }`. `room.peers` is a `Map` of the open WebRTC connections: on the host, one per guest; on a guest, just the host. Each peer has its player's `name`. `room.name` is the room's name.

| Event | Arguments | When |
| --- | --- | --- |
| `peer` | `peer` | A peer connection's data channels opened |
| `members` | `members` | The member list changed |
| `peerAway` | `id` | A guest's socket dropped (host only) |
| `peerBack` | `id` | That guest resumed (host only) |
| `peerLeft` | `id`, `reason` | A guest was removed: `left`, `timeout` or `kicked` |
| `hostAway` | `graceSecs` | The host's socket dropped (guests) |
| `hostBack` | | The host resumed (guests) |
| `meta` | `{ meta, locked }` | The host changed meta or the lock |
| `closed` | `reason` | The room ended for you |

`closed` reasons:

| Reason | Meaning |
| --- | --- |
| `left` | You called `room.leave()` or `hs.close()` |
| `host_left` | The host left |
| `host_gone` | The host did not come back within the grace period |
| `kicked` | The host kicked you |
| `expired` | The room reached its maximum age |
| `idle` | The host was alone too long |
| `replaced` | The same session resumed on another connection |
| `lost` | The client could not resume (the room or your slot is gone) |

See the [JS client API](/reference/client/) for every property and method.

## Lifecycle and resume

Every member, host included, holds a resume token. Phones drop sockets constantly, so a disconnect starts a grace period instead of removing anyone.

| Event | Effect |
| --- | --- |
| Guest's socket drops | Host gets `peerAway`; the slot is held for the grace period (30 s) |
| Guest resumes in time | Host gets `peerBack` |
| Guest's grace expires, or the guest leaves | Guest removed; members get `peerLeft` |
| Host's socket drops | Guests get `hostAway`; the room is held for the grace period |
| Host resumes in time | Guests get `hostBack` |
| Host's grace expires, or the host leaves | Room closed (`host_gone` or `host_left`) |
| Room older than 12 h | Room closed (`expired`) |
| Host alone for 30 min | Room closed (`idle`) |

The timings come from `[limits]` in the config (`grace_secs`, `room_max_age_secs`, `idle_room_secs`). "Alone" starts when the room is created and whenever the last guest leaves. A guest that resumes while its old socket still looks open causes `peerBack` without an earlier `peerAway`.

Only the signaling socket is resumed. WebRTC connections between players are separate and often survive a short signaling outage untouched.

### What the client does for you

You do not write any of the resume logic:

- **Resume with backoff.** When the socket closes while you are in a room, the client reconnects and sends `resume`, retrying after 0.25, 1, 2, 4 and then every 8 seconds. Signaling messages queued while it was down are sent after the resume, and the client catches up on members who left in the meantime. If the server no longer knows the room or your slot, the room closes with reason `lost`.
- **Heartbeat.** iOS can keep a dead socket looking open. Every 25 seconds the client makes a round trip to the server; if it gets no answer, it drops the socket and resumes. The same tick renews TURN credentials before they expire.
- **Page visibility and network.** When the page becomes visible again, or the browser fires `online`, the client checks the socket at once and resumes immediately if it is gone, instead of waiting for the next backoff step. On `online` it also restarts ICE on every peer connection (see [Connectivity](/guides/connectivity/)).
- **Session tokens.** The client fetches and renews the session token itself.

## Host migration is the game's job

The server does not move a room to a new host. It announces `hostAway` and, if the host does not return within the grace period, closes the room with `host_gone`. If your game wants to continue, it decides which guest takes over, that player creates a new room, and the others join it.

Guests are not connected to each other, so once the host is gone they have no channel of their own to pass on a new code: the new host shares a fresh link the usual way. If you want the hand-over to be smooth, have the host tell every guest who the successor is while it is still connected, so the game can show the right screen to each player.
