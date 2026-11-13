import test from 'node:test';
import assert from 'node:assert/strict';
import { clientStats } from '../../js/telemetry.js';
import { assess } from '../../js/stats.js';

test('clientStats maps one interval to the message', () => {
  const m = clientStats({ fps: 59.4, decodeMs: 4.2, droppedDelta: 2, freezesDelta: 0 }, false);
  assert.deepEqual(m, { type: 'clientStats', fps: 59.4, decodeMs: 4.2, dropped: 2, freezes: 0, hidden: false });
});

test('clientStats leaves out figures that are not numbers', () => {
  const m = clientStats({ fps: null, decodeMs: NaN, droppedDelta: -3, freezesDelta: 1 }, true);
  assert.equal(m.fps, 0);
  assert.equal(m.decodeMs, null);
  assert.equal(m.dropped, 0);
  assert.equal(m.hidden, true);
});

test('assess names the limiter and what was done', () => {
  const s = { width: 1920, height: 1080, droppedDelta: 0, freezesDelta: 0, pliDelta: 0 };
  const out = assess(s, null, null, { limiter: 'hostEncode', detail: 'host encode takes 40.0 ms', scalePercent: 75, relieving: true });
  const w = out.find((a) => a.key === 'limiter');
  assert.match(w.text, /Limited by the host encoder/);
  assert.match(w.text, /75%/);
  assert.equal(assess(s, null, null, { limiter: 'none' }).some((a) => a.key === 'limiter'), false);
});

test('the reduced-resolution warning names the real cause', () => {
  const s = { width: 960, height: 536, droppedDelta: 0, freezesDelta: 0, pliDelta: 0 };
  const cfg = { width: 2560, height: 1440 };
  const b = { limiter: 'none', detail: '', scalePercent: 37, relieving: true, relievingFor: 'clientDecode' };
  assert.match(assess(s, null, cfg, b).find((a) => a.key === 'resolution').text, /for this device \(decode\)/);
  assert.match(assess(s, null, cfg, null).find((a) => a.key === 'resolution').text, /for the network/);
  const text = assess(s, null, cfg, { ...b, limiter: 'clientDecode' }).find((a) => a.key === 'limiter').text;
  assert.match(text, /Limited by this device \(decode\)/);
});
