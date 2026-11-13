import test from 'node:test';
import assert from 'node:assert/strict';
import { StallWatchdog } from '../../js/watchdog.js';

const v = (bytesReceived, framesDecoded) => ({ bytesReceived, framesDecoded });

test('a decoding stream never asks', () => {
  const w = new StallWatchdog();
  for (let t = 0; t < 20; t++) assert.equal(w.update(v(t * 1000, t * 60), t * 1000), false);
});

test('packets with no decoded frames ask after the stall time, then keep asking', () => {
  const w = new StallWatchdog({ stallMs: 2000, retryMs: 2000 });
  const asks = [];
  for (let t = 0; t <= 8; t++) if (w.update(v(t * 1000, 0), t * 1000)) asks.push(t);
  assert.deepEqual(asks, [3, 5, 7]);
});

test('a stream that recovers stops asking and starts over later', () => {
  const w = new StallWatchdog();
  let n = 0;
  for (let t = 0; t <= 3; t++) if (w.update(v(t * 1000, 0), t * 1000)) n++;
  assert.equal(n, 1);
  assert.equal(w.update(v(5000, 30), 4000), false);
  assert.equal(w.update(v(6000, 30), 5000), false);
  assert.equal(w.update(v(7000, 30), 6000), false);
});

test('no packets at all is not a stall, and a missing report resets', () => {
  const w = new StallWatchdog();
  for (let t = 0; t < 10; t++) assert.equal(w.update(v(0, 0), t * 1000), false);
  w.update(v(1, 0), 11000);
  w.update(null, 12000);
  assert.equal(w.since, null);
});

test('a hidden tab never asks and starts over when shown', () => {
  const w = new StallWatchdog();
  for (let t = 0; t < 10; t++) assert.equal(w.update(v(t * 1000, 0), t * 1000, true), false);
  assert.equal(w.update(v(10000, 0), 10000, false), false);
  assert.equal(w.update(v(11000, 0), 11000, false), false);
});
