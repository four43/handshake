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
  /** @param {string} name @param {...any} args */
  emit(name, ...args) {
    for (const fn of [...(this.#handlers.get(name) ?? [])]) {
      try { fn(...args); } catch (e) { console.error('handshake listener', e); }
    }
  }
}

const parse = text => { try { return JSON.parse(text); } catch { return null; } };
const member = p => ({ id: p.id, nearby: !!p.nearby, away: !!p.away });

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

export class Handshake {
  #o; #token = null; #tokenUntil = 0; #turn = false; #ice = []; #iceUntil = 0;
  #ws = null; #welcomed = false; #connecting = null; #waiters = []; #queue = [];
  #room = null; #resume = null; #closed = false; #retry = 0; #retryTimer = null; #beat = null; #lock = null;

  /**
   * @param {object} o
   * @param {string} o.server e.g. 'https://handshake.four43.com'
   * @param {string} o.app the app id in the server's config
   * @param {number} o.version the game's network protocol version; rooms only match the same version
   * @param {boolean} [o.relayUnlessNearby] connect only through TURN to a player who is not on the host's network
   * @param {any} [o.WebSocket] @param {any} [o.RTCPeerConnection] @param {any} [o.fetch] for tests
   * @param {boolean} [o.wakeLock] keep the screen on while in a room
   */
  constructor({ server, app, version, relayUnlessNearby = false, WebSocket = globalThis.WebSocket,
    RTCPeerConnection = globalThis.RTCPeerConnection, fetch = globalThis.fetch, wakeLock = true }) {
    this.#o = { server: server.replace(/\/+$/, ''), app, version, relayUnlessNearby, WebSocket, RTCPeerConnection,
      fetch: (...a) => fetch(...a), wakeLock }; // a bare `fetch` must not be called as a method (Illegal invocation)
    globalThis.addEventListener?.('online', this.#onOnline);
    globalThis.document?.addEventListener('visibilitychange', this.#onVisible);
  }

  /**
   * Look at a room without joining it.
   * @param {string} code @param {string} [key] needed for a private room
   * @returns {Promise<{ code: string, name: string, players: number, maxPlayers: number, locked: boolean, full: boolean }>}
   */
  async peek(code, key) {
    const m = await this.#request({ t: 'peek', code: String(code).trim().toUpperCase(), key }, 'room_info');
    return { code: m.code, name: m.name, players: m.players, maxPlayers: m.max_players, locked: m.locked, full: m.full };
  }

  /** @param {{ public?: boolean, maxPlayers?: number, meta?: any }} [o] @returns {Promise<Room>} */
  async createRoom({ public: pub = false, maxPlayers, meta } = {}) {
    if (this.#room) throw new HandshakeError('already_in_room');
    await this.#iceServers();
    const m = await this.#request({ t: 'create', public: pub, max_players: maxPlayers, meta }, 'joined');
    return this.#enter(m.room);
  }

  /** @param {string} code @param {string} [key] needed for a private room @returns {Promise<Room>} */
  async joinRoom(code, key) {
    if (this.#room) throw new HandshakeError('already_in_room');
    await this.#iceServers();
    const m = await this.#request({ t: 'join', code: String(code).trim().toUpperCase(), key }, 'joined');
    return this.#enter(m.room);
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
    const data = await res.json().catch(() => ({}));
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

  /** TURN credentials, renewed before they expire (and pushed into open peer connections). */
  async #iceServers() {
    await this.#session();
    if (!this.#turn || Date.now() < this.#iceUntil - REFRESH_EARLY_MS) return this.#ice;
    try {
      const t = await this.#post('/turn', null, this.#token);
      this.#ice = t.ice_servers;
      this.#iceUntil = Date.now() + t.ttl * 1000;
      for (const peer of this.#room?.peers.values() ?? []) peer._setIce(this.#ice);
    } catch (e) { console.warn('handshake: no TURN credentials', e); }
    return this.#ice;
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

  /** Send `msg` and wait for a reply of type `want`. With `errors`, the first `error` that arrives rejects it. */
  async #request(msg, want, errors = true) {
    if (this.#closed) throw new HandshakeError('closed');
    await this.#connect();
    return new Promise((resolve, reject) => {
      const w = { want, errors, resolve, reject };
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
      const w = this.#waiters.find(x => x.errors);
      if (w) { this.#drop(w); w.reject(new HandshakeError(m.code, m.message)); }
      else console.warn('handshake:', m.code, m.message);
      return;
    }
    const w = this.#waiters.find(x => x.want === m.t);
    if (w) { this.#drop(w); return w.resolve(m); }
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
    this.#request({ t: 'list' }, 'rooms', false).catch(() => {
      if (ws !== this.#ws) return;
      try { ws.close(); } catch { /* already closing */ }
      this.#onClose(ws); // do not wait for a close handshake on a dead socket
    });
    if (this.#room) this.#iceServers().catch(() => {});
  }

  // ---- room lifecycle -----------------------------------------------------

  #enter(r) {
    this.#resume = r.resume;
    const link = {
      send: msg => this.#send(msg),
      signal: (to, data) => this.#signal(to, data),
      leave: () => { this.#send({ t: 'leave' }); this.#exit('left'); },
      ice: () => this.#ice,
      RTCPeerConnection: this.#o.RTCPeerConnection,
      relayUnlessNearby: this.#o.relayUnlessNearby,
    };
    this.#room = new Room(link, r);
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

/** One room: its members (from the server) and its peer connections (host <-> each guest). */
class Room extends Emitter {
  #link;

  constructor(link, r) {
    super();
    this.#link = link;
    /** @type {string} */ this.code = r.code;
    /** @type {string | null} */ this.key = r.key ?? null;
    /** @type {boolean} */ this.isHost = !!r.is_host;
    /** @type {string} */ this.you = r.you;
    /** @type {string} */ this.hostId = r.host;
    /** @type {boolean} */ this.locked = !!r.locked;
    /** @type {any} */ this.meta = r.meta ?? null;
    /** @type {{ id: string, nearby: boolean, away: boolean }[]} */ this.members = r.peers.map(member);
    /** @type {Map<string, Peer>} */ this.peers = new Map();
    this.closed = false;
    if (!this.isHost) this.#addPeer(this.hostId, false); // the host sends the offer
  }

  /** @param {boolean} locked */ lock(locked) { this.#link.send({ t: 'lock', locked: !!locked }); }
  /** @param {any} meta at most 1 KB of JSON */ setMeta(meta) { this.#link.send({ t: 'meta', meta }); }
  /** @param {string} peerId */ kick(peerId) { this.#link.send({ t: 'kick', peer: peerId }); }
  leave() { this.#link.leave(); }

  /** @internal a server message for this room */
  _handle(m) {
    switch (m.t) {
      case 'peer_joined':
        this.members.push({ id: m.peer, nearby: !!m.nearby, away: false });
        this.emit('members', this.members);
        if (this.isHost) this.#addPeer(m.peer, true);
        break;
      case 'peer_away': this.#setAway(m.peer, true); this.emit('peerAway', m.peer); this.emit('members', this.members); break;
      case 'peer_back': this.#setAway(m.peer, false); this.emit('peerBack', m.peer); this.emit('members', this.members); break;
      case 'peer_left':
        this.members = this.members.filter(x => x.id !== m.peer);
        this.peers.get(m.peer)?._close();
        this.emit('peerLeft', m.peer, m.reason);
        this.emit('members', this.members);
        break;
      case 'host_away': this.#setAway(this.hostId, true); this.emit('hostAway', m.grace_secs); this.emit('members', this.members); break;
      case 'host_back': this.#setAway(this.hostId, false); this.emit('hostBack'); this.emit('members', this.members); break;
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
    for (const peer of [...this.peers.values()]) peer._close();
    this.emit('closed', reason);
  }

  #setAway(id, away) { const x = this.members.find(y => y.id === id); if (x) x.away = away; }

  #addPeer(id, initiator) {
    // Relay policy uses the guest's flag: the host's view of the guest, or the guest's own entry.
    const nearby = !!this.members.find(x => x.id === (initiator ? id : this.you))?.nearby;
    const peer = new Peer(this.#link, id, nearby, initiator,
      () => this.emit('peer', peer),
      () => { if (this.peers.get(id) === peer) this.peers.delete(id); });
    this.peers.set(id, peer);
    return peer;
  }

  #onSignal(from, data) {
    let peer = this.peers.get(from);
    if (!peer && !this.isHost && from === this.hostId) peer = this.#addPeer(from, false);
    peer?._signal(data);
  }
}

/** One WebRTC connection with its `state` and `events` channels. */
class Peer extends Emitter {
  #link; #pc; #initiator; #opened; #gone; #state = null; #events = null; #pending = []; #chain = Promise.resolve(); #stats = null;

  constructor(link, id, nearby, initiator, opened, gone) {
    super();
    this.#link = link;
    this.#initiator = initiator;
    this.#opened = opened;
    this.#gone = gone;
    /** @type {string} */ this.id = id;
    /** @type {boolean} */ this.nearby = nearby;
    /** @type {'direct' | 'relayed' | null} */ this.connectionType = null;
    /** @type {boolean} */ this.open = false;
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
   * @returns {boolean} false when the channel is not open (nothing was sent)
   */
  send(data, { reliable = false } = {}) {
    const ch = reliable ? this.#events : this.#state;
    const binary = data instanceof ArrayBuffer || ArrayBuffer.isView(data);
    if (!binary && !reliable) throw new TypeError('handshake: only binary data can go on the unreliable channel');
    if (!this.open || ch?.readyState !== 'open') return false;
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

  /** @internal */
  _close() {
    if (this.#gone === null) return;
    const gone = this.#gone;
    this.#gone = null;
    this.open = false;
    clearInterval(this.#stats);
    try { this.#pc.close(); } catch { /* already closed */ }
    gone();
    this.emit('close');
  }

  async #offer(iceRestart) {
    try {
      const pc = this.#pc;
      await pc.setLocalDescription(await pc.createOffer(iceRestart ? { iceRestart: true } : undefined));
      this.#link.signal(this.id, { sdp: pc.localDescription });
    } catch (e) { console.warn('handshake offer', e); }
  }

  async #apply(data) {
    const pc = this.#pc;
    if (this.#gone === null || !data) return;
    if (data.sdp) {
      await pc.setRemoteDescription(data.sdp);
      for (const c of this.#pending.splice(0)) await pc.addIceCandidate(c).catch(() => {});
      if (data.sdp.type === 'offer') {
        await pc.setLocalDescription(await pc.createAnswer());
        this.#link.signal(this.id, { sdp: pc.localDescription });
      }
    } else if (data.candidate) {
      if (pc.remoteDescription) await pc.addIceCandidate(data.candidate).catch(() => {});
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
    ch.onclose = () => { if (this.open) this._close(); };
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
