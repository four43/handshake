---
title: Game networking
description: Recommendations for structuring a host-authoritative browser game on top of Handshake's two data channels.
---

:::note
These are recommendations for games, not server features. Handshake never sees your game data; it only connects the host to each guest and gives you two data channels per connection. What you send over them is up to you.
:::

The advice targets physics games (Rapier and three.js) played on phones and tablets, but most of it applies to any real-time game on Handshake.

## Model

Make the host authoritative and use snapshot interpolation:

- The host runs the authoritative physics world at a fixed 60 Hz and broadcasts snapshots at 20 to 30 Hz.
- Guests send their inputs, render remote bodies slightly behind real time by interpolating between snapshots, and predict their own player locally, reconciling against the host's corrections.
- Size the interpolation buffer from the jitter you measure: about 100 ms on a LAN, more over the internet, where 30 to 150 ms of latency is normal.
- Lockstep with a deterministic engine (`@dimforge/rapier3d-deterministic`) is possible but much less forgiving of Wi-Fi jitter. Start with snapshots.

## Replicated objects: keyframes and diffs

Model shared state as a registry of replicated objects instead of writing one message type per feature. This is the same idea as Unreal's replicated actors, Unity's `NetworkObject` and Quake's snapshots.

- **Kinds and authority.** Each kind of object declares its fields (with their quantization) and one authority: the host for world objects, the owning player for that player's avatar or vehicle. Only the authority changes an object.
- **Ownership number.** Each object has an id and an ownership number that goes up every time its owner changes. Receivers ignore data older than what they already have.
- **Keyframes and diffs.** The authority sends a keyframe (full state) on the reliable channel every couple of seconds and to every new joiner. In between it sends diffs on the unreliable channel: only the objects that changed, each with all its fields. Diffs carry absolute values, so a lost diff needs no acknowledgement or resend; the next diff or keyframe heals it.
- **Events for requests.** Requests such as "claim this object", "I hit something" or "deliver" are events on the reliable channel. Their result always comes back as replicated state, never as a separate reply, so a lost reply cannot leave peers disagreeing.
- **Repair at keyframes.** At every keyframe, each peer takes the keyframe's value for everything it is not the authority for. After any loss, peers agree again within one keyframe interval.

In a star topology, a guest's own objects reach other guests through the host: the guest sends to the host, and the host forwards.

## Channels and encoding

Every Handshake peer connection has two data channels:

| Channel | Settings | Use it for | Send with |
| --- | --- | --- | --- |
| `state` | unordered, `maxRetransmits: 0` | snapshots, diffs, inputs | `peer.send(buffer)` |
| `events` | reliable, ordered | joins, keyframes, scoring, chat, requests | `peer.send(data, { reliable: true })` |

```js
peer.send(diffBuffer);                                   // state: ArrayBuffer or typed array only
peer.send(keyframeBuffer, { reliable: true });           // events: binary passes through untouched
peer.send({ type: 'chat', text: 'gg' }, { reliable: true }); // events: plain objects go as JSON

peer.on('message', (data, { reliable }) => {
  // data is an ArrayBuffer for binary, or the parsed object for JSON
});
```

- Use binary `ArrayBuffer`s for anything sent often. JSON is fine for rare events like chat, but not for state.
- Quantize positions and rotations (quaternions) to small integers, and send only bodies that are awake or changed.
- Rapier's `world.takeSnapshot()` is for late joiners only. It is far too large to send every tick.
- `peer.send()` returns `false` when the channel is not open; nothing is queued. On the unreliable channel that is fine; for events, decide whether to retry or rely on the next keyframe.

## Budget

About 20 quantized bodies at 30 Hz is roughly 100 kbps per guest. With 7 guests the host uploads about 0.7 Mbps, which phones on cellular or busy Wi-Fi can struggle with. Relayed connections push the same traffic through your TURN server, so its bandwidth grows with the share of relayed players (see [Connectivity](/guides/connectivity/)).
