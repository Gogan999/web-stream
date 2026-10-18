import test from 'node:test';
import assert from 'node:assert/strict';
import { toXInput, sameState, XI } from '../../js/gamepad.js';

const pad = (pressed = [], axes = [0, 0, 0, 0], lt = 0, rt = 0) => ({
  buttons: Array.from({ length: 17 }, (_, i) => ({
    pressed: pressed.includes(i), value: i === 6 ? lt : i === 7 ? rt : pressed.includes(i) ? 1 : 0,
  })),
  axes,
});

test('standard buttons map to XInput flags', () => {
  assert.equal(toXInput(pad([0])).buttons, XI.A);
  assert.equal(toXInput(pad([3, 12, 9])).buttons, XI.Y | XI.DPAD_UP | XI.START);
  assert.equal(toXInput(pad([6, 7])).buttons, 0, 'triggers are analog, not flags');
});

test('axes flip Y to XInput convention and triggers scale to u8', () => {
  const s = toXInput(pad([], [1, 1, -1, -1], 1, 0.5));
  assert.equal(s.lx, 32767);
  assert.equal(s.ly, -32767);
  assert.equal(s.rx, -32767);
  assert.equal(s.ry, 32767);
  assert.equal(s.lt, 255);
  assert.equal(s.rt, 128);
});

test('sameState', () => {
  assert.ok(sameState(toXInput(pad([1])), toXInput(pad([1]))));
  assert.ok(!sameState(toXInput(pad([1])), toXInput(pad([2]))));
  assert.ok(!sameState(null, toXInput(pad())));
});
