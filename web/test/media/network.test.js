// The stream on a bad network: the browser's media goes through lib/impair.js, which adds delay,
// jitter, loss, a bandwidth limit with a queue, or an outage. Checks what the host is built for:
// old frames are thrown away instead of queueing up (latency stays bounded under a slow link), the
// picture keeps moving under loss, and it comes back on its own after an outage.
//
//   cargo build -p ws-host --example test-host --features synthetic
//   WS_HOST_BIN=../target/debug/examples/test-host npm run test:media
//
// MEDIA_REPORT=<file> writes the figures as JSON. Limits are loose on purpose (see stream.test.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname } from 'node:path';
import { bin, collect, connect, launchBrowser, now, startHost } from '../lib/host.js';
import { startImpairment } from '../lib/impair.js';
import { summarize } from '../lib/metrics.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function session(t, args = []) {
  const host = await startHost(t, ['--synthetic=2', ...args]);
  const net = await startImpairment({ port: host.port });
  t.after(() => net.close());
  const browser = await launchBrowser(t);
  const { page } = await connect(browser, host, { proxyPort: net.port, settings: { resolution: '720', fps: 60, bitrate: '8000' } });
  // Measures `ms` of the stream under the profile now set, after `settleMs` to let it adapt.
  const measure = async (ms, settleMs = 3000) => {
    await sleep(settleMs);
    const from = await now(page);
    await sleep(ms);
    return summarize(await collect(page), from, await now(page));
  };
  return { host, net, page, measure };
}

const log = async (name, figures) => {
  const base = process.env.MEDIA_REPORT;
  if (!base) return;
  await mkdir(dirname(base), { recursive: true });
  await writeFile(base.replace(/\.json$/, `-${name}.json`), JSON.stringify(figures, null, 2));
};

test('latency and loss: delay, jitter and 2% packet loss', { skip: !bin && 'WS_HOST_BIN not set', timeout: 120000 }, async (t) => {
  const { net, measure } = await session(t);
  const clean = await measure(5000, 2000);
  t.diagnostic(`clean: ${JSON.stringify(clean)}`);
  assert.ok(clean.fps >= 24, `clean fps ${clean.fps}`);

  net.set({ delayMs: 30, jitterMs: 10, loss: 0.02 });
  const bad = await measure(10000);
  await log('loss', { clean, bad, proxy: net.stats() });
  t.diagnostic(`2% loss, 60 ms rtt: ${JSON.stringify(bad)} proxy ${JSON.stringify(net.stats())}`);
  assert.ok(net.stats().down.lost > 0, 'the proxy dropped packets');
  assert.ok(bad.fps >= 15, `fps ${bad.fps} under loss`);
  // The delay is 60 ms round trip; the rest is retransmission and the jitter buffer.
  assert.ok(bad.latencyMs.p50 < 250, `latency p50 ${bad.latencyMs.p50} ms`);
  assert.ok(bad.latencyMs.p95 < 600, `latency p95 ${bad.latencyMs.p95} ms`);
  assert.ok(bad.longestFreezeMs < 1500, `froze for ${bad.longestFreezeMs} ms`);
});

test('a link slower than the stream: frames are dropped, not queued', { skip: !bin && 'WS_HOST_BIN not set', timeout: 120000 }, async (t) => {
  const { net, measure } = await session(t);
  await measure(2000, 1000);
  // 8 Mb/s asked for, 2.5 Mb/s on offer, with a 150 ms queue like a router's.
  net.set({ delayMs: 15, rateKbps: 2500, queueMs: 150 });
  const slow = await measure(12000, 6000);
  await log('slow', { slow, proxy: net.stats() });
  t.diagnostic(`2.5 Mb/s link: ${JSON.stringify(slow)} proxy ${JSON.stringify(net.stats())}`);
  assert.ok(slow.fps >= 10, `fps ${slow.fps}`);
  // A queue that grew without bound would show as latency climbing past the queue's own 150 ms.
  assert.ok(slow.latencyMs.p95 < 700, `latency p95 ${slow.latencyMs.p95} ms: frames are queueing`);
  assert.ok(slow.longestFreezeMs < 2000, `froze for ${slow.longestFreezeMs} ms`);
});

test('an outage: the picture comes back by itself', { skip: !bin && 'WS_HOST_BIN not set', timeout: 120000 }, async (t) => {
  const { net, page, measure } = await session(t);
  await measure(2000, 1000);
  const start = await now(page);
  net.set({ outage: true });
  await sleep(3000);
  net.set({});
  const back = await now(page);
  // The first frame shown after the link is back.
  await page.waitForFunction((back) => window.__probe.frames.some((f) => !f.bad && f.shown > back), back, { timeout: 20000 });
  const data = await collect(page);
  const first = data.frames.find((f) => !f.bad && f.shown > back);
  const recoveryMs = Math.round(first.shown - back);
  const after = summarize(data, first.shown + 2000, first.shown + 6000);
  await sleep(6500);
  const settled = summarize(await collect(page), first.shown + 2000, first.shown + 6000);
  await log('outage', { start, recoveryMs, settled, proxy: net.stats() });
  t.diagnostic(`outage 3 s: first frame ${recoveryMs} ms after the link came back; then ${JSON.stringify(settled)} (${after.frames} so far)`);
  assert.ok(recoveryMs < 8000, `first frame ${recoveryMs} ms after the link was back`);
  assert.ok(settled.fps >= 20, `fps ${settled.fps} afterwards`);
  assert.ok(settled.latencyMs.p95 < 500, `latency p95 ${settled.latencyMs.p95} ms afterwards: the backlog was not thrown away`);
});
