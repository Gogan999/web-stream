import test from 'node:test';
import assert from 'node:assert/strict';
import { LIMITS, lossLevel, rttLevel, decodeSlow, encodeSlow, decodeLimitMs, moving, bandwidthShort, bitrateReduced } from '../../js/health.js';
import { problem } from '../../js/autotune.js';
import { assess } from '../../js/stats.js';

test('loss and round trip levels', () => {
  assert.equal(lossLevel(null), null);
  assert.equal(lossLevel(0.9), null);
  assert.equal(lossLevel(1), 'warn');
  assert.equal(lossLevel(5), 'bad');
  assert.equal(rttLevel(79), null);
  assert.equal(rttLevel(80), 'warn');
  assert.equal(rttLevel(150), 'bad');
});

test('decode and encode are judged against the frame interval', () => {
  assert.ok(Math.abs(decodeLimitMs(60) - 10) < 0.01);
  assert.equal(decodeSlow(9, 60), false);
  assert.equal(decodeSlow(11, 60), true);
  assert.equal(decodeSlow(null, 60), false);
  assert.equal(encodeSlow(16, 60), false);
  assert.equal(encodeSlow(17, 60), true);
  assert.equal(decodeSlow(5, 0), false);
});

test('bandwidth counts only while the picture moves', () => {
  assert.equal(moving(60, 60), true);
  assert.equal(moving(10, 60), false);
  assert.equal(bandwidthShort(12000, 20000, true), true);
  assert.equal(bandwidthShort(17500, 20000, true), false);
  assert.equal(bandwidthShort(3000, 20000, false), false);
  assert.equal(bandwidthShort(null, 20000, true), false);
});

test('a low bitrate target is a limit only while the encoder uses it', () => {
  assert.equal(bitrateReduced({ encoderKbps: 20000, maxKbps: 20000 }), null);
  assert.equal(bitrateReduced({ encoderKbps: 10000, maxKbps: 20000, sentKbps: 9500 }), 'warn');
  assert.equal(bitrateReduced({ encoderKbps: 4000, maxKbps: 20000, sentKbps: 3900 }), 'bad');
  assert.equal(bitrateReduced({ encoderKbps: 4000, maxKbps: 20000, sentKbps: 900 }), null);
  assert.equal(bitrateReduced(null), null);
});

// The overlay and auto-tune must agree: the same numbers are a problem in both, or in neither.
test('stats overlay and auto-tune use the same limits', () => {
  const config = { fps: 60, width: 1920, height: 1080, bitrateKbps: 20000 };
  const clean = { freezes: 0, keyframeRequests: 0, loss: 0, bweKbps: null, hostFps: 60, decodeMs: 2, encodeMs: 3 };
  const s = { width: 1920, height: 1080, fps: 60, rttMs: 4, lossPct: 0, droppedDelta: 0, freezesDelta: 0, pliDelta: 0, decodeMs: 2 };
  const host = { encoderKbps: 20000, maxKbps: 20000, fps: 60, encodeMs: 3 };
  const cases = [
    ['loss just under', { loss: 0.0099 }, { lossPct: 0.99 }, {}, false],
    ['loss at the limit', { loss: 0.01 }, { lossPct: 1 }, {}, true],
    ['decode over the limit', { decodeMs: 11 }, { decodeMs: 11 }, {}, true],
    ['decode under the limit', { decodeMs: 9 }, { decodeMs: 9 }, {}, false],
    ['encode over a frame', { encodeMs: 18 }, {}, { encodeMs: 18 }, true],
    ['encode under a frame', { encodeMs: 14 }, {}, { encodeMs: 14 }, false],
  ];
  for (const [name, m, sv, hv, bad] of cases) {
    const tune = problem({ ...clean, ...m }, config) !== null;
    const overlay = assess({ ...s, ...sv }, { ...host, ...hv }, config).length > 0;
    assert.equal(tune, bad, `auto-tune: ${name}`);
    assert.equal(overlay, bad, `overlay: ${name}`);
  }
  assert.equal(LIMITS.keyframesTolerated, 1);
});
