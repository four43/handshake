// Unit tests for handshake.js against a scripted fake server and fake WebRTC (see fakes.js).
import { test, mock } from 'node:test';
import assert from 'node:assert/strict';
import { Handshake, HandshakeError, connectionType, shareUrl } from '../handshake.js';
import { fakeEnv, until, statsFor } from './fakes.js';

const SERVER = 'https://handshake.test';
const make = (env, opts = {}) => new Handshake({
  server: SERVER, app: 'game', version: 3, wakeLock: false,
  WebSocket: env.WebSocket, RTCPeerConnection: env.RTCPeerConnection, fetch: env.fetch, ...opts,
});
const view = (extra = {}) => ({
  code: 'K7MX2', name: 'Room', public: true, max_players: 4, locked: false, meta: null,
  host: 'H', you: 'H', is_host: true, resume: 'rH', key: 'kk',
  peers: [{ id: 'H', name: 'Host', away: false, nearby: true }], ...extra,
});
const guestView = (extra = {}) => view({
  you: 'G', is_host: false, key: null, resume: 'rG',
  peers: [{ id: 'H', name: 'Host', away: false, nearby: true }, { id: 'G', name: 'Player', away: false, nearby: false }], ...extra,
});

async function hosting(env, opts) {
  const hs = make(env, opts), pending = hs.createRoom({ public: true });
  const ws = await until(() => env.sockets[0]);
  await ws.next('create');
  ws.push({ t: 'joined', resumed: false, room: view() });
  return { hs, ws, room: await pending };
}
async function guesting(env, opts) {
  const hs = make(env, opts), pending = hs.joinRoom('k7mx2');
  const ws = await until(() => env.sockets[0]);
  await ws.next('join');
  ws.push({ t: 'joined', resumed: false, room: guestView() });
  return { hs, ws, room: await pending };
}
/** Record every call of the named events as [name, ...args]. */
const record = (emitter, names) => { const log = []; for (const n of names) emitter.on(n, (...a) => log.push([n, ...a])); return log; };

test('createRoom fetches a session and TURN, says hello, and returns the room', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  assert.deepEqual(env.posts.map(p => p.path), ['/session', '/turn']);
  assert.deepEqual(JSON.parse(env.posts[0].init.body), { app: 'game', version: 3 });
  assert.equal(env.posts[1].init.headers.authorization, 'Bearer tok1');
  assert.equal(ws.url, 'wss://handshake.test/ws');
  assert.deepEqual(ws.sent.slice(0, 2), [{ t: 'hello', v: 1, token: 'tok1' }, { t: 'create', public: true }]);
  assert.deepEqual(
    [room.code, room.key, room.isHost, room.you, room.hostId, room.locked, room.meta],
    ['K7MX2', 'kk', true, 'H', 'H', false, null],
  );
  assert.deepEqual(room.members, [{ id: 'H', name: 'Host', nearby: true, away: false }]);
  hs.close();
});

test('peek normalizes the code, sends a key, maps room_info; errors reject with their code', async () => {
  const env = fakeEnv(), hs = make(env), pending = hs.peek(' k7mx2 ');
  const ws = await until(() => env.sockets[0]);
  assert.deepEqual(await ws.next('peek'), { t: 'peek', code: 'K7MX2' });
  ws.push({ t: 'room_info', code: 'K7MX2', name: 'Room', players: 2, max_players: 4, locked: false, full: false });
  assert.deepEqual(await pending, { code: 'K7MX2', name: 'Room', players: 2, maxPlayers: 4, locked: false, full: false });
  assert.deepEqual(env.posts.map(p => p.path), ['/session']); // no TURN for a peek

  const missing = hs.peek('ZZZZZ');
  await ws.next('peek');
  ws.push({ t: 'error', code: 'not_found', message: 'no room with that code' });
  await assert.rejects(missing, e => e instanceof HandshakeError && e.code === 'not_found');

  const priv = hs.peek('AAAAA', 'kk'); // a private room's key goes along
  assert.deepEqual(await ws.next('peek'), { t: 'peek', code: 'AAAAA', key: 'kk' });
  ws.push({ t: 'error', code: 'bad_key', message: 'invalid room key' });
  await assert.rejects(priv, { code: 'bad_key' });
  hs.close();
});

test('session failures reject with the server error code or network', async () => {
  const env = fakeEnv();
  env.sessionError = { status: 403, error: 'origin' };
  await assert.rejects(make(env).peek('AAAAA'), { code: 'origin' });
  env.sessionError = null; env.failFetch = true;
  await assert.rejects(make(env).peek('AAAAA'), { code: 'network' });
});

