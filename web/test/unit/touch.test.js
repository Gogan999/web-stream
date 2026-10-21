import test from 'node:test';
import assert from 'node:assert/strict';
import { TouchGestures, HOLD_MS, TAP_DRAG_MS, SECOND_FINGER_MS } from '../../js/touch.js';

function rig(mode, zoomed = false) {
  const log = [];
  const g = new TouchGestures({
    move: (dx, dy) => log.push(['move', dx, dy]),
    moveTo: (x, y, reliable) => log.push([reliable ? 'TO' : 'to', x, y]), // TO: on the reliable channel
    button: (b, down) => log.push([down ? 'down' : 'up', b]),
    wheel: (dx, dy) => log.push(['wheel', dx, dy]),
    haptic: () => log.push(['haptic']),
    pinch: (ratio, x, y, dx, dy) => log.push(['pinch', ratio, x, y, dx, dy]),
    zoomed: () => zoomed,
  }, mode);
  return { g, log };
}

test('trackpad: tap clicks, drag moves without clicking', () => {
  const { g, log } = rig('trackpad');
  g.down(1, 100, 100, 0);
  g.move(1, 103, 101);
  g.up(1, 80);
  assert.deepEqual(log, [['down', 0], ['up', 0]]);
  log.length = 0;
  g.down(1, 100, 100, 1000);
  g.move(1, 115, 100); // past the slop: the whole distance so far is sent
  g.move(1, 120, 95);
  g.up(1, 1200);
  assert.deepEqual(log, [['move', 15, 0], ['move', 5, -5]]);
});

test('trackpad: hold then drag drags with the left button', () => {
  const { g, log } = rig('trackpad');
  g.down(1, 100, 100, 0);
  g.tick(HOLD_MS - 1);
  assert.deepEqual(log, []);
  g.tick(HOLD_MS);
  g.move(1, 100, 130);
  g.up(1, 900);
  assert.deepEqual(log, [['haptic'], ['down', 0], ['move', 0, 30], ['up', 0]]);
});

test('two-finger tap right-clicks; two-finger drag scrolls like a touch screen', () => {
  const { g, log } = rig('trackpad');
  g.down(1, 100, 100, 0);
  g.down(2, 200, 100, 20);
  g.up(1, 120);
  g.up(2, 130);
  assert.deepEqual(log, [['down', 2], ['up', 2]]);
  log.length = 0;
  g.down(1, 100, 100, 1000);
  g.down(2, 200, 100, 1010);
  // Both fingers down 30 px: content follows the fingers, which is wheel up (+).
  for (let y = 105; y <= 130; y += 5) { g.move(1, 100, y); g.move(2, 200, y); }
  g.up(1, 1500);
  g.up(2, 1510);
  const wheel = log.filter((e) => e[0] === 'wheel').reduce((s, e) => s + e[2], 0);
  assert.ok(wheel > 0, JSON.stringify(log));
  assert.ok(!log.some((e) => e[0] === 'down' || e[0] === 'move'));
  // The lift after scrolling doesn't click, and a lone finger left over doesn't start moving.
  log.length = 0;
  g.down(1, 100, 100, 2000);
  g.down(2, 200, 100, 2010);
  g.up(2, 2400);
  g.move(1, 150, 150);
  g.up(1, 2500);
  assert.deepEqual(log, []);
});

test('direct: cursor follows the finger; tap clicks there, drag drags, hold right-clicks', () => {
  const { g, log } = rig('direct');
  g.down(1, 10, 20, 0);
  g.up(1, 50);
  assert.deepEqual(log, [['TO', 10, 20], ['down', 0], ['up', 0]]);
  log.length = 0;
  g.down(1, 10, 20, 1000);
  g.move(1, 40, 20);
  g.up(1, 1200);
  assert.deepEqual(log, [['TO', 10, 20], ['TO', 10, 20], ['down', 0], ['to', 40, 20], ['up', 0]]);
  log.length = 0;
  g.down(1, 10, 20, 2000);
  g.tick(2000 + HOLD_MS);
  g.up(1, 2600);
  assert.deepEqual(log, [['TO', 10, 20], ['haptic'], ['down', 2], ['up', 2]]);
});

test('cancel lets go of a held button; a second finger mid-drag is ignored', () => {
  const { g, log } = rig('direct');
  g.down(1, 0, 0, 0);
  g.move(1, 50, 0);
  g.down(2, 300, 300, 100);
  g.move(2, 320, 300);
  g.cancel();
  assert.deepEqual(log, [['TO', 0, 0], ['TO', 0, 0], ['down', 0], ['to', 50, 0], ['up', 0]]);
});

