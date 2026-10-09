// A host and a guest in two browser contexts: real WebRTC through the real server (run with e2e/run.sh).
import { test, expect } from '@playwright/test';

test('host and guest connect, talk on both channels and survive a socket drop', async ({ browser }) => {
  const host = await (await browser.newContext()).newPage();
  const guest = await (await browser.newContext()).newPage();
  for (const p of [host, guest]) { await p.goto('/e2e/page.html'); await p.waitForFunction(() => window.ready); }

  const code = await host.evaluate(async () => {
    window.hs = make();
    window.room = await hs.createRoom({ public: true });
    window.events = [];
    for (const e of ['peerAway', 'peerBack']) room.on(e, () => events.push(e));
    room.on('peer', peer => { window.peer = peer; listen(peer); });
    return room.code;
  });
  expect(code).toMatch(/^[A-HJKMNP-Z2-9]{5}$/);

  const info = await guest.evaluate(async code => { window.hs = make(); return hs.peek(code); }, code);
  expect(info).toMatchObject({ code, players: 1, maxPlayers: 4, locked: false, full: false });

  await guest.evaluate(async code => {
    window.room = await hs.joinRoom(code);
    await new Promise(resolve => room.on('peer', peer => { window.peer = peer; listen(peer); resolve(); }));
  }, code);
  await host.waitForFunction(() => window.peer?.open);

  // guest -> host, binary on the unreliable channel (sent a few times: it may drop one)
  await guest.evaluate(() => { for (let i = 0; i < 20; i++) peer.send(new Uint8Array([1, 2, 3])); });
  await host.waitForFunction(() => got.some(m => !m.reliable && m.data.join() === '1,2,3'));
  // host -> guest, JSON on the reliable channel; guest -> host, binary on the reliable channel
  await host.evaluate(() => peer.send({ hello: 'guest' }, { reliable: true }));
  await guest.waitForFunction(() => got.some(m => m.reliable && m.data.hello === 'guest'));
  await guest.evaluate(() => peer.send(new Uint8Array([9]).buffer, { reliable: true }));
  await host.waitForFunction(() => got.some(m => m.reliable && Array.isArray(m.data) && m.data.join() === '9'));
  await host.waitForFunction(() => peer.connectionType === 'direct');

  // The guest's socket drops: the host sees it away, then back after the resume; WebRTC keeps working.
  await guest.evaluate(() => sockets.at(-1).close());
  await host.waitForFunction(() => events.join() === 'peerAway,peerBack');
  expect(await guest.evaluate(() => [sockets.length, room.closed])).toEqual([2, false]);
  await host.evaluate(() => peer.send({ after: 'resume' }, { reliable: true }));
  await guest.waitForFunction(() => got.some(m => m.reliable && m.data.after === 'resume'));

  for (const p of [host, guest]) await p.evaluate(() => hs.close());
});

// Both pages connect only through the server's built-in TURN relay, once per transport.
for (const transport of ['udp', 'tcp']) {
  test(`host and guest connect through the TURN relay over ${transport}`, async ({ browser }) => {
    const host = await (await browser.newContext()).newPage();
    const guest = await (await browser.newContext()).newPage();
    for (const p of [host, guest]) { await p.goto(`/e2e/page.html?relay=${transport}`); await p.waitForFunction(() => window.ready); }

    const code = await host.evaluate(async () => {
      window.hs = make();
      window.room = await hs.createRoom({ public: true });
      room.on('peer', peer => { window.peer = peer; listen(peer); });
      return room.code;
    });
    await guest.evaluate(async code => {
      window.hs = make();
      window.room = await hs.joinRoom(code);
      await new Promise(resolve => room.on('peer', peer => { window.peer = peer; listen(peer); resolve(); }));
    }, code);
    await host.waitForFunction(() => window.peer?.open);

    await guest.evaluate(() => { for (let i = 0; i < 20; i++) peer.send(new Uint8Array([4, 5, 6])); });
    await host.waitForFunction(() => got.some(m => !m.reliable && m.data.join() === '4,5,6'));
    await host.evaluate(() => peer.send({ via: 'relay' }, { reliable: true }));
    await guest.waitForFunction(() => got.some(m => m.reliable && m.data.via === 'relay'));
    for (const p of [host, guest]) await p.waitForFunction(() => peer.connectionType === 'relayed');

    for (const p of [host, guest]) await p.evaluate(() => hs.close());
  });
}