test('hello without welcome times out after 10 s', async () => {
  mock.timers.enable({ apis: ['setTimeout'] });
  try {
    const env = fakeEnv(); env.autoWelcome = false;
    const pending = make(env).peek('AAAAA');
    const ws = await until(() => env.sockets[0]);
    await ws.next('hello');
    mock.timers.tick(10_000);
    await assert.rejects(pending, { code: 'timeout' });
  } finally { mock.timers.reset(); }
});

test('a refused hello rejects with its code and the next call fetches a new token', async () => {
  const env = fakeEnv(); env.autoWelcome = false;
  const hs = make(env), first = hs.peek('AAAAA');
  const ws = await until(() => env.sockets[0]);
  await ws.next('hello');
  ws.push({ t: 'error', code: 'bad_token', message: 'invalid or expired session token' });
  await assert.rejects(first, { code: 'bad_token' });

  env.autoWelcome = true;
  const second = hs.peek('AAAAA');
  const ws2 = await until(() => env.sockets[1]);
  assert.equal((await ws2.next('hello')).token, 'tok2');
  await ws2.next('peek');
  ws2.push({ t: 'error', code: 'not_found', message: '' });
  await assert.rejects(second, { code: 'not_found' });
  hs.close();
});

test('a refused join rejects and leaves the client free to try again', async () => {
  const env = fakeEnv(), hs = make(env), full = hs.joinRoom('AAAAA');
  const ws = await until(() => env.sockets[0]);
  assert.deepEqual(await ws.next('join'), { t: 'join', code: 'AAAAA' });
  ws.push({ t: 'error', code: 'full', message: 'room is full' });
  await assert.rejects(full, { code: 'full' });

  const ok = hs.joinRoom('K7MX2', 'kk');
  assert.deepEqual(await ws.next('join'), { t: 'join', code: 'K7MX2', key: 'kk' });
  ws.push({ t: 'joined', resumed: false, room: guestView() });
  assert.equal((await ok).isHost, false);
  await assert.rejects(hs.createRoom(), { code: 'already_in_room' });
  hs.close();
});

test('without TURN there is no /turn call and no ICE servers', async () => {
  const env = fakeEnv({ turn: false }), { hs, ws } = await hosting(env);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  assert.deepEqual(env.posts.map(p => p.path), ['/session']);
  assert.deepEqual(env.pcs[0].config.iceServers, []);
  hs.close();
});

test('close leaves the room, closes the socket and refuses later calls', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const closed = record(room, ['closed']);
  hs.close();
  assert.deepEqual(ws.sent.at(-1), { t: 'leave' });
  assert.equal(ws.readyState, 3);
  assert.deepEqual(closed, [['closed', 'left']]);
  await assert.rejects(hs.peek('AAAAA'), { code: 'closed' });
});

test('connectionType reads relay at either end, and null without a selected pair', () => {
  assert.equal(connectionType(new Map()), null);
  assert.equal(connectionType(statsFor('relay')), 'relayed');
  assert.equal(connectionType(statsFor('host', 'relay')), 'relayed');
  assert.equal(connectionType(statsFor('srflx', 'host')), 'direct');
});

test('room events keep members up to date', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const log = record(room, ['members', 'peerAway', 'peerBack', 'peerLeft', 'meta']);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: false });
  assert.deepEqual(room.members, [{ id: 'H', name: 'Host', nearby: true, away: false }, { id: 'G', name: 'Player', nearby: false, away: false }]);
  ws.push({ t: 'peer_away', peer: 'G' });
  assert.equal(room.members[1].away, true);
  ws.push({ t: 'peer_back', peer: 'G' });
  ws.push({ t: 'room_meta', meta: { farm: 7 }, locked: true });
  assert.deepEqual([room.locked, room.meta], [true, { farm: 7 }]);
  ws.push({ t: 'peer_left', peer: 'G', reason: 'timeout' });
  assert.deepEqual(room.members.map(m => m.id), ['H']);
  assert.deepEqual(log.map(e => e[0]), ['members', 'peerAway', 'members', 'peerBack', 'members', 'meta', 'peerLeft', 'members']);
  assert.deepEqual(log.find(e => e[0] === 'peerLeft'), ['peerLeft', 'G', 'timeout']);
  assert.equal(room.peers.has('G'), false);
  assert.equal(env.pcs[0].closed, true);
  hs.close();
});

test('guests hear when the host is away and back', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const log = record(room, ['hostAway', 'hostBack']);
  ws.push({ t: 'host_away', grace_secs: 30 });
  assert.equal(room.members[0].away, true);
  ws.push({ t: 'host_back' });
  assert.equal(room.members[0].away, false);
  assert.deepEqual(log, [['hostAway', 30], ['hostBack']]);
  hs.close();
});

