import test from 'node:test';
import assert from 'node:assert/strict';
import { InputCapture, MOTION_GAP_MS } from '../../js/input.js';
import { decode } from '../../js/protocol.js';

// Fake session whose input-rt channel can be toggled between backed up and free.
function fakeSession() {
  const sent = [];
  return {
    sent, blocked: false,
    canSend(label) { return label !== 'input-motion' || !this.blocked; },
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
  assert.deepEqual(session.sent, [{ label: 'input-motion', type: 'mouse-rel', dx: 12, dy: -5 }]);
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

test('mouse motion faster than MOTION_GAP_MS is summed, then sent', async () => {
  const session = fakeSession();
  const ic = new InputCapture({ session, video: {}, stage: {} });
  Object.defineProperty(ic, 'locked', { value: true });
  ic.onMouseMove({ movementX: 3, movementY: 1 });
  ic.onMouseMove({ movementX: 4, movementY: 1 });
  ic.onMouseMove({ movementX: 5, movementY: 1 });
  assert.deepEqual(session.sent.map((m) => m.dx), [3], 'only the first goes out at once');
  await new Promise((r) => setTimeout(r, MOTION_GAP_MS + 10));
  assert.deepEqual(session.sent.map((m) => [m.dx, m.dy]), [[3, 1], [9, 2]]);
});

test('an absolute click sends the pointer position before the button', () => {
  const session = fakeSession();
  const video = { getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }), videoWidth: 100, videoHeight: 100 };
  const ic = new InputCapture({ session, video, stage: {} });
  Object.defineProperty(ic, 'locked', { value: false });
  ic.mode = 'absolute';
  ic.stageFocused = true;
  ic.motionSentAt = performance.now(); // motion just went out, so this move alone would wait
  ic.onMouseDown({ clientX: 50, clientY: 50, button: 0, preventDefault() {} });
  assert.deepEqual(session.sent.map((m) => m.type), ['mouse-abs', 'mouse-button']);
  clearTimeout(ic.motionTimer);
});

test('a button waits for no motion: pending motion goes out first', () => {
  const session = fakeSession();
  const ic = new InputCapture({ session, video: {}, stage: {} });
  Object.defineProperty(ic, 'locked', { value: true });
  ic.onMouseMove({ movementX: 1, movementY: 0 });
  ic.onMouseMove({ movementX: 2, movementY: 0 }); // held back by MOTION_GAP_MS
  ic.onMouseDown({ button: 0, preventDefault() {} });
  assert.deepEqual(session.sent.map((m) => [m.type, m.dx]), [['mouse-rel', 1], ['mouse-rel', 2], ['mouse-button', undefined]]);
  clearTimeout(ic.motionTimer);
});
