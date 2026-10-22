import test from 'node:test';
import assert from 'node:assert/strict';
import * as P from '../../js/protocol.js';

test('mouse rel encodes little-endian i16 and clamps', () => {
  const b = new Uint8Array(P.mouseRel(-2, 300));
  assert.deepEqual([...b], [0x01, 0xfe, 0xff, 0x2c, 0x01]);
  assert.deepEqual(P.decode(P.mouseRel(99999, -99999)), { type: 'mouse-rel', dx: 32767, dy: -32768 });
});

test('mouse abs normalizes 0..1 to 0..65535', () => {
  assert.deepEqual(P.decode(P.mouseAbs(0, 1)), { type: 'mouse-abs', x: 0, y: 65535 });
  assert.deepEqual(P.decode(P.mouseAbs(0.5, 2)), { type: 'mouse-abs', x: 32768, y: 65535 });
});

test('button, wheel, key, release round-trip', () => {
  assert.deepEqual(P.decode(P.mouseButton(2, true)), { type: 'mouse-button', button: 2, down: true });
  assert.deepEqual(P.decode(P.wheel(0, -120)), { type: 'wheel', dx: 0, dy: -120 });
  assert.deepEqual([...new Uint8Array(P.key(0xe0, true))], [0x05, 0xe0, 0x00, 0x01]);
  assert.deepEqual(P.decode(P.key(0x04, false)), { type: 'key', usage: 0x04, down: false });
  assert.deepEqual(P.decode(P.releaseAll()), { type: 'release-all' });
  assert.deepEqual(P.decode(P.gamepadDisconnect(3)), { type: 'gamepad-disconnect', index: 3 });
});

test('gamepad is 14 bytes and round-trips', () => {
  const s = { buttons: 0x9001, lx: -32767, ly: 32767, rx: 5, ry: -5, lt: 255, rt: 0 };
  const buf = P.gamepad(1, s);
  assert.equal(buf.byteLength, 14);
  assert.deepEqual(P.decode(buf), { type: 'gamepad', index: 1, ...s });
});

test('numbered gamepad snapshots and button changes carry a u16 sequence', () => {
  const s = { buttons: 0x1000, lx: 1, ly: 2, rx: 3, ry: 4, lt: 5, rt: 6 };
  const motion = P.gamepad(2, s, 0x10001);
  assert.equal(motion.byteLength, 16);
  assert.deepEqual(P.decode(motion), { type: 'gamepad', index: 2, ...s, seq: 1 });
  const edge = P.gamepadButtons(2, s, 7);
  assert.equal(edge.byteLength, 16);
  assert.equal(new Uint8Array(edge)[0], 0x09);
  assert.deepEqual(P.decode(edge), { type: 'gamepad-buttons', index: 2, ...s, seq: 7 });
});

test('numbered absolute moves are 7 bytes', () => {
  const buf = P.mouseAbs(0, 1, 300);
  assert.equal(buf.byteLength, 7);
  assert.deepEqual(P.decode(buf), { type: 'mouse-abs', x: 0, y: 65535, seq: 300 });
});