test('lock, setMeta, kick and leave send their messages; leave closes the room and fails pending controls', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const closed = record(room, ['closed']);
  const pending = [room.lock(true), room.setMeta({ farm: 1 }), room.kick('G')];
  room.leave();
  assert.deepEqual(ws.sent.slice(2), [{ t: 'lock', locked: true }, { t: 'meta', meta: { farm: 1 } }, { t: 'kick', peer: 'G' }, { t: 'leave' }]);
  assert.deepEqual(closed, [['closed', 'left']]);
  for (const p of pending) await assert.rejects(p, { code: 'closed' });
  const again = hs.createRoom();
  await ws.next('create');
  ws.push({ t: 'joined', resumed: false, room: view({ code: 'AAAAA' }) });
  assert.equal((await again).code, 'AAAAA');
  hs.close();
});

test('room_closed and kicked close the room with their reason', async () => {
  const env = fakeEnv(), a = await hosting(env), log = record(a.room, ['closed']);
  a.ws.push({ t: 'room_closed', reason: 'expired' });
  assert.deepEqual(log, [['closed', 'expired']]);
  a.hs.close();

  const env2 = fakeEnv(), b = await guesting(env2), log2 = record(b.room, ['closed']);
  b.ws.push({ t: 'kicked' });
  assert.deepEqual(log2, [['closed', 'kicked']]);
  assert.equal(env2.pcs[0].closed, true);
  b.hs.close();
});

test('the host offers to each new guest over two channels and opens when both are open', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const opened = record(room, ['peer']);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  const pc = env.pcs[0];
  assert.deepEqual(pc.config, { iceServers: [{ urls: ['turn:turn.test:3478'], username: 'u', credential: 'c' }] });
  assert.deepEqual(pc.channels.map(c => [c.label, c.opts]), [['state', { ordered: false, maxRetransmits: 0 }], ['events', { ordered: true }]]);
  assert.deepEqual(await ws.next('signal'), { t: 'signal', to: 'G', data: { sdp: { type: 'offer', sdp: 'offer1' }, new: true } });

  pc.ice({ candidate: 'c1' });
  assert.deepEqual((await ws.next('signal')).data, { candidate: { candidate: 'c1' } });

  ws.push({ t: 'signal', from: 'G', data: { candidate: { candidate: 'g1' } } }); // before the answer: held
  ws.push({ t: 'signal', from: 'G', data: { sdp: { type: 'answer', sdp: 'answer' } } });
  await until(() => pc.candidates.length === 1);
  assert.deepEqual(pc.remoteDescription, { type: 'answer', sdp: 'answer' });
  assert.deepEqual(pc.candidates, [{ candidate: 'g1' }]);

  pc.channels[0].open();
  assert.equal(opened.length, 0);
  pc.channels[1].open();
  const peer = room.peers.get('G');
  assert.deepEqual(opened, [['peer', peer]]);
  assert.deepEqual([peer.open, peer.nearby, pc.channels[0].binaryType], [true, true, 'arraybuffer']);
  hs.close();
});

test("a guest answers the host's offer and opens on the host's channels", async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const opened = record(room, ['peer']), pc = env.pcs[0];
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offer1' } } });
  assert.deepEqual(await ws.next('signal'), { t: 'signal', to: 'H', data: { sdp: { type: 'answer', sdp: 'answer' } } });
  pc.remoteChannels();
  pc.channels.forEach(c => c.open());
  assert.equal(opened.length, 1);
  assert.equal(room.peers.get('H').open, true);
  hs.close();
});

test('send uses the state channel for binary and the events channel for reliable data', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  const pc = env.pcs[0], peer = room.peers.get('G'), bytes = new Uint8Array([1, 2, 3]);
  assert.equal(peer.send(bytes), false); // not open yet
  pc.open();
  const [stateCh, eventsCh] = pc.channels;
  assert.equal(peer.send(bytes), true);
  assert.equal(peer.send({ score: 1 }, { reliable: true }), true);
  peer.send(bytes.buffer, { reliable: true });
  assert.deepEqual([stateCh.sent, eventsCh.sent], [[bytes], ['{"score":1}', bytes.buffer]]);
  assert.throws(() => peer.send({ score: 1 }), TypeError);

  const got = record(peer, ['message']);
  stateCh.receive(bytes.buffer);
  eventsCh.receive('{"hi":true}');
  eventsCh.receive('not json');
  stateCh.receive('text on the state channel');
  assert.deepEqual(got, [['message', bytes.buffer, { reliable: false }], ['message', { hi: true }, { reliable: true }]]);
  hs.close();
});

test('relayUnlessNearby forces relay for players who are not nearby', async () => {
  const env = fakeEnv(), h = await hosting(env, { relayUnlessNearby: true });
  h.ws.push({ t: 'peer_joined', peer: 'N', name: 'Player', nearby: true });
  h.ws.push({ t: 'peer_joined', peer: 'F', name: 'Player', nearby: false });
  assert.deepEqual(env.pcs.map(pc => pc.config.iceTransportPolicy), [undefined, 'relay']);
  h.hs.close();

  const env2 = fakeEnv(), g = await guesting(env2, { relayUnlessNearby: true }); // guestView: G is not nearby
  assert.equal(env2.pcs[0].config.iceTransportPolicy, 'relay');
  g.hs.close();

  const env3 = fakeEnv(), d = await guesting(env3); // option off: never forced
  assert.equal(env3.pcs[0].config.iceTransportPolicy, undefined);
  d.hs.close();
});

