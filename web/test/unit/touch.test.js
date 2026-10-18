import test from 'node:test';
import assert from 'node:assert/strict';
import { TouchGestures, HOLD_MS } from '../../js/touch.js';

function rig(mode) {
  const log = [];
  const g = new TouchGestures({
    move: (dx, dy) => log.push(['move', dx, dy]),
    moveTo: (x, y) => log.push(['to', x, y]),
    button: (b, down) => log.push([down ? 'down' : 'up', b]),
    wheel: (dx, dy) => log.push(['wheel', dx, dy]),
    haptic: () => log.push(['haptic']),
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
  assert.deepEqual(log, [['to', 10, 20], ['down', 0], ['up', 0]]);
  log.length = 0;
  g.down(1, 10, 20, 1000);
  g.move(1, 40, 20);
  g.up(1, 1200);
  assert.deepEqual(log, [['to', 10, 20], ['down', 0], ['to', 40, 20], ['up', 0]]);
  log.length = 0;
  g.down(1, 10, 20, 2000);
  g.tick(2000 + HOLD_MS);
  g.up(1, 2600);
  assert.deepEqual(log, [['to', 10, 20], ['haptic'], ['down', 2], ['up', 2]]);
});

test('cancel lets go of a held button; a second finger mid-drag is ignored', () => {
  const { g, log } = rig('direct');
  g.down(1, 0, 0, 0);
  g.move(1, 50, 0);
  g.down(2, 300, 300, 100);
  g.move(2, 320, 300);
  g.cancel();
  assert.deepEqual(log, [['to', 0, 0], ['down', 0], ['to', 50, 0], ['up', 0]]);
});
