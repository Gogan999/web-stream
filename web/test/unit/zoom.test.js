import test from 'node:test';
import assert from 'node:assert/strict';
import { pinchView, followView, clampView, MAX_ZOOM } from '../../js/zoom.js';

const fit = { s: 1, x: 0, y: 0 };

test('pinch zooms around the pinch point and stays covering the stage', () => {
  // 2x around the middle of an 800x400 stage: the middle stays put.
  let v = pinchView(fit, 2, 400, 200, 0, 0, 800, 400);
  assert.deepEqual(v, { s: 2, x: -400, y: -200 });
  // Panned past the edge: clamped.
  v = pinchView(v, 1, 400, 200, 1000, 0, 800, 400);
  assert.deepEqual(v, { s: 2, x: 0, y: -200 });
  // Can't zoom out past fit, or in past the maximum.
  assert.deepEqual(pinchView(v, 0.1, 400, 200, 0, 0, 800, 400), fit);
  assert.equal(pinchView(fit, 100, 0, 0, 0, 0, 800, 400).s, MAX_ZOOM);
});

test('follow pans just enough to keep a point inside the margin', () => {
  const v = { s: 2, x: -400, y: -200 }; // showing content 200..600 x 100..300
  assert.deepEqual(followView(v, 400, 200, 800, 400, 40), v);
  // Content x 590 is at stage 780, 20 px past the margin: pan left by 20.
  assert.deepEqual(followView(v, 590, 200, 800, 400, 40), { s: 2, x: -420, y: -200 });
  // Never past the content's edge.
  assert.deepEqual(followView(v, 0, 0, 800, 400, 40), { s: 2, x: 0, y: 0 });
  assert.deepEqual(clampView({ s: 2, x: -2000, y: 5 }, 800, 400), { s: 2, x: -800, y: 0 });
});