test('connectionType follows the selected candidate pair and emits type on change', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: false });
  const pc = env.pcs[0], peer = room.peers.get('G'), types = record(peer, ['type']);
  pc.setStats('relay'); pc.state('connected');
  await until(() => peer.connectionType === 'relayed');
  pc.setStats('host', 'srflx'); pc.state('connected');
  await until(() => peer.connectionType === 'direct');
  assert.deepEqual(types, [['type', 'relayed'], ['type', 'direct']]);
  hs.close();
});

test('a failed connection restarts ICE: the host re-offers, a guest asks the host to', async () => {
  const env = fakeEnv(), h = await hosting(env);
  h.ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  await h.ws.next('signal');
  env.pcs[0].state('failed');
  const again = await h.ws.next('signal');
  assert.deepEqual([env.pcs[0].offers.at(-1), again.data.sdp.sdp], [{ iceRestart: true }, 'offer2']);
  h.ws.push({ t: 'signal', from: 'G', data: { restart: true } });
  assert.equal((await h.ws.next('signal')).data.sdp.sdp, 'offer3');
  h.hs.close();

  const env2 = fakeEnv(), g = await guesting(env2);
  env2.pcs[0].state('failed');
  assert.deepEqual(await g.ws.next('signal'), { t: 'signal', to: 'H', data: { restart: true } });
  g.hs.close();
});

test('a dropped socket resumes the room and sends signals queued meanwhile', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const pc = env.pcs[0], closed = record(room, ['closed']);
  ws.drop();
  pc.ice({ candidate: 'late' }); // no socket: queued
  const ws2 = await until(() => env.sockets[1]); // after the 250 ms backoff
  assert.deepEqual(await ws2.next('resume'), { t: 'resume', token: 'rG' });
  ws2.push({ t: 'joined', resumed: true, room: guestView({ resume: 'rG2' }) });
  assert.deepEqual(await ws2.next('signal'), { t: 'signal', to: 'H', data: { candidate: { candidate: 'late' } } });
  assert.equal(env.pcs.length, 1); // the WebRTC connection is untouched
  assert.deepEqual(closed, []);

  ws2.drop();
  const ws3 = await until(() => env.sockets[2]);
  assert.equal((await ws3.next('resume')).token, 'rG2'); // the newest resume token
  hs.close();
});

test('a resume that finds no room closes it as lost', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const closed = record(room, ['closed']);
  ws.drop();
  const ws2 = await until(() => env.sockets[1]);
  await ws2.next('resume');
  ws2.push({ t: 'error', code: 'not_found', message: 'room no longer exists' });
  await until(() => closed.length === 1);
  assert.deepEqual(closed, [['closed', 'lost']]);
  assert.equal(env.pcs[0].closed, true);
  hs.close();
});

test('a host that resumes adds guests it missed and drops guests that left', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  const left = record(room, ['peerLeft']);
  ws.drop();
  const ws2 = await until(() => env.sockets[1]);
  await ws2.next('resume');
  ws2.push({ t: 'joined', resumed: true, room: view({
    resume: 'rH2',
    peers: [{ id: 'H', name: 'Host', away: false, nearby: true }, { id: 'G2', name: 'Player', away: false, nearby: false }],
  }) });
  await until(() => room.peers.has('G2'));
  assert.deepEqual([...room.peers.keys()], ['G2']);
  assert.deepEqual(left, [['peerLeft', 'G', 'left']]);
  assert.equal(env.pcs[0].closed, true);
  hs.close();
});

test('an expiring session token is fetched again before the next hello', async () => {
  const env = fakeEnv(); env.expiresIn = 30; // inside the 60 s margin: every connect fetches a new token
  const { hs, ws } = await guesting(env);
  const before = env.sessionCount;
  ws.drop();
  const ws2 = await until(() => env.sockets[1]);
  const hello = await ws2.next('hello');
  assert.equal(env.sessionCount, before + 1);
  assert.equal(hello.token, `tok${before + 1}`);
  hs.close();
});

test('a heartbeat with no answer treats the socket as dead and resumes', async () => {
  mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  try {
    const env = fakeEnv(), { hs, ws } = await guesting(env);
    mock.timers.tick(25_000);
    await ws.next('list');
    mock.timers.tick(10_000); // no `rooms` reply
    await until(() => ws.readyState === 3);
    mock.timers.tick(250);
    const ws2 = await until(() => env.sockets[1]);
    assert.equal((await ws2.next('resume')).token, 'rG');
    hs.close();
  } finally { mock.timers.reset(); }
});

