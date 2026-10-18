import test from 'node:test';
import assert from 'node:assert/strict';
import { cssCursor, decodeImage, overlayBox } from '../../js/cursor.js';

test('standard shapes use the browser cursor; others the host image', () => {
  assert.equal(cssCursor('nwse-resize', null), 'nwse-resize');
  assert.equal(cssCursor('none', null), 'none');
  const image = { url: 'data:image/png;base64,AA', width: 32, height: 32, hotX: 3, hotY: 4 };
  assert.equal(cssCursor('ew-resize', image), 'ew-resize');
  assert.equal(cssCursor('custom', image), 'url("data:image/png;base64,AA") 3 4, default');
  assert.equal(cssCursor('custom', null), 'default');
  assert.equal(cssCursor('custom', { ...image, width: 200 }), 'default', 'too big for a CSS cursor');
  assert.equal(cssCursor('url(x)', null), 'default', 'unknown kinds are not passed to CSS');
});

test('cursor images are checked before use', () => {
  const rgba = Buffer.from([1, 2, 3, 4]).toString('base64');
  const img = decodeImage({ width: 1, height: 1, hotX: 0, hotY: 0, rgba });
  assert.deepEqual([...img.rgba], [1, 2, 3, 4]);
  assert.equal(decodeImage({ width: 2, height: 1, hotX: 0, hotY: 0, rgba }), null, 'wrong length');
  assert.equal(decodeImage({ width: 1, height: 1, hotX: 1, hotY: 0, rgba }), null, 'hotspot outside');
  assert.equal(decodeImage({ width: 1, height: 1, hotX: 0, hotY: 0, rgba: '!!' }), null, 'not base64');
  assert.equal(decodeImage({ width: 1000, height: 1, hotX: 0, hotY: 0, rgba }), null, 'too big');
});

test('overlay sits at the host position, scaled with the picture', () => {
  const image = { width: 32, height: 32, hotX: 0, hotY: 0 };
  const display = { width: 3840, height: 2160 };
  // 1920x1080 stream of a 4K display in a 1000x1000 box: picture 1000x562.5, top bar 218.75.
  const box = overlayBox({ x: 1920, y: 1080 }, image, display, { width: 1000, height: 1000 }, 1920, 1080);
  assert.ok(Math.abs(box.left - 500) < 1e-9 && Math.abs(box.top - 500) < 1e-9);
  assert.ok(Math.abs(box.width - 32 * 1000 / 3840) < 1e-9);
  // The hotspot lands on the position.
  const hot = overlayBox({ x: 0, y: 0 }, { ...image, hotX: 10, hotY: 10 }, display, { width: 1000, height: 1000 }, 1920, 1080);
  assert.ok(hot.left < 0 && hot.top < 218.75);
  assert.equal(overlayBox({ x: -1, y: 5 }, image, display, { width: 1000, height: 1000 }, 1920, 1080), null, 'on another display');
  assert.equal(overlayBox({ x: 5, y: 5 }, null, display, { width: 1000, height: 1000 }, 1920, 1080), null);
});
