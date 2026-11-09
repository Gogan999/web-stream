// The media path end to end, measured from the browser: the test host's synthetic source (fake
// displays whose frames carry a stamp, software H.264, a beep every second for game audio) into
// Google Chrome, which reads every frame it shows (lib/probe.js). Checks frame rate, latency, lost
// frames and freezes, audio latency and A/V sync, the mic reaching the host, and switching displays.
//
//   cargo build -p ws-host --example test-host --features synthetic
//   WS_HOST_BIN=../target/debug/examples/test-host npm run test:media
//
// MEDIA_REPORT=<file> writes the figures as JSON. Limits are deliberately loose: a CI runner
// encodes in software on 4 cores. They catch a broken path, not a few milliseconds.
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname } from 'node:path';
import { bin, collect, connect, launchBrowser, now, startHost } from '../lib/host.js';
import { summarize } from '../lib/metrics.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const report = {};

test.after(async () => {
  if (process.env.MEDIA_REPORT) await mkdir(dirname(process.env.MEDIA_REPORT), { recursive: true });
  if (process.env.MEDIA_REPORT) await writeFile(process.env.MEDIA_REPORT, JSON.stringify(report, null, 2));
});

test('synthetic stream: rate, latency, audio, mic and display switching', { skip: !bin && 'WS_HOST_BIN not set', timeout: 180000 }, async (t) => {
  const host = await startHost(t, ['--synthetic=3']);
  const browser = await launchBrowser(t);
  const { page, errors } = await connect(browser, host, { settings: { resolution: '720', fps: 60, mic: true } });

  // A steady 10 seconds.
  await sleep(2000);
  const from = await now(page);
  await sleep(10000);
  const steady = summarize(await collect(page), from, await now(page));
  report.steady = { ...steady, browser: await page.evaluate(() => window.__probe.stats()) };
  t.diagnostic(`steady: ${JSON.stringify(steady)}`);

  assert.equal(steady.size, '1280x720');
  assert.deepEqual(steady.displays, [0], 'the primary display');
  assert.ok(steady.fps >= 24, `fps ${steady.fps}`);
  assert.ok(steady.latencyMs.p50 < 120, `latency p50 ${steady.latencyMs.p50} ms`);
  assert.ok(steady.latencyMs.p95 < 250, `latency p95 ${steady.latencyMs.p95} ms`);
  assert.ok(steady.lostPct < 5, `${steady.lostPct}% of frames lost`);
  assert.ok(steady.longestFreezeMs < 500, `froze for ${steady.longestFreezeMs} ms`);
  assert.equal(steady.unreadable, 0, 'every frame read');

  assert.ok(steady.beeps >= 7, `heard ${steady.beeps} beeps in 10 s`);
  assert.ok(steady.audioLatencyMs.p50 < 300, `audio latency p50 ${steady.audioLatencyMs.p50} ms`);
  assert.ok(Math.abs(steady.avOffsetMs) < 200, `A/V offset ${steady.avOffsetMs} ms`);

  // The browser's fake mic (a beep) reached the host's mic meter.
  const [, peak] = await host.says(/synthetic mic: peak (-?[\d.]+) dBFS/, 0, 10000);
  const loud = [...host.out.matchAll(/synthetic mic: peak (-?[\d.]+) dBFS/g)].map((m) => Number(m[1]));
  report.micPeakDbfs = loud;
  assert.ok(Math.max(...loud) > -40, `mic peak ${Math.max(...loud)} dBFS (first ${peak})`);

  // Switching displays: the picture changes over quickly, without a long freeze.
  report.switches = [];
  await page.click('#more-toggle');
  for (const id of [1, 2, 0]) {
    const at = await now(page);
    await page.selectOption('#display', String(id));
    await page.waitForFunction(([id, at]) => window.__probe.frames.some((f) => f.display === id && f.shown > at), [id, at], { timeout: 10000 });
    await sleep(1500);
    const data = await collect(page);
    const first = data.frames.find((f) => f.display === id && f.shown > at);
    const after = summarize(data, first.shown, first.shown + 1500);
    const sw = { to: id, switchMs: Math.round(first.shown - at), after };
    report.switches.push(sw);
    t.diagnostic(`switch: ${JSON.stringify(sw)}`);
    assert.ok(sw.switchMs < 1500, `display ${id} showed after ${sw.switchMs} ms`);
    if (after.displays.length !== 1) {
      // Show the order the browser presented things in around the switch, to tell a late frame
      // from the old display from a mis-read.
      const around = data.frames.filter((f) => f.shown > at - 100 && f.shown < first.shown + 400)
        .map((f) => (f.bad ? 'bad' : `d${f.display}#${f.frame}@${Math.round(f.shown - at)}p${f.presented}`));
      t.diagnostic(`switch to ${id} at 0, first new frame ${Math.round(first.shown - at)} ms: ${around.join(' ')}`);
    }
    assert.deepEqual(after.displays, [id], `only display ${id} after the switch`);
    assert.ok(after.fps >= 20, `fps ${after.fps} after switching to display ${id}`);
  }

  assert.deepEqual(errors, []);
});
