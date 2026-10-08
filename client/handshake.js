// SPDX-License-Identifier: MIT
// MIT License
//
// Copyright (c) 2026 Seth Miller
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
// associated documentation files (the "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO
// EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
// IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.
//
// This file is MIT-licensed so games can vendor it freely; the server (the rest of
// https://github.com/four43/handshake) is under the Business Source License 1.1.

// handshake.js: the browser client for the Handshake signaling server (docs/specs/handshake-server.md, "JS client library").
// One plain ES module with no dependencies; each game keeps a copy. The server matches players into
// rooms and relays WebRTC signaling; game data goes peer to peer (host <-> each guest) over two data
// channels: `state` (unordered, no resends) and `events` (reliable, ordered).

const SIGNAL_VERSION = 1;
const HELLO_MS = 10_000;          // the server closes a socket that has not said hello in 10 s
const REQUEST_MS = 10_000;        // longest wait for a reply to create, join, peek, resume or list
const HEARTBEAT_MS = 25_000;      // a `list` round trip proves the socket lives (iOS keeps dead sockets "open")
const RESUME_BACKOFF_MS = [250, 1_000, 2_000, 4_000, 8_000];
const REFRESH_EARLY_MS = 60_000;  // renew a session token or TURN credential this long before it expires
const STATS_MS = 5_000;           // connection type check while a peer is open
const REBUILD_MS = [0, 2_000, 5_000, 15_000]; // host: wait before each new connection to a guest whose last one closed
const MAX_BUFFERED = { state: 64 * 1024, events: 1024 * 1024 }; // bytes queued on a channel before `send` skips
// Without `re` (an older server), these errors can only answer these requests, never a create, join, peek or resume.
const ERROR_REQUESTS = { peer_unavailable: ['kick'], not_in_room: ['lock', 'meta', 'kick', 'leave'], not_host: ['lock', 'meta', 'kick'] };

export class HandshakeError extends Error {
  /** @param {string} code a server error code, or 'network', 'timeout' or 'closed' @param {string} [message] */
  constructor(code, message = code) {
    super(message);
    this.name = 'HandshakeError';
    /** @type {string} */
    this.code = code;
  }
}