test('names: yours goes as player on create and join, the room gets one, members and peers carry theirs', async () => {
  const env = fakeEnv(), hs = make(env, { name: 'Seth' }), pending = hs.createRoom({ name: 'Pig pens' });
  const ws = await until(() => env.sockets[0]);
  assert.deepEqual(await ws.next('create'), { t: 'create', public: false, name: 'Pig pens', player: 'Seth' });
  ws.push({ t: 'joined', resumed: false, room: view({ name: 'Pig pens', peers: [{ id: 'H', name: 'Seth', away: false, nearby: true }] }) });
  const room = await pending;
  assert.equal(room.name, 'Pig pens');
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Ada', nearby: true });
  assert.deepEqual(room.members.map(m => m.name), ['Seth', 'Ada']);
  assert.equal(room.peers.get('G').name, 'Ada');
  hs.close();

  const env2 = fakeEnv(), hs2 = make(env2, { name: 'Ada' }), joining = hs2.joinRoom('K7MX2', 'kk');
  const ws2 = await until(() => env2.sockets[0]);
  assert.deepEqual(await ws2.next('join'), { t: 'join', code: 'K7MX2', key: 'kk', player: 'Ada' });
  ws2.push({ t: 'joined', resumed: false, room: guestView() });
  const g = await joining;
  assert.equal(g.peers.get('H').name, 'Host'); // a guest's one peer is the host
  hs2.close();
});

test('a resume refreshes member names', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  ws.drop();
  const ws2 = await until(() => env.sockets[1]);
  await ws2.next('resume');
  ws2.push({ t: 'joined', resumed: true, room: guestView({
    name: 'Renamed', peers: [{ id: 'H', name: 'Host2', away: false, nearby: true }, { id: 'G', name: 'Player', away: false, nearby: false }],
  }) });
  await until(() => room.members[0].name === 'Host2');
  assert.equal(room.name, 'Renamed');
  hs.close();
});

test('listRooms maps the public list; replies are matched in order', async () => {
  const env = fakeEnv(), hs = make(env), first = hs.listRooms(), second = hs.listRooms();
  const ws = await until(() => env.sockets[0]);
  await ws.next('list'); await ws.next('list');
  ws.push({ t: 'rooms', rooms: [{ code: 'AAAAA', name: 'Pens', players: 2, max_players: 4, meta: { map: 1 }, nearby: true }] });
  ws.push({ t: 'rooms', rooms: [] });
  assert.deepEqual(await first, [{ code: 'AAAAA', name: 'Pens', players: 2, maxPlayers: 4, meta: { map: 1 }, nearby: true }]);
  assert.deepEqual(await second, []);
  assert.deepEqual(env.posts.map(p => p.path), ['/session']); // no TURN for a list
  hs.close();
});

test('shareUrl sets r and k on a page URL and keeps its other params', () => {
  assert.equal(shareUrl('https://g.test/play/?lang=fi#top', 'K7MX2', 'kk'), 'https://g.test/play/?lang=fi&r=K7MX2&k=kk#top');
  assert.equal(shareUrl('https://g.test/?r=OLD&k=old', 'K7MX2', null), 'https://g.test/?r=K7MX2');
});

test('room.shareUrl uses the page location; guests keep the key they joined with', async () => {
  globalThis.location = { href: 'https://g.test/play/' };
  try {
    const env = fakeEnv(), h = await hosting(env);
    assert.equal(h.room.shareUrl, 'https://g.test/play/?r=K7MX2&k=kk');
    h.hs.close();

    const env2 = fakeEnv(), hs = make(env2), joining = hs.joinRoom('K7MX2', 'kk');
    const ws = await until(() => env2.sockets[0]);
    await ws.next('join');
    ws.push({ t: 'joined', resumed: false, room: guestView() }); // key: null for a guest
    const room = await joining;
    assert.equal(room.key, 'kk');
    assert.equal(room.shareUrl, 'https://g.test/play/?r=K7MX2&k=kk');
    hs.close();
  } finally { delete globalThis.location; }

  const env3 = fakeEnv(), g = await guesting(env3); // no location (not a browser): no share URL
  assert.equal(g.room.shareUrl, null);
  g.hs.close();
});

test('joinFromUrl joins with r and k, and resolves null without r', async () => {
  const env = fakeEnv(), hs = make(env);
  assert.equal(await hs.joinFromUrl('https://g.test/play/?lang=fi'), null);
  assert.equal(env.sockets.length, 0); // nothing opened
  const joining = hs.joinFromUrl('https://g.test/play/?r=k7mx2&k=kk');
  const ws = await until(() => env.sockets[0]);
  assert.deepEqual(await ws.next('join'), { t: 'join', code: 'K7MX2', key: 'kk' });
  ws.push({ t: 'joined', resumed: false, room: guestView() });
  assert.equal((await joining).code, 'K7MX2');
  hs.close();
});

