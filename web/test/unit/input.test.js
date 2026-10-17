import test from 'node:test';
import assert from 'node:assert/strict';
import { videoPoint, wheelToDelta } from '../../js/input.js';

test('videoPoint maps through letterboxing', () => {
  // 16:9 video in a 1000x1000 box: picture is 1000x562.5, top bar 218.75.
  const rect = { left: 0, top: 0, width: 1000, height: 1000 };
  assert.equal(videoPoint(500, 100, rect, 1920, 1080), null, 'in the black bar');
  const p = videoPoint(500, 500, rect, 1920, 1080);
  assert.ok(Math.abs(p.x - 0.5) < 1e-9 && Math.abs(p.y - 0.5) < 1e-9);
  const c = videoPoint(0, 218.75, rect, 1920, 1080);
  assert.ok(Math.abs(c.x) < 1e-9 && Math.abs(c.y) < 1e-9);
  assert.equal(videoPoint(1, 1, rect, 0, 0), null);
});

test('wheel converts to WHEEL_DELTA with Windows sign', () => {
  assert.deepEqual(wheelToDelta({ deltaMode: 0, deltaX: 0, deltaY: 100 }), { dx: 0, dy: -120 });
  assert.deepEqual(wheelToDelta({ deltaMode: 1, deltaX: 3, deltaY: -3 }), { dx: 120, dy: 120 });
});

import { defaultDeviceName } from '../../js/pairing.js';
test('device name from user agent', () => {
  assert.equal(defaultDeviceName('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36'), 'Chrome on Windows');
  assert.equal(defaultDeviceName('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/141.0 Safari/537.36 Edg/141.0'), 'Edge on macOS');
});

import { pairingError } from '../../js/pairing.js';
test('pairing error messages', () => {
  assert.match(pairingError(403, { error: 'pairing_closed' }), /Open pairing on the host/);
  assert.match(pairingError(423, { error: 'locked' }), /Reopen pairing/);
  assert.match(pairingError(500), /Pairing failed \(500\)/);
});