class Emitter {
  #handlers = new Map();
  /** @param {string} name @param {(...args: any[]) => void} fn @returns {this} */
  on(name, fn) {
    if (!this.#handlers.has(name)) this.#handlers.set(name, new Set());
    this.#handlers.get(name).add(fn);
    return this;
  }
  /** @param {string} name @param {(...args: any[]) => void} fn @returns {this} */
  off(name, fn) { this.#handlers.get(name)?.delete(fn); return this; }
  /** @internal @param {string} name @param {...any} args */
  emit(name, ...args) {
    for (const fn of [...(this.#handlers.get(name) ?? [])]) {
      try { fn(...args); } catch (e) { console.error('handshake listener', e); }
    }
  }
}

const parse = text => { try { return JSON.parse(text); } catch { return null; } };
const warned = new Set();
/** console.warn once per `kind`: a failure that repeats (every candidate, every heartbeat) is logged, never a flood. */
const warnOnce = (kind, ...args) => { if (warned.has(kind)) return; warned.add(kind); console.warn('handshake:', kind, ...args); };
/** True when at least one ICE server is a TURN relay (a turn: or turns: URL); STUN alone cannot carry a relay-only connection. */
const hasRelay = servers => (servers ?? []).some(s => [].concat(s?.urls ?? []).some(u => /^turns?:/i.test(String(u))));
const member = p => ({ id: p.id, name: p.name ?? '', nearby: !!p.nearby, away: !!p.away });

/**
 * A link to `href` that joins a room: `r` and `k` (when there is a key) set, the other parameters kept.
 * @param {string} href @param {string} code @param {string | null} [key] @returns {string}
 */
export function shareUrl(href, code, key) {
  const u = new URL(href);
  u.searchParams.set('r', code);
  if (key) u.searchParams.set('k', key); else u.searchParams.delete('k');
  return u.href;
}

/**
 * 'relayed' when either end of the selected candidate pair is a TURN relay, 'direct' otherwise,
 * null before a pair is selected.
 * @param {Map<string, any> | RTCStatsReport} stats
 * @returns {'direct' | 'relayed' | null}
 */
export function connectionType(stats) {
  let pair = null;
  stats.forEach(s => { if (s.type === 'transport' && s.selectedCandidatePairId) pair = stats.get(s.selectedCandidatePairId) ?? pair; });
  if (!pair) stats.forEach(s => { if (!pair && s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') pair = s; });
  if (!pair) return null;
  const local = stats.get(pair.localCandidateId), remote = stats.get(pair.remoteCandidateId);
  return local?.candidateType === 'relay' || remote?.candidateType === 'relay' ? 'relayed' : 'direct';
}

/**
 * A room seen from outside, as `peek` returns it.
 * @typedef {object} RoomInfo
 * @property {string} code the 5-character join code
 * @property {string} name the room's name
 * @property {number} players members now, host included
 * @property {number} maxPlayers room size, host included
 * @property {boolean} locked the host stopped new joins
 * @property {boolean} full no seat left
 */

/**
 * A public room in the lobby, as `listRooms` returns it.
 * @typedef {object} ListedRoom
 * @property {string} code the 5-character join code
 * @property {string} name the room's name
 * @property {number} players members now, host included
 * @property {number} maxPlayers room size, host included
 * @property {any} meta the host's metadata (map, mode...), or null
 * @property {boolean} nearby the host is on your network (same public IP)
 */

/**
 * A member of a room, host included.
 * @typedef {object} Member
 * @property {string} id peer id
 * @property {string} name display name
 * @property {boolean} nearby on the host's network (same public IP); always true for the host
 * @property {boolean} away the socket dropped; the seat is held for the grace period
 */

/**
 * The client: one per player. It fetches session tokens and TURN credentials, keeps the signaling socket open,
 * resumes after drops and sets up a WebRTC connection to each peer.
 *
 * Every promise rejects with a {@link HandshakeError}. Its `code` is a server error code or `network`, `timeout`,
 * `closed` or `no_turn`. Any call can fail with `network`, `timeout`, `closed`, `rate_limited`, `origin` (the page's
 * origin is not allowed for the app) or `not_found` (unknown app id). With `relayUnlessNearby`, `createRoom` and
 * `joinRoom` fail with `no_turn` when no TURN relay is to be had (the server offers none, or `/turn` failed twice).
 */
export class Handshake {
  #o; #token = null; #tokenUntil = 0; #turn = false; #ice = []; #iceUntil = 0;
  #ws = null; #welcomed = false; #connecting = null; #waiters = []; #queue = [];
  #room = null; #resume = null; #closed = false; #retry = 0; #retryTimer = null; #beat = null; #lock = null;

  /**
   * @param {object} o
   * @param {string} o.server e.g. 'https://handshake.four43.com'
   * @param {string} o.app the app id in the server's config
   * @param {number} o.version the game's network protocol version; rooms only match the same version
   * @param {string} [o.name] your display name, shown to the other players (the server cuts it to 32 characters)
   * @param {boolean} [o.relayUnlessNearby] connect only through TURN to a player who is not on the host's network
   * @param {any} [o.WebSocket] @param {any} [o.RTCPeerConnection] @param {any} [o.fetch] for tests
   * @param {boolean} [o.wakeLock] keep the screen on while in a room
   */
  constructor({ server, app, version, name, relayUnlessNearby = false, WebSocket = globalThis.WebSocket,
    RTCPeerConnection = globalThis.RTCPeerConnection, fetch = globalThis.fetch, wakeLock = true }) {
    this.#o = { server: server.replace(/\/+$/, ''), app, version, name, relayUnlessNearby, WebSocket, RTCPeerConnection,
      fetch: (...a) => fetch(...a), wakeLock }; // a bare `fetch` must not be called as a method (Illegal invocation)
    globalThis.addEventListener?.('online', this.#onOnline);
    globalThis.document?.addEventListener('visibilitychange', this.#onVisible);
  }

  /**
   * Look at a room without joining it, e.g. to show "Join this room?". Counts as a join attempt for rate limits.
   * @param {string} code the join code (case-insensitive) @param {string} [key] needed for a private room
   * @returns {Promise<RoomInfo>}
   * @throws {HandshakeError} `not_found`, `bad_key`, `version_mismatch`, `already_in_room`, `rate_limited`
   */
  async peek(code, key) {
    const m = await this.#request({ t: 'peek', code: String(code).trim().toUpperCase(), key }, 'room_info');
    return { code: m.code, name: m.name, players: m.players, maxPlayers: m.max_players, locked: m.locked, full: m.full };
  }

  /**
   * The app's open public rooms for your version: unlocked, not full, host present, nearby first, at most 50.
   * Empty when the app sets `list = "none"`.
   * @returns {Promise<ListedRoom[]>}
   */
  async listRooms() {
    const m = await this.#request({ t: 'list' }, 'rooms');
    return m.rooms.map(r => ({ code: r.code, name: r.name, players: r.players, maxPlayers: r.max_players, meta: r.meta ?? null, nearby: !!r.nearby }));
  }

  /**
   * Create a room and become its host.
   * @param {object} [o]
   * @param {boolean} [o.public] joinable with the code alone, and listed unless the app sets `list = "none"`;
   *   needs `public_rooms` on the app. Default false: guests need the code and the key.
   * @param {number} [o.maxPlayers] room size, host included, from 2 up to the app's `max_players` (the default)
   * @param {any} [o.meta] any JSON (at most 1 KB) the lobby shows, such as map or mode; the server never reads it
   * @param {string} [o.name] the room's name in listings (cut to 32 characters)
   * @returns {Promise<Room>}
   * @throws {HandshakeError} `already_in_room`, `public_disabled`, `too_many_rooms`, `meta_too_large`, `no_turn`
   */
  async createRoom({ public: pub = false, maxPlayers, meta, name } = {}) {
    if (this.#room) throw new HandshakeError('already_in_room');
    await this.#iceServers({ needed: this.#o.relayUnlessNearby });
    const m = await this.#request({ t: 'create', public: pub, name, player: this.#o.name, max_players: maxPlayers, meta }, 'joined');
    return this.#enter(m.room);
  }

  /**
   * Join a room by code.
   * @param {string} code the join code (case-insensitive) @param {string} [key] needed for a private room
   * @returns {Promise<Room>}
   * @throws {HandshakeError} `not_found`, `bad_key`, `version_mismatch`, `locked`, `full`, `already_in_room`,
   *   `rate_limited`, `no_turn`
   */
  async joinRoom(code, key) {
    if (this.#room) throw new HandshakeError('already_in_room');
    await this.#iceServers({ needed: this.#o.relayUnlessNearby });
    const m = await this.#request({ t: 'join', code: String(code).trim().toUpperCase(), key, player: this.#o.name }, 'joined');
    return this.#enter(m.room, key);
  }

  /**
   * Join the room a share link points to (`?r=<code>&k=<key>`, see `Room.shareUrl`). Resolves null when the URL
   * has no room code, so a game can call it on every page load.
   * @param {string} [url] defaults to the page's URL
   * @returns {Promise<Room | null>}
   * @throws {HandshakeError} as `joinRoom`
   */
  async joinFromUrl(url = globalThis.location?.href) {
    const params = url ? new URL(url).searchParams : null;
    const code = params?.get('r');
    return code ? this.joinRoom(code, params.get('k') ?? undefined) : null;
  }

  /** Leave any room, close the socket and stop. The object cannot be used again. */
  close() {
    if (this.#closed) return;
    if (this.#room) { this.#send({ t: 'leave' }); this.#exit('left'); }
    this.#closed = true;
    clearInterval(this.#beat);
    const ws = this.#ws;
    this.#ws = null;
    if (ws) { ws.onclose = null; try { ws.close(); } catch { /* already closed */ } }
    for (const w of this.#waiters.splice(0)) { clearTimeout(w.timer); w.reject(new HandshakeError('closed')); }
    globalThis.removeEventListener?.('online', this.#onOnline);
    globalThis.document?.removeEventListener('visibilitychange', this.#onVisible);
  }

  // ---- HTTP -------------------------------------------------------------

  async #post(path, body, token) {
    let res;
    try {
      res = await this.#o.fetch(this.#o.server + path, {
        method: 'POST',
        headers: { 'content-type': 'application/json', ...(token ? { authorization: `Bearer ${token}` } : {}) },
        body: body ? JSON.stringify(body) : undefined,
      });
    } catch (e) { throw new HandshakeError('network', String(e?.message ?? e)); }
    const data = await res.json().catch(e => { warnOnce(`${path} reply is not JSON`, e); return {}; });
    if (!res.ok) throw new HandshakeError(data.error ?? 'network', `HTTP ${res.status}`);
    return data;
  }

  async #session() {
    if (this.#token && Date.now() < this.#tokenUntil - REFRESH_EARLY_MS) return this.#token;
    const s = await this.#post('/session', { app: this.#o.app, version: this.#o.version });
    this.#token = s.token;
    this.#tokenUntil = Date.now() + s.expires_in * 1000;
    this.#turn = !!s.turn;
    return this.#token;
  }

  /**
   * TURN credentials, renewed before they expire (and pushed into open peer connections). A failed fetch is tried
   * once more. When `needed` (relay-only connections) and there are still none, it throws `no_turn`; otherwise
   * the client goes on with what it has (direct connections only, or the old credentials until they expire).
   */
  async #iceServers({ needed = false } = {}) {
    await this.#session();
    if (!this.#turn) {
      if (needed) throw new HandshakeError('no_turn', 'the server offers no TURN relay for this app');
      return this.#ice;
    }
    if (Date.now() < this.#iceUntil - REFRESH_EARLY_MS) return this.#ice;
    let error = null;
    for (let i = 0; i < 2; i++) {
      try {
        const t = await this.#post('/turn', null, this.#token);
        if (needed && !hasRelay(t.ice_servers)) throw new HandshakeError('no_turn', 'the server\'s ICE servers have no turn: URL (only STUN), so a relay-only connection can never be made');
        this.#ice = t.ice_servers;
        this.#iceUntil = Date.now() + t.ttl * 1000;
        for (const peer of this.#room?.peers.values() ?? []) peer._setIce(this.#ice);
        return this.#ice;
      } catch (e) { if (e?.code === 'no_turn') throw e; error = e; }
    }
    const current = Date.now() < this.#iceUntil;
    if (needed && !current) throw new HandshakeError('no_turn', `no TURN credentials: ${error.message}`);
    warnOnce('no TURN credentials', error);
    return current ? this.#ice : [];
  }

  // ---- socket -------------------------------------------------------------

  #connect() {
    if (this.#ws && this.#welcomed && this.#ws.readyState === 1) return Promise.resolve();
    return (this.#connecting ??= this.#open().finally(() => { this.#connecting = null; }));
  }

  async #open() {
    const token = await this.#session();
    if (this.#closed) throw new HandshakeError('closed');
    const ws = new this.#o.WebSocket(this.#o.server.replace(/^http/, 'ws') + '/ws');
    this.#ws = ws;
    this.#welcomed = false;
    await new Promise((resolve, reject) => {
      const fail = err => {
        clearTimeout(timer);
        ws.onopen = ws.onmessage = ws.onclose = null;
        try { ws.close(); } catch { /* already closed */ }
        if (this.#ws === ws) this.#ws = null;
        if (err.code === 'bad_token') this.#token = null;
        reject(err);
      };
      const timer = setTimeout(() => fail(new HandshakeError('timeout', 'no welcome from the server')), HELLO_MS);
      ws.onopen = () => ws.send(JSON.stringify({ t: 'hello', v: SIGNAL_VERSION, token }));
      ws.onmessage = e => {
        const m = parse(e.data);
        if (m?.t === 'error') return fail(new HandshakeError(m.code, m.message));
        if (m?.t !== 'welcome') return;
        clearTimeout(timer);
        this.#welcomed = true;
        ws.onmessage = ev => this.#onMessage(ws, ev);
        ws.onclose = () => this.#onClose(ws);
        resolve();
      };
      ws.onclose = () => fail(new HandshakeError('network', 'socket closed before welcome'));
    });
    this.#startHeartbeat();
  }

  #send(msg) {
    if (!this.#ws || !this.#welcomed || this.#ws.readyState !== 1) return false;
    this.#ws.send(JSON.stringify(msg));
    return true;
  }

  /** Signals are queued while the socket is down and sent after the resume. */
  #signal(to, data) {
    const msg = { t: 'signal', to, data };
    if (!this.#send(msg)) this.#queue.push(msg);
  }

  /**
   * Send `msg` and wait for a reply of type `want` (for which `match` is true). With `errors`, the first `error` that
   * arrives rejects it. A `pass` reply is also handled as usual. Without `connect`, a socket that is down rejects
   * at once. `room` waiters fail when the room ends.
   */
  async #request(msg, want, { errors = true, match = null, pass = false, connect = true, room = false } = {}) {
    if (this.#closed) throw new HandshakeError('closed');
    if (connect) await this.#connect();
    return new Promise((resolve, reject) => {
      const w = { re: msg.t, want, errors, match, pass, room, resolve, reject };
      w.timer = setTimeout(() => { this.#drop(w); reject(new HandshakeError('timeout', `no ${want} reply`)); }, REQUEST_MS);
      this.#waiters.push(w);
      if (!this.#send(msg)) { this.#drop(w); reject(new HandshakeError('network', 'socket is not open')); }
    });
  }

  #drop(w) {
    const i = this.#waiters.indexOf(w);
    if (i >= 0) this.#waiters.splice(i, 1);
    clearTimeout(w.timer);
  }

  #onMessage(ws, e) {
    if (ws !== this.#ws) return;
    const m = parse(e.data);
    if (!m) return;
    if (m.t === 'error') {
      if (m.code === 'replaced') return this.#exit('replaced');
      // `re` names the request this error answers; an older server sends none (see ERROR_REQUESTS)
      const w = this.#waiters.find(x => x.errors && (m.re ? x.re === m.re : !ERROR_REQUESTS[m.code] || ERROR_REQUESTS[m.code].includes(x.re)));
      if (w) { this.#drop(w); w.reject(new HandshakeError(m.code, m.message)); }
      else warnOnce(`server error ${m.code}${m.re ? ` (${m.re})` : ''}`, m.message); // e.g. ICE candidates to a peer who is away
      return;
    }
    const w = this.#waiters.find(x => x.want === m.t && (!x.match || x.match(m)));
    if (w) {
      this.#drop(w);
      w.resolve(m);
      if (!w.pass) return;
    }
    if (m.t === 'room_closed') return this.#exit(m.reason);
    if (m.t === 'kicked') return this.#exit('kicked');
    this.#room?._handle(m);
  }

  #onClose(ws) {
    if (ws !== this.#ws) return;
    this.#ws = null;
    this.#welcomed = false;
    clearInterval(this.#beat);
    for (const w of this.#waiters.splice(0)) { clearTimeout(w.timer); w.reject(new HandshakeError('network', 'socket closed')); }
    if (this.#room && !this.#closed) this.#scheduleResume();
  }

  #startHeartbeat() {
    clearInterval(this.#beat);
    this.#beat = setInterval(() => this.#beatOnce(), HEARTBEAT_MS);
  }

  #beatOnce() {
    const ws = this.#ws;
    if (!ws) return;
    this.#request({ t: 'list' }, 'rooms', { errors: false }).catch(e => {
      if (ws !== this.#ws) return;
      warnOnce('heartbeat failed, resuming', e);
      try { ws.close(); } catch { /* already closing */ }
      this.#onClose(ws); // do not wait for a close handshake on a dead socket
    });
    if (this.#room) this.#iceServers().catch(e => warnOnce('TURN refresh failed', e));
  }

  // ---- room lifecycle -----------------------------------------------------

  #enter(r, key) {
    this.#resume = r.resume;
    const link = {
      send: msg => this.#send(msg),
      control: (msg, want, match) => this.#request(msg, want, { match, pass: true, connect: false, room: true }),
      signal: (to, data) => this.#signal(to, data),
      leave: () => { this.#send({ t: 'leave' }); this.#exit('left'); },
      ice: () => this.#ice,
      RTCPeerConnection: this.#o.RTCPeerConnection,
      relayUnlessNearby: this.#o.relayUnlessNearby,
    };
    this.#room = new Room(link, r, key);
    this.#wake(true);
    return this.#room;
  }

  #exit(reason) {
    const room = this.#room;
    if (!room) return;
    this.#room = null;
    this.#resume = null;
    this.#queue.length = 0;
    this.#retry = 0;
    clearTimeout(this.#retryTimer);
    this.#retryTimer = null;
    this.#wake(false);
    for (const w of this.#waiters.filter(x => x.room)) { this.#drop(w); w.reject(new HandshakeError('closed')); }
    room._close(reason);
  }

  #scheduleResume(now = false) {
    if (this.#retryTimer !== null) {
      if (!now) return;
      clearTimeout(this.#retryTimer);
    }
    const delay = now ? 0 : RESUME_BACKOFF_MS[Math.min(this.#retry, RESUME_BACKOFF_MS.length - 1)];
    this.#retryTimer = setTimeout(() => { this.#retryTimer = null; this.#resumeRoom(); }, delay);
  }

  async #resumeRoom() {
    if (!this.#room || this.#closed) return;
    this.#retry++;
    try {
      const m = await this.#request({ t: 'resume', token: this.#resume }, 'joined');
      if (!this.#room) return;
      this.#retry = 0;
      this.#resume = m.room.resume;
      this.#room._resumed(m.room);
      for (const msg of this.#queue.splice(0)) if (!this.#send(msg)) this.#queue.push(msg);
      this.#wake(true);
    } catch (e) {
      if (!this.#room || this.#closed) return;
      if (e.code === 'not_found') return this.#exit('lost');
      const ws = this.#ws;
      if (ws) { try { ws.close(); } catch { /* already closing */ } this.#onClose(ws); }
      else this.#scheduleResume();
    }
  }

  #onVisible = () => {
    if (globalThis.document?.visibilityState !== 'visible' || this.#closed || !this.#room) return;
    this.#wake(true);
    if (this.#ws) this.#beatOnce(); else this.#scheduleResume(true);
  };

  #onOnline = () => {
    if (this.#closed || !this.#room) return;
    if (this.#ws) this.#beatOnce(); else this.#scheduleResume(true);
    for (const peer of this.#room.peers.values()) peer._restart();
  };

  async #wake(on) {
    if (!this.#o.wakeLock) return;
    try {
      if (on && !this.#lock && globalThis.navigator?.wakeLock && globalThis.document?.visibilityState === 'visible') {
        this.#lock = await navigator.wakeLock.request('screen');
        this.#lock.addEventListener?.('release', () => { this.#lock = null; });
      } else if (!on && this.#lock) {
        const lock = this.#lock;
        this.#lock = null;
        await lock.release();
      }
    } catch { this.#lock = null; } // not allowed (not visible, low power): no wake lock
  }
}

/**
 * One room: its members (from the server) and its peer connections (host <-> each guest).
 * Returned by `createRoom`, `joinRoom` and `joinFromUrl`.
 *
 * Events (`room.on(name, fn)`):
 * - `peer` (peer: Peer): a peer connection opened; the host gets one per guest, a guest one for the host
 * - `members` (members): the member list changed
 * - `peerAway` (id) / `peerBack` (id): a member's socket dropped / resumed
 * - `peerLeft` (id, reason): a member left; reason is `left`, `timeout` or `kicked`
 * - `hostAway` (graceSecs) / `hostBack` (): the host's socket dropped / resumed
 * - `meta` ({ meta, locked }): the host changed the room's meta or lock
 * - `closed` (reason): you are out of the room: `left`, `host_left`, `host_gone`, `kicked`, `expired`, `idle`,
 *   `replaced` or `lost`
 * @hideconstructor
 */
class Room extends Emitter {
  #link; #rebuilds = new Map(); // host: guest id -> { n: new connections since one last opened, timer }

  constructor(link, r, key) {
    super();
    this.#link = link;
    /** @type {string} the 5-character join code */ this.code = r.code;
    /** @type {string} the room's name */ this.name = r.name;
    /** @type {string | null} the private key: from the server for the host, the one you joined with for a guest */
    this.key = r.key ?? key ?? null;
    /** @type {boolean} you created this room */ this.isHost = !!r.is_host;
    /** @type {string} your peer id */ this.you = r.you;
    /** @type {string} the host's peer id */ this.hostId = r.host;
    /** @type {boolean} new joins are refused (see `lock`) */ this.locked = !!r.locked;
    /** @type {any} the host's metadata (see `setMeta`), or null */ this.meta = r.meta ?? null;
    /** @type {Member[]} everyone in the room, host included, in join order (updated by `members` events) */
    this.members = r.peers.map(member);
    /** @type {Map<string, Peer>} open or opening connections by peer id: each guest for the host, the host for a guest */
    this.peers = new Map();
    /** @type {boolean} true after `closed` */ this.closed = false;
    if (!this.isHost) this.#addPeer(this.hostId, false); // the host sends the offer
  }

  /** A link that joins this room (see `joinFromUrl`); null outside a browser page. @type {string | null} */
  get shareUrl() {
    const href = globalThis.location?.href;
    return href ? shareUrl(href, this.code, this.key) : null;
  }

  // Host controls resolve once the server has applied them and reject with a HandshakeError: `not_host`, a server
  // error, `network` while the socket is down (nothing is queued), `timeout`, or `closed` if the room ends first.

  /** Host only: stop (or allow) new joins. @param {boolean} locked @returns {Promise<void>} */
  lock(locked) { return this.#control({ t: 'lock', locked: !!locked }, 'room_meta', m => !!m.locked === !!locked); }
  /** Host only: replace the room's meta. @param {any} meta at most 1 KB of JSON @returns {Promise<void>} */
  setMeta(meta) { return this.#control({ t: 'meta', meta }, 'room_meta'); }
  /** Host only: remove a peer; it can rejoin unless the room is locked. @param {string} peerId @returns {Promise<void>} */
  kick(peerId) { return this.#control({ t: 'kick', peer: peerId }, 'peer_left', m => m.peer === peerId); }
  /** Leave the room (the host leaving closes it for everyone). The client stays connected for another room. */
  leave() { this.#link.leave(); }

  async #control(msg, want, match) {
    if (!this.isHost) throw new HandshakeError('not_host');
    await this.#link.control(msg, want, match);
  }

  /** @internal a server message for this room */
  _handle(m) {
    switch (m.t) {
      case 'peer_joined':
        this.members.push({ id: m.peer, name: m.name ?? '', nearby: !!m.nearby, away: false });
        this.emit('members', this.members);
        if (this.isHost) this.#addPeer(m.peer, true);
        break;
      case 'peer_away': this.#setAway(m.peer, true); this.emit('peerAway', m.peer); this.emit('members', this.members); break;
      case 'peer_back':
        this.#setAway(m.peer, false);
        if (this.isHost && !this.peers.has(m.peer)) this.#rebuild(m.peer); // its connection closed while it was away
        this.emit('peerBack', m.peer);
        this.emit('members', this.members);
        break;
      case 'peer_left':
        this.members = this.members.filter(x => x.id !== m.peer);
        clearTimeout(this.#rebuilds.get(m.peer)?.timer);
        this.#rebuilds.delete(m.peer);
        this.peers.get(m.peer)?._close();
        this.emit('peerLeft', m.peer, m.reason);
        this.emit('members', this.members);
        break;
      case 'host_away': this.#setAway(this.hostId, true); this.emit('hostAway', m.grace_secs); this.emit('members', this.members); break;
      case 'host_back':
        this.#setAway(this.hostId, false);
        if (!this.isHost && !this.peers.has(this.hostId)) this.#lost(this.hostId); // an ask while the host was away found nobody
        this.emit('hostBack');
        this.emit('members', this.members);
        break;
      case 'room_meta':
        this.locked = !!m.locked;
        this.meta = m.meta ?? null;
        this.emit('meta', { meta: this.meta, locked: this.locked });
        break;
      case 'signal': this.#onSignal(m.from, m.data); break;
    }
  }

  /** @internal the `joined` view after a resume: catch up on anything missed while the socket was down */
  _resumed(r) {
    const now = new Set(r.peers.map(p => p.id));
    for (const x of this.members) {
      if (now.has(x.id)) continue;
      this.peers.get(x.id)?._close();
      this.emit('peerLeft', x.id, 'left');
    }
    this.members = r.peers.map(member);
    this.name = r.name;
    for (const x of this.members) { const peer = this.peers.get(x.id); if (peer) peer.name = x.name; }
    this.locked = !!r.locked;
    this.meta = r.meta ?? null;
    if (this.isHost) for (const x of this.members) if (x.id !== this.you && !this.peers.has(x.id)) this.#addPeer(x.id, true);
    this.emit('members', this.members);
    this.emit('meta', { meta: this.meta, locked: this.locked });
  }

  /** @internal */
  _close(reason) {
    if (this.closed) return;
    this.closed = true;
    for (const r of this.#rebuilds.values()) clearTimeout(r.timer);
    for (const peer of [...this.peers.values()]) peer._close();
    this.emit('closed', reason);
  }

  #setAway(id, away) { const x = this.members.find(y => y.id === id); if (x) x.away = away; }

  #addPeer(id, initiator) {
    // Relay policy uses the guest's flag: the host's view of the guest, or the guest's own entry.
    const nearby = !!this.members.find(x => x.id === (initiator ? id : this.you))?.nearby;
    const peer = new Peer(this.#link, id, this.members.find(x => x.id === id)?.name ?? '', nearby, initiator,
      () => { this.#rebuilds.delete(id); this.emit('peer', peer); },
      failed => {
        if (this.peers.get(id) !== peer) return;
        this.peers.delete(id);
        if (failed) this.#lost(id);
      });
    this.peers.set(id, peer);
    return peer;
  }

  /** A connection closed by itself (ICE failed, a channel closed): the host makes a new one, a guest asks it to. */
  #lost(id) {
    if (this.closed) return;
    if (this.isHost) this.#rebuild(id);
    else this.#link.signal(this.hostId, { restart: true, rebuild: true }); // queued while the socket is down
  }

  /**
   * Host: a new connection to guest `id`, once it is here (not away, or `here`: it just signaled) and has none;
   * later tries wait longer.
   */
  #rebuild(id, here = false) {
    const r = this.#rebuilds.get(id) ?? { n: 0, timer: null };
    this.#rebuilds.set(id, r);
    if (r.timer !== null) return;
    const go = () => {
      r.timer = null;
      if (this.closed || this.peers.has(id) || !this.members.find(x => x.id === id && (here || !x.away))) return;
      r.n++;
      this.#addPeer(id, true);
    };
    const delay = REBUILD_MS[Math.min(r.n, REBUILD_MS.length - 1)];
    if (delay) r.timer = setTimeout(go, delay); else go();
  }

  #onSignal(from, data) {
    let peer = this.peers.get(from);
    if (this.isHost) {
      if (data?.restart && this.members.some(x => x.id === from)) {
        // No connection here, or the guest lost its side: a new connection. A new one not yet answered is on its way.
        if (!peer) return this.#rebuild(from, true);
        if (data.rebuild) {
          if (!peer._started) return;
          peer._close();
          return this.#rebuild(from, true);
        }
      }
    } else if (from === this.hostId) {
      if (peer && data?.new && data.sdp && peer._started) { peer._close(); peer = null; } // the host made a new connection
      if (!peer) peer = this.#addPeer(from, false);
    }
    peer?._signal(data);
  }
}

/**
 * One WebRTC connection with its `state` and `events` channels. Delivered by the room's `peer` event.
 *
 * Events (`peer.on(name, fn)`):
 * - `message` (data, { reliable }): an ArrayBuffer, or a parsed JSON value from the reliable channel
 * - `type` ('direct' | 'relayed'): the connection type changed
 * - `close` (): the connection closed
 * @hideconstructor
 */
class Peer extends Emitter {
  #link; #pc; #initiator; #opened; #gone; #state = null; #events = null; #pending = []; #chain = Promise.resolve(); #stats = null;

  constructor(link, id, name, nearby, initiator, opened, gone) {
    super();
    this.#link = link;
    this.#initiator = initiator;
    this.#opened = opened;
    this.#gone = gone;
    /** @type {string} the other side's peer id */ this.id = id;
    /** @type {string} the player's display name */ this.name = name;
    /** @type {boolean} the guest is on the host's network (same public IP) */ this.nearby = nearby;
    /** @type {'direct' | 'relayed' | null} through a TURN relay or not; null until known (see the `type` event) */
    this.connectionType = null;
    /** @type {boolean} both data channels are open, so `send` works */ this.open = false;
    const config = { iceServers: link.ice() };
    if (link.relayUnlessNearby && !nearby) config.iceTransportPolicy = 'relay';
    const pc = (this.#pc = new link.RTCPeerConnection(config));
    pc.onicecandidate = e => { if (e.candidate) link.signal(id, { candidate: e.candidate }); };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === 'connected') this.#checkType();
      if (pc.connectionState === 'failed') this._restart();
    };
    if (initiator) {
      this.#wire(pc.createDataChannel('state', { ordered: false, maxRetransmits: 0 }));
      this.#wire(pc.createDataChannel('events', { ordered: true }));
      this.#offer(false);
    } else {
      pc.ondatachannel = e => this.#wire(e.channel);
    }
  }

  /**
   * Binary (ArrayBuffer or typed array) goes on either channel; a plain object only on the reliable one, as JSON.
   * @param {ArrayBuffer | ArrayBufferView | object} data
   * @param {{ reliable?: boolean }} [o]
   * @returns {boolean} false when the channel is not open, or holds more than 64 KB (`state`) or 1 MB (`events`)
   *   not yet sent: nothing was sent
   */
  send(data, { reliable = false } = {}) {
    const ch = reliable ? this.#events : this.#state;
    const binary = data instanceof ArrayBuffer || ArrayBuffer.isView(data);
    if (!binary && !reliable) throw new TypeError('handshake: only binary data can go on the unreliable channel');
    if (!this.open || ch?.readyState !== 'open') return false;
    if (ch.bufferedAmount > MAX_BUFFERED[ch.label]) { warnOnce(`${ch.label} channel is full: sends skipped`); return false; }
    ch.send(binary ? data : JSON.stringify(data));
    return true;
  }

  /** @internal signaling data from the other side, applied in order */
  _signal(data) {
    this.#chain = this.#chain.then(() => this.#apply(data)).catch(e => console.warn('handshake signal', e));
  }

  /** @internal ICE restart: the host re-offers; a guest asks the host to */
  _restart() {
    if (this.#gone === null) return;
    if (this.#initiator) this.#offer(true);
    else this.#link.signal(this.id, { restart: true });
  }

  /** @internal new TURN credentials */
  _setIce(ice) {
    try { this.#pc.setConfiguration({ ...this.#pc.getConfiguration(), iceServers: ice }); }
    catch (e) { console.warn('handshake setConfiguration', e); }
  }

  /** @internal the other side's description has arrived (the guest's answer, or the host's offer) */
  get _started() { return !!this.#pc.remoteDescription; }

  /** @internal `failed`: it closed by itself (not left, kicked or replaced), so the room makes a new one */
  _close(failed = false) {
    if (this.#gone === null) return;
    const gone = this.#gone;
    this.#gone = null;
    this.open = false;
    clearInterval(this.#stats);
    try { this.#pc.close(); } catch { /* already closed */ }
    gone(failed);
    this.emit('close');
  }

  /** The first offer of a connection is marked `new`, so a guest that still has an old one replaces it. */
  async #offer(iceRestart) {
    try {
      const pc = this.#pc;
      await pc.setLocalDescription(await pc.createOffer(iceRestart ? { iceRestart: true } : undefined));
      this.#link.signal(this.id, iceRestart ? { sdp: pc.localDescription } : { sdp: pc.localDescription, new: true });
    } catch (e) { console.warn('handshake offer', e); }
  }

  async #apply(data) {
    const pc = this.#pc;
    if (this.#gone === null || !data) return;
    if (data.sdp) {
      await pc.setRemoteDescription(data.sdp);
      for (const c of this.#pending.splice(0)) await pc.addIceCandidate(c).catch(e => warnOnce('ICE candidate rejected', e));
      if (data.sdp.type === 'offer') {
        await pc.setLocalDescription(await pc.createAnswer());
        this.#link.signal(this.id, { sdp: pc.localDescription });
      }
    } else if (data.candidate) {
      if (pc.remoteDescription) await pc.addIceCandidate(data.candidate).catch(e => warnOnce('ICE candidate rejected', e));
      else this.#pending.push(data.candidate); // before the offer/answer: hold it
    } else if (data.restart && this.#initiator) {
      await this.#offer(true);
    }
  }

  #wire(ch) {
    const reliable = ch.label === 'events';
    if (ch.label === 'state') this.#state = ch;
    else if (reliable) this.#events = ch;
    else return;
    ch.binaryType = 'arraybuffer';
    ch.onopen = () => this.#checkOpen();
    ch.onclose = () => this._close(true); // also before both opened: that connection failed
    ch.onmessage = e => {
      let data = e.data;
      if (typeof data === 'string') {
        if (!reliable) return; // text never belongs on the state channel
        data = parse(data);
        if (data === null) return;
      }
      this.emit('message', data, { reliable });
    };
    if (ch.readyState === 'open') this.#checkOpen();
  }

  #checkOpen() {
    if (this.open || this.#gone === null || this.#state?.readyState !== 'open' || this.#events?.readyState !== 'open') return;
    this.open = true;
    this.#opened();
    this.#checkType();
    this.#stats = setInterval(() => this.#checkType(), STATS_MS);
  }

  async #checkType() {
    let type;
    try { type = connectionType(await this.#pc.getStats()); } catch { return; }
    if (type && type !== this.connectionType) { this.connectionType = type; this.emit('type', type); }
  }
}