test('lock and setMeta resolve on the room_meta they cause, which still updates the room', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const metas = record(room, ['meta']);
  const locking = room.lock(true), meta = room.setMeta({ map: 2 });
  await ws.next('lock'); await ws.next('meta');
  ws.push({ t: 'room_meta', meta: null, locked: true });
  ws.push({ t: 'room_meta', meta: { map: 2 }, locked: true });
  await locking; await meta;
  assert.deepEqual([room.locked, room.meta], [true, { map: 2 }]);
  assert.equal(metas.length, 2);
  hs.close();
});

test('host controls reject with the server error, not a console warning', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const big = room.setMeta({ huge: true });
  await ws.next('meta');
  ws.push({ t: 'error', code: 'meta_too_large', message: 'meta must be 1 KB or less' });
  await assert.rejects(big, e => e instanceof HandshakeError && e.code === 'meta_too_large');

  const gone = room.kick('X');
  await ws.next('kick');
  ws.push({ t: 'error', code: 'peer_unavailable', message: 'no such peer' });
  await assert.rejects(gone, { code: 'peer_unavailable' });
  hs.close();
});

test('kick resolves when that peer has left', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  ws.push({ t: 'peer_joined', peer: 'G', name: 'Ada', nearby: true });
  ws.push({ t: 'peer_joined', peer: 'G2', name: 'Bo', nearby: true });
  const left = record(room, ['peerLeft']);
  let done = false;
  const kicking = room.kick('G2').then(() => { done = true; });
  await ws.next('kick');
  ws.push({ t: 'peer_left', peer: 'G', reason: 'left' }); // someone else
  await new Promise(r => setImmediate(r));
  assert.equal(done, false);
  ws.push({ t: 'peer_left', peer: 'G2', reason: 'kicked' });
  await kicking;
  assert.deepEqual(left, [['peerLeft', 'G', 'left'], ['peerLeft', 'G2', 'kicked']]);
  hs.close();
});

test('host controls: not_host for a guest, network while the socket is down, timeout without a reply', async () => {
  const env = fakeEnv(), g = await guesting(env);
  await assert.rejects(g.room.lock(true), { code: 'not_host' });
  await assert.rejects(g.room.setMeta({}), { code: 'not_host' });
  await assert.rejects(g.room.kick('H'), { code: 'not_host' });
  assert.equal(g.ws.sent.some(m => ['lock', 'meta', 'kick'].includes(m.t)), false);
  g.hs.close();

  const env2 = fakeEnv(), h = await hosting(env2);
  h.ws.drop();
  await assert.rejects(h.room.lock(true), { code: 'network' });
  assert.equal(env2.sockets.length, 1); // no fresh socket outside the resume
  h.hs.close();

  mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  try {
    const env3 = fakeEnv(), t = await hosting(env3);
    const locking = t.room.lock(true);
    await t.ws.next('lock');
    mock.timers.tick(10_000);
    await assert.rejects(locking, { code: 'timeout' });
    t.hs.close();
  } finally { mock.timers.reset(); }
});

// ---- reconnecting a dropped peer (C-1) ----------------------------------------

/** Host with guest G (nearby): the offer is out. */
async function hostWithGuest(env, opts) {
  const h = await hosting(env, opts);
  h.ws.push({ t: 'peer_joined', peer: 'G', name: 'Player', nearby: true });
  await h.ws.next('signal');
  return h;
}

test('the first offer to a guest is marked new; an ICE restart offer is not', async () => {
  const env = fakeEnv(), h = await hostWithGuest(env);
  assert.equal(h.ws.sent.find(m => m.t === 'signal').data.new, true);
  env.pcs[0].state('failed');
  const again = await h.ws.next('signal');
  assert.equal(again.data.new, undefined);
  h.hs.close();
});

test('a channel that closes before both open closes the peer, and the host offers again', async () => {
  const env = fakeEnv(), h = await hostWithGuest(env), closes = [];
  const first = h.room.peers.get('G');
  first.on('close', () => closes.push('G'));
  env.pcs[0].channels[0].close(); // never opened
  assert.deepEqual([closes, env.pcs[0].closed], [['G'], true]);
  const offer = await h.ws.next('signal');
  assert.deepEqual([offer.to, offer.data.new, env.pcs.length], ['G', true, 2]);
  assert.notEqual(h.room.peers.get('G'), first);
  h.hs.close();
});

