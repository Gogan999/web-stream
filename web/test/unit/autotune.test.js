import test from 'node:test';
import assert from 'node:assert/strict';
import { AutoTune, Cancelled, problem } from '../../js/autotune.js';

const base = { codec: 'h264', width: 1920, height: 1080, fps: 60, bitrateKbps: 20000, quality: 'balanced' };
const clean = { freezes: 0, keyframeRequests: 0, loss: 0, bweKbps: null, hostFps: 60, decodeMs: 2, encodeMs: 3 };

test('problems in a measurement window', () => {
  assert.equal(problem(clean, base), null);
  assert.match(problem({ ...clean, freezes: 1 }, base), /froze/);
  assert.match(problem({ ...clean, keyframeRequests: 3 }, base), /dropped/);
  assert.match(problem({ ...clean, loss: 0.05 }, base), /5\.0% packet loss/);
  assert.match(problem({ ...clean, bweKbps: 12000 }, base), /12 Mb\/s/);
  assert.equal(problem({ ...clean, bweKbps: 19000 }, base), null);
  // A still screen sends little, and the estimate stays low: not a network problem.
  assert.equal(problem({ ...clean, bweKbps: 3000, hostFps: 10 }, base), null);
  assert.equal(problem({ ...clean, encodeMs: 12 }, base), null);
  assert.match(problem({ ...clean, encodeMs: 20 }, base), /encode too slow/);
  assert.equal(problem({ ...clean, encodeMs: 20 }, base, { encode: false }), null);
  assert.match(problem({ ...clean, decodeMs: 12 }, base), /decode too slow/);
});

// A fake link that carries up to `capKbps` and a host that encodes `clarity` too slowly.
function run(capKbps, slowClarity = false, still = false) {
  const applied = [];
  let current = base;
  const tune = new AutoTune({
    base,
    apply: (c) => { current = c; applied.push(c); },
    wait: async () => {},
    measure: async () => ({
      ...clean,
      bweKbps: still ? 2000 : Math.min(current.bitrateKbps, capKbps),
      hostFps: still ? 10 : 60,
      encodeMs: slowClarity && current.quality === 'clarity' ? 24 : 3,
    }),
  });
  return tune.run().then((r) => ({ ...r, applied, tune }));
}

test('keeps headroom below the highest bitrate that passed, then the best quality that keeps up', async () => {
  const r = await run(55000);
  assert.equal(r.config.bitrateKbps, 42500); // 50 Mb/s passed, 80 didn't; 85% of 50
  assert.equal(r.config.quality, 'clarity');
  assert.match(r.summary, /42\.5 Mb\/s, clarity/);
  assert.match(r.summary, /80 Mb\/s failed/);
  assert.deepEqual(r.applied.at(-1), r.config);
});

test('steps quality down when the encoder can not keep up', async () => {
  const r = await run(1_000_000, true);
  assert.equal(r.config.bitrateKbps, 127500);
  assert.equal(r.config.quality, 'balanced');
});

test('says so when the picture was still', async () => {
  const r = await run(3000, false, true);
  assert.equal(r.config.bitrateKbps, 127500);
  assert.match(r.summary, /mostly still/);
  assert.doesNotMatch((await run(55000)).summary, /mostly still/);
});

test('a slow encoder does not fail the bitrate steps', async () => {
  const applied = [];
  let current = base;
  const tune = new AutoTune({
    base,
    apply: (c) => { current = c; applied.push(c); },
    wait: async () => {},
    measure: async () => ({ ...clean, bweKbps: Math.min(current.bitrateKbps, 55000), encodeMs: 30 }),
  });
  const r = await tune.run();
  assert.equal(r.config.bitrateKbps, 42500);
  assert.equal(r.config.quality, 'speed');
});

test('falls back to the floor when nothing passes', async () => {
  const r = await run(3000);
  assert.equal(r.config.bitrateKbps, 5000);
});

test('can be cancelled', async () => {
  const tune = new AutoTune({ base, apply: () => {}, wait: async () => { tune.cancel(); }, measure: async () => clean });
  await assert.rejects(tune.run(), Cancelled);
});
