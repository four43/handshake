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