test('the host rebuilds a closed peer when the guest comes back', async () => {
  const env = fakeEnv(), h = await hostWithGuest(env);
  env.pcs[0].open();
  h.ws.push({ t: 'peer_away', peer: 'G' });
  env.pcs[0].channels[1].close(); // ICE failed while the guest was away
  assert.equal(h.room.peers.has('G'), false);
  await new Promise(r => setImmediate(r));
  assert.equal(env.pcs.length, 1); // no offer to a guest whose socket is down
  h.ws.push({ t: 'peer_back', peer: 'G' });
  const offer = await h.ws.next('signal');
  assert.deepEqual([offer.to, offer.data.new, env.pcs.length], ['G', true, 2]);
  h.hs.close();
});

test('the host rebuilds when a guest with no peer asks for a restart, or a guest asks for a new connection', async () => {
  const env = fakeEnv(), h = await hostWithGuest(env);
  env.pcs[0].open();
  h.ws.push({ t: 'peer_away', peer: 'G' });
  env.pcs[0].channels[0].close();
  h.ws.push({ t: 'signal', from: 'G', data: { restart: true } }); // the guest's socket resumed first
  const offer = await h.ws.next('signal');
  assert.deepEqual([offer.data.new, env.pcs.length], [true, 2]);

  // The guest lost its side: the open peer here is replaced, not ICE-restarted.
  h.ws.push({ t: 'signal', from: 'G', data: { sdp: { type: 'answer', sdp: 'answer' } } });
  await until(() => env.pcs[1].remoteDescription);
  env.pcs[1].open();
  h.ws.push({ t: 'signal', from: 'G', data: { restart: true, rebuild: true } });
  const again = await h.ws.next('signal');
  assert.deepEqual([again.data.new, env.pcs.length, env.pcs[1].closed], [true, 3, true]);

  // A rebuild request while a new offer is still unanswered changes nothing.
  h.ws.push({ t: 'signal', from: 'G', data: { restart: true, rebuild: true } });
  await new Promise(r => setImmediate(r));
  assert.equal(env.pcs.length, 3);
  h.hs.close();
});

test('rebuilds back off while a guest keeps failing, and stop when the room closes', async () => {
  mock.timers.enable({ apis: ['setTimeout'] });
  try {
    const env = fakeEnv(), h = await hostWithGuest(env);
    env.pcs[0].channels[0].close();
    await until(() => env.pcs.length === 2); // the first rebuild is at once
    env.pcs[1].channels[0].close();
    await new Promise(r => setImmediate(r));
    assert.equal(env.pcs.length, 2);
    mock.timers.tick(2_000);
    await until(() => env.pcs.length === 3);
    env.pcs[2].channels[0].close();
    h.room.leave();
    mock.timers.tick(60_000);
    await new Promise(r => setImmediate(r));
    assert.equal(env.pcs.length, 3);
    h.hs.close();
  } finally { mock.timers.reset(); }
});

test("a guest whose connection closes asks the host for a new one and takes the host's new offer", async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const opened = record(room, ['peer']);
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offer1' }, new: true } });
  await ws.next('signal');
  env.pcs[0].remoteChannels(); env.pcs[0].channels.forEach(c => c.open());
  env.pcs[0].channels[0].close();
  assert.deepEqual(await ws.next('signal'), { t: 'signal', to: 'H', data: { restart: true, rebuild: true } });
  assert.equal(room.peers.has('H'), false);
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offer9' }, new: true } });
  assert.equal((await ws.next('signal')).data.sdp.type, 'answer');
  assert.equal(env.pcs.length, 2);
  env.pcs[1].remoteChannels(); env.pcs[1].channels.forEach(c => c.open());
  assert.equal(opened.length, 2);
  hs.close();
});

test('a guest replaces its peer when the host sends a new offer for a connection it already has', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offer1' }, new: true } });
  await ws.next('signal');
  env.pcs[0].remoteChannels(); env.pcs[0].channels.forEach(c => c.open());
  const old = room.peers.get('H');
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offerB' }, new: true } }); // the host rebuilt its side
  await ws.next('signal');
  assert.deepEqual([env.pcs.length, env.pcs[0].closed, env.pcs[1].remoteDescription.sdp], [2, true, 'offerB']);
  assert.notEqual(room.peers.get('H'), old);
  assert.equal(ws.sent.some(m => m.t === 'signal' && m.data.rebuild), false); // a replaced peer asks for nothing
  hs.close();
});

test('a guest that loses its connection while its socket is down asks after the resume', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  ws.push({ t: 'signal', from: 'H', data: { sdp: { type: 'offer', sdp: 'offer1' }, new: true } });
  await ws.next('signal');
  env.pcs[0].remoteChannels(); env.pcs[0].channels.forEach(c => c.open());
  ws.drop();
  env.pcs[0].channels[1].close();
  const ws2 = await until(() => env.sockets[1]);
  await ws2.next('resume');
  ws2.push({ t: 'joined', resumed: true, room: guestView({ resume: 'rG2' }) });
  assert.deepEqual((await ws2.next('signal')).data, { restart: true, rebuild: true });
  assert.equal(room.closed, false);
  hs.close();
});