test('trackpad: tap then touch again and drag drags with the left button, moving before the press', () => {
  const { g, log } = rig('trackpad');
  g.down(1, 100, 100, 0);
  g.up(1, 60);
  log.length = 0;
  g.down(1, 100, 100, 60 + TAP_DRAG_MS - 1);
  g.move(1, 120, 100);
  g.move(1, 140, 110);
  g.up(1, 900);
  assert.deepEqual(log, [['move', 20, 0], ['down', 0], ['move', 20, 10], ['up', 0]]);
  // Too late after the tap: an ordinary move.
  log.length = 0;
  g.down(1, 100, 100, 1000);
  g.up(1, 1050);
  log.length = 0;
  g.down(1, 100, 100, 1050 + TAP_DRAG_MS + 1);
  g.move(1, 120, 100);
  g.up(1, 2000);
  assert.deepEqual(log, [['move', 20, 0]]);
  // Two quick taps double-click; a third quick touch doesn't drag.
  log.length = 0;
  g.down(1, 100, 100, 3000);
  g.up(1, 3050);
  g.down(1, 100, 100, 3150);
  g.up(1, 3200);
  g.down(1, 100, 100, 3300);
  g.move(1, 130, 100);
  g.up(1, 3500);
  assert.deepEqual(log, [['down', 0], ['up', 0], ['down', 0], ['up', 0], ['move', 30, 0]]);
});

test('pinch zooms around the midpoint and pans with it; no scroll or click', () => {
  const { g, log } = rig('direct');
  g.down(1, 100, 100, 0);
  g.down(2, 200, 100, 10);
  log.length = 0;
  for (let i = 1; i <= 5; i++) { g.move(1, 100 - i * 10, 100); g.move(2, 200 + i * 10, 100); }
  g.up(1, 500);
  g.up(2, 510);
  const pinches = log.filter((e) => e[0] === 'pinch');
  assert.deepEqual(log.filter((e) => e[0] !== 'pinch'), []);
  const ratio = pinches.reduce((r, e) => r * e[1], 1);
  assert.ok(Math.abs(ratio - 2) < 1e-9, String(ratio)); // spread 100 -> 200
  assert.ok(pinches.every((e) => Math.abs(e[2] - 150) <= 5 && e[3] === 100));
});

test('zoomed in, a two-finger drag pans the view instead of scrolling', () => {
  const { g, log } = rig('trackpad', true);
  g.down(1, 100, 100, 0);
  g.down(2, 200, 100, 10);
  for (let y = 105; y <= 130; y += 5) { g.move(1, 100, y); g.move(2, 200, y); }
  g.up(1, 500);
  g.up(2, 510);
  assert.ok(log.length && log.every((e) => e[0] === 'pinch'), JSON.stringify(log));
  const pan = log.reduce((s, e) => s + e[5], 0);
  assert.equal(pan, 30);
});

test('a pinch whose first finger slides before the second lands still pinches, not drags', () => {
  for (const mode of ['direct', 'trackpad']) {
    const { g, log } = rig(mode);
    if (mode === 'trackpad') { g.down(1, 100, 100, 0); g.up(1, 50); } // a tap first: the touch would tap-drag
    log.length = 0;
    const t = 100;
    g.down(1, 100, 100, t);
    g.move(1, 80, 100, t + 30); // past the slop
    g.down(2, 200, 100, t + 60);
    for (let i = 1; i <= 5; i++) { g.move(1, 80 - i * 10, 100, t + 60 + i); g.move(2, 200 + i * 10, 100, t + 60 + i); }
    g.up(1, t + 300);
    g.up(2, t + 310);
    const other = log.filter((e) => e[0] !== 'pinch' && e[0] !== 'TO');
    assert.deepEqual(other, [], mode);
    assert.ok(log.some((e) => e[0] === 'pinch'), mode);
  }
});

test('direct: a drag waits for a second finger at most SECOND_FINGER_MS, from where it landed', () => {
  const { g, log } = rig('direct');
  g.down(1, 10, 20, 0);
  g.move(1, 40, 20, 50);
  assert.deepEqual(log, [['TO', 10, 20]]);
  g.tick(SECOND_FINGER_MS); // the finger stopped: the timer starts it
  g.move(1, 60, 20, SECOND_FINGER_MS + 10);
  g.up(1, 400);
  assert.deepEqual(log, [['TO', 10, 20], ['TO', 10, 20], ['down', 0], ['to', 40, 20], ['to', 60, 20], ['up', 0]]);
  // A quick flick, lifted before the wait is over, is still a drag rather than a click.
  log.length = 0;
  g.down(1, 10, 20, 1000);
  g.move(1, 60, 20, 1040);
  g.up(1, 1060);
  assert.deepEqual(log, [['TO', 10, 20], ['TO', 10, 20], ['down', 0], ['to', 60, 20], ['up', 0]]);
  // No hold right-click for a finger that slid.
  log.length = 0;
  g.down(1, 10, 20, 2000);
  g.move(1, 60, 20, 2010);
  g.tick(2000 + HOLD_MS);
  g.up(1, 2600);
  assert.ok(!log.some((e) => e[0] === 'down' && e[1] === 2), JSON.stringify(log));
});

test('lifting one finger of a pinch and putting it back keeps pinching', () => {
  const { g, log } = rig('direct');
  g.down(1, 100, 100, 0);
  g.down(2, 200, 100, 10);
  for (let i = 1; i <= 5; i++) { g.move(1, 100 - i * 10, 100); g.move(2, 200 + i * 10, 100); }
  g.up(2, 300);
  g.move(1, 40, 100); // the finger left over does nothing on its own
  log.length = 0;
  g.down(2, 200, 100, 400);
  g.move(2, 202, 100); // under the pinch threshold, but this is a pinch already
  g.up(1, 500);
  g.up(2, 510);
  assert.deepEqual(log.map((e) => e[0]), ['pinch']);
});
