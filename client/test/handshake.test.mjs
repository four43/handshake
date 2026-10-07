// Unit tests for handshake.js against a scripted fake server and fake WebRTC (see fakes.js).
import { test, mock } from 'node:test';
import assert from 'node:assert/strict';
import { Handshake, HandshakeError, connectionType } from '../handshake.js';
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
  assert.deepEqual(room.members, [{ id: 'H', nearby: true, away: false }]);
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
  assert.deepEqual(room.members, [{ id: 'H', nearby: true, away: false }, { id: 'G', nearby: false, away: false }]);
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

test('lock, setMeta, kick and leave send their messages; leave closes the room', async () => {
  const env = fakeEnv(), { hs, ws, room } = await hosting(env);
  const closed = record(room, ['closed']);
  room.lock(true); room.setMeta({ farm: 1 }); room.kick('G'); room.leave();
  assert.deepEqual(ws.sent.slice(2), [{ t: 'lock', locked: true }, { t: 'meta', meta: { farm: 1 } }, { t: 'kick', peer: 'G' }, { t: 'leave' }]);
  assert.deepEqual(closed, [['closed', 'left']]);
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
  assert.deepEqual(await ws.next('signal'), { t: 'signal', to: 'G', data: { sdp: { type: 'offer', sdp: 'offer1' } } });

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
