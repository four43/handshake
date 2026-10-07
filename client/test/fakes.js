// Fakes for handshake.js unit tests: a scripted signaling server (fetch + WebSocket) and WebRTC.
// Node has no WebRTC; the e2e test (client/e2e) covers the real thing.

const json = (status, body) => ({ ok: status < 400, status, json: async () => body });

/** Poll `fn` (on setImmediate, so mocked timers do not stop it) until it returns a truthy value. */
export async function until(fn, ms = 2000) {
  const end = Date.now() + ms;
  for (;;) {
    const v = fn();
    if (v) return v;
    if (Date.now() > end) throw new Error('until: timed out');
    await new Promise(r => setImmediate(r));
  }
}

/** A getStats() report whose selected pair has these candidate types. */
export function statsFor(localType, remoteType = 'host') {
  return new Map([
    ['T', { type: 'transport', selectedCandidatePairId: 'P' }],
    ['P', { type: 'candidate-pair', localCandidateId: 'L', remoteCandidateId: 'R', nominated: true, state: 'succeeded' }],
    ['L', { type: 'local-candidate', candidateType: localType }],
    ['R', { type: 'remote-candidate', candidateType: remoteType }],
  ]);
}

class FakeChannel {
  constructor(label, opts) { this.label = label; this.opts = opts; this.readyState = 'connecting'; this.binaryType = 'blob'; this.sent = []; }
  send(data) { this.sent.push(data); }
  close() { this.readyState = 'closed'; this.onclose?.(); }
  // test helpers
  open() { this.readyState = 'open'; this.onopen?.(); }
  receive(data) { this.onmessage?.({ data }); }
}

export function fakeEnv({ turn = true } = {}) {
  const env = {
    sockets: [], pcs: [], posts: [], sessionCount: 0,
    turn, expiresIn: 900, failFetch: false, sessionError: null, autoWelcome: true,
  };

  env.fetch = async (url, init) => {
    if (env.failFetch) throw new TypeError('Failed to fetch');
    const path = new URL(url).pathname;
    env.posts.push({ path, init });
    if (path === '/session') {
      if (env.sessionError) return json(env.sessionError.status, { error: env.sessionError.error });
      env.sessionCount++;
      return json(200, { token: `tok${env.sessionCount}`, expires_in: env.expiresIn, turn: env.turn });
    }
    if (path === '/turn') return json(200, { ice_servers: [{ urls: ['turn:turn.test:3478'], username: 'u', credential: 'c' }], ttl: 3600 });
    return json(404, { error: 'not_found' });
  };

  env.WebSocket = class FakeWebSocket {
    constructor(url) {
      this.url = url; this.readyState = 0; this.sent = []; this.taken = new Set(); this.waiters = [];
      env.sockets.push(this);
      queueMicrotask(() => { if (this.readyState !== 0) return; this.readyState = 1; this.onopen?.({}); });
    }
    send(text) {
      const m = JSON.parse(text);
      this.sent.push(m);
      if (m.t === 'hello' && env.autoWelcome) this.push({ t: 'welcome', v: 1 });
      for (const w of this.waiters.splice(0)) w();
    }
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      queueMicrotask(() => this.onclose?.({ code: 1000 }));
    }
    // test helpers
    /** The server sends `m` to the client (synchronously). */
    push(m) { this.onmessage?.({ data: JSON.stringify(m) }); }
    /** The network drops the socket. */
    drop() { this.readyState = 3; this.onclose?.({ code: 1006 }); }
    /** The next message of type `t` the client sent that no earlier `next` returned. */
    async next(t) {
      for (;;) {
        const i = this.sent.findIndex((m, k) => m.t === t && !this.taken.has(k));
        if (i >= 0) { this.taken.add(i); return this.sent[i]; }
        await new Promise(r => this.waiters.push(r));
      }
    }
  };

  env.RTCPeerConnection = class FakePeerConnection {
    constructor(config) {
      this.config = config; this.channels = []; this.candidates = []; this.offers = []; this.stats = new Map();
      this.localDescription = null; this.remoteDescription = null; this.connectionState = 'new'; this.closed = false;
      env.pcs.push(this);
    }
    createDataChannel(label, opts) { const ch = new FakeChannel(label, opts); this.channels.push(ch); return ch; }
    async createOffer(opts) { this.offers.push(opts ?? {}); return { type: 'offer', sdp: `offer${this.offers.length}` }; }
    async createAnswer() { return { type: 'answer', sdp: 'answer' }; }
    async setLocalDescription(d) { this.localDescription = d; }
    async setRemoteDescription(d) { this.remoteDescription = d; }
    async addIceCandidate(c) { this.candidates.push(c); }
    async getStats() { return this.stats; }
    getConfiguration() { return this.config; }
    setConfiguration(c) { this.config = c; }
    close() { this.closed = true; }
    // test helpers
    open() { for (const ch of this.channels) ch.open(); }
    remoteChannels() {
      for (const label of ['state', 'events']) { const ch = new FakeChannel(label, {}); this.channels.push(ch); this.ondatachannel?.({ channel: ch }); }
    }
    state(s) { this.connectionState = s; this.onconnectionstatechange?.(); }
    ice(candidate) { this.onicecandidate?.({ candidate }); }
    setStats(localType, remoteType) { this.stats = statsFor(localType, remoteType); }
  };

  return env;
}
