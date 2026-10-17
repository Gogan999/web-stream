import test from 'node:test';
import assert from 'node:assert/strict';
import { InputCapture } from '../../js/input.js';
import { decode } from '../../js/protocol.js';

// Fake session whose input-rt channel can be toggled between backed up and free.
function fakeSession() {
  const sent = [];
  return {
    sent, blocked: false,
    canSend(label) { return label !== 'input-rt' || !this.blocked; },
    sendInput(label, buf) { if (!this.canSend(label)) return false; sent.push({ label, ...decode(buf) }); return true; },
  };
}

test('relative motion accumulates while backed up and is sent as one sum', () => {
  const session = fakeSession();
  const ic = new InputCapture({ session, video: {}, stage: {} });
  session.blocked = true;
  ic.motion.dx += 5; ic.motion.dy -= 2; ic.flushMotion();
  ic.motion.dx += 7; ic.motion.dy -= 3; ic.flushMotion();
  assert.equal(session.sent.length, 0);
  session.blocked = false;
  ic.flushMotion();
  assert.deepEqual(session.sent, [{ label: 'input-rt', type: 'mouse-rel', dx: 12, dy: -5 }]);
});

test('large sums split into i16-sized messages', () => {
  const session = fakeSession();
  const ic = new InputCapture({ session, video: {}, stage: {} });
  ic.motion.dx = 40000;
  ic.flushMotion();
  assert.deepEqual(session.sent.map((m) => m.dx), [32767, 7233]);
});

test('absolute keeps only the latest point while backed up', () => {
  const session = fakeSession();
  const ic = new InputCapture({ session, video: {}, stage: {} });
  session.blocked = true;
  ic.motion.abs = { x: 0.1, y: 0.1 }; ic.flushMotion();
  ic.motion.abs = { x: 0.5, y: 0.5 }; ic.flushMotion();
  session.blocked = false;
  ic.flushMotion();
  assert.equal(session.sent.length, 1);
  assert.equal(session.sent[0].x, 32768);
});
