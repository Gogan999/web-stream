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
  assert.equal(defaultDeviceName('Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Mobile Safari/537.36'), 'Chrome on Android');
  assert.equal(defaultDeviceName('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'), 'Safari on iPhone');
});

import { pairingError } from '../../js/pairing.js';
test('pairing error messages', () => {
  assert.match(pairingError(403, { error: 'pairing_closed' }), /Open pairing on the host/);
  assert.match(pairingError(423, { error: 'locked' }), /Reopen pairing/);
  assert.match(pairingError(409, { error: 'busy' }), /Another pairing is in progress/);
  assert.match(pairingError(500), /Pairing failed \(500\)/);
});

import { pictureScale } from '../../js/input.js';
test('touch points clamp to the picture; trackpad scale is picture pixels per CSS pixel', () => {
  const rect = { left: 0, top: 0, width: 1000, height: 1000 };
  assert.deepEqual(videoPoint(500, 100, rect, 1920, 1080, true), { x: 0.5, y: 0 });
  assert.equal(pictureScale(rect, 1920, 1080), 1.92);
  assert.equal(pictureScale(rect, 0, 0), 1);
});

test('absolute points leave out bars the host drew for a display of another shape', async () => {
  const { videoPoint, pictureRect } = await import('../../js/input.js');
  // A 21:9 display streamed at 1920x1080: the host fits 2560x1080 into 1920x810, centred.
  const display = { width: 2560, height: 1080 };
  assert.deepEqual(pictureRect(1920, 1080, display), { x: 0, y: 134, width: 1920, height: 810 });
  const rect = { left: 0, top: 0, width: 1920, height: 1080 };
  assert.deepEqual(videoPoint(960, 134, rect, 1920, 1080, false, display), { x: 0.5, y: 0 });
  assert.equal(videoPoint(960, 100, rect, 1920, 1080, false, display), null);
  // Same shape: nothing changes.
  assert.deepEqual(pictureRect(1920, 1080, { width: 3840, height: 2160 }), { x: 0, y: 0, width: 1920, height: 1080 });
});
