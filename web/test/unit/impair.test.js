import test from 'node:test';
import assert from 'node:assert/strict';
import dgram from 'node:dgram';
import { createShaper, startImpairment } from '../lib/impair.js';

// An echo server stands in for the host; the proxy sits in front of it.
async function setup(t) {
  const echo = dgram.createSocket('udp4');
  await new Promise((r) => echo.bind(0, '127.0.0.1', r));
  echo.on('message', (m, from) => echo.send(m, from.port, from.address));
  const net = await startImpairment({ port: echo.address().port });
  const client = dgram.createSocket('udp4');
  await new Promise((r) => client.bind(0, '127.0.0.1', r));
  const got = [];
  client.on('message', (m) => got.push({ n: m.readUInt32BE(0), at: performance.now() }));
  t.after(() => { net.close(); echo.close(); client.close(); });
  const send = (n, size = 100) => {
    const b = Buffer.alloc(size);
    b.writeUInt32BE(n);
    client.send(b, net.port, '127.0.0.1');
  };
  return { net, send, got };
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// Waits until nothing more arrives for a while: loopback UDP is slow on some sandboxes.
async function settle(got) {
  await sleep(500);
  for (let n = -1; n !== got.length;) {
    n = got.length;
    await sleep(1000);
  }
}

test('passes packets through, delayed both ways', async (t) => {
  const { net, send, got } = await setup(t);
  net.set({ delayMs: 30 });
  const start = performance.now();
  send(1);
  await settle(got);
  assert.equal(got.length, 1);
  assert.ok(got[0].at - start >= 55, `round trip ${got[0].at - start} ms`);
});

test('drops about the share asked for, and everything in an outage', () => {
  const sh = createShaper();
  sh.set({ loss: 0.2 });
  let lost = 0;
  for (let i = 0; i < 1000; i++) if (sh.decide('down', 100, i).drop) lost++;
  assert.ok(lost > 160 && lost < 240, `${lost} of 1000`);
  sh.set({ outage: true });
  assert.equal(sh.decide('up', 100, 2000).drop, 'loss');
  sh.set({});
  assert.deepEqual(sh.decide('up', 100, 3000), { at: 3000 });
});

test('a rate limit queues, then drops what would wait too long', () => {
  const sh = createShaper();
  // 1000 B packets at 800 kb/s: 10 ms each; a 100 ms queue holds about 10 of a burst.
  sh.set({ rateKbps: 800, queueMs: 100, delayMs: 5 });
  const out = Array.from({ length: 50 }, () => sh.decide('down', 1000, 0));
  const sent = out.filter((d) => !d.drop);
  assert.equal(sent.length, 11);
  assert.deepEqual(sent.slice(0, 3).map((d) => d.at), [15, 25, 35]);
  assert.ok(out.slice(11).every((d) => d.drop === 'queue'));
  // The rate limit is only towards the browser.
  assert.deepEqual(sh.decide('up', 1000, 0), { at: 5 });
});

test('jitter never reorders', () => {
  const sh = createShaper(7);
  sh.set({ delayMs: 20, jitterMs: 15 });
  const at = Array.from({ length: 200 }, (_, i) => sh.decide('down', 100, i).at);
  assert.deepEqual(at, [...at].sort((a, b) => a - b));
});