// ---- TURN required (C-2) ------------------------------------------------------

test('relayUnlessNearby: a failed TURN fetch is tried once more, then createRoom and joinRoom fail with no_turn', async () => {
  const env = fakeEnv();
  env.turnFailures = 1;
  const h = await hosting(env, { relayUnlessNearby: true }); // the second try works
  assert.equal(env.posts.filter(p => p.path === '/turn').length, 2);
  h.hs.close();

  const env2 = fakeEnv();
  env2.turnFailures = 2;
  const hs2 = make(env2, { relayUnlessNearby: true });
  await assert.rejects(hs2.createRoom(), e => e instanceof HandshakeError && e.code === 'no_turn');
  assert.equal(env2.sockets.length, 0);
  env2.turnFailures = 2;
  await assert.rejects(hs2.joinRoom('K7MX2'), { code: 'no_turn' });
  hs2.close();
});

test('relayUnlessNearby with a session that offers no TURN fails with no_turn', async () => {
  const env = fakeEnv({ turn: false }), hs = make(env, { relayUnlessNearby: true });
  await assert.rejects(hs.joinRoom('K7MX2'), { code: 'no_turn' });
  hs.close();
});

test('without relayUnlessNearby a failed TURN fetch is a warning and the room still opens', async t => {
  const warn = t.mock.method(console, 'warn', () => {});
  const env = fakeEnv();
  env.turnFailures = 2;
  const h = await hosting(env);
  assert.equal(h.room.code, 'K7MX2');
  assert.equal(warn.mock.callCount(), 1);
  h.hs.close();
});

// ---- send buffer cap (C-3) ----------------------------------------------------

test('send skips a channel whose buffer is over its cap', async () => {
  const env = fakeEnv(), h = await hostWithGuest(env);
  env.pcs[0].open();
  const peer = h.room.peers.get('G'), [stateCh, eventsCh] = env.pcs[0].channels;
  stateCh.bufferedAmount = 256 * 1024;
  assert.equal(peer.send(new Uint8Array([1])), false);
  eventsCh.bufferedAmount = 4 * 1024 * 1024;
  assert.equal(peer.send({ a: 1 }, { reliable: true }), false);
  assert.deepEqual([stateCh.sent, eventsCh.sent], [[], []]);
  stateCh.bufferedAmount = 0;
  assert.equal(peer.send(new Uint8Array([1])), true);
  h.hs.close();
});

// ---- errors go to the request they answer (C-5) -------------------------------

test('an error for another request does not fail a pending resume', async () => {
  const env = fakeEnv(), { hs, ws, room } = await guesting(env);
  const closed = record(room, ['closed']);
  ws.drop();
  const ws2 = await until(() => env.sockets[1]);
  await ws2.next('resume');
  ws2.push({ t: 'error', code: 'peer_unavailable', message: 'that peer is not connected', re: 'signal' });
  ws2.push({ t: 'error', code: 'not_in_room', message: 'join a room first' }); // an older server sends no re
  ws2.push({ t: 'joined', resumed: true, room: guestView({ resume: 'rG2' }) });
  ws2.drop();
  const ws3 = await until(() => env.sockets[2]);
  assert.equal((await ws3.next('resume')).token, 'rG2'); // the first resume worked
  assert.deepEqual(closed, []);
  hs.close();
});

test('an error with re rejects the request of that type, not the first one', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const meta = room.setMeta({ a: 1 }), kick = room.kick('X');
  await ws.next('kick');
  ws.push({ t: 'error', code: 'peer_unavailable', message: 'no such peer', re: 'kick' });
  await assert.rejects(kick, { code: 'peer_unavailable' });
  ws.push({ t: 'room_meta', meta: { a: 1 }, locked: false });
  await meta;
  hs.close();
});

// ---- no silent failures -------------------------------------------------------

test('a candidate that cannot be added is warned about once', async t => {
  const warn = t.mock.method(console, 'warn', () => {});
  const env = fakeEnv(), h = await hostWithGuest(env);
  h.ws.push({ t: 'signal', from: 'G', data: { sdp: { type: 'answer', sdp: 'answer' } } });
  h.ws.push({ t: 'signal', from: 'G', data: { candidate: { bad: 1 } } });
  h.ws.push({ t: 'signal', from: 'G', data: { candidate: { bad: 2 } } });
  h.ws.push({ t: 'signal', from: 'G', data: { candidate: { candidate: 'ok' } } });
  await until(() => env.pcs[0].candidates.length === 1);
  assert.equal(warn.mock.calls.filter(c => c.arguments.some(a => String(a).includes('candidate'))).length, 1);
  h.hs.close();
});
