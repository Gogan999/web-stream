import test from 'node:test';
import assert from 'node:assert/strict';
import { diagEnabled, videoSample } from '../../js/diag.js';

const store = (v) => ({ getItem: () => v });

test('diag is on with ?diag or the stored flag', () => {
  assert.equal(diagEnabled(new URLSearchParams('diag'), store(null)), true);
  assert.equal(diagEnabled(new URLSearchParams('diag=0'), store('1')), false);
  assert.equal(diagEnabled(new URLSearchParams(''), store('1')), true);
  assert.equal(diagEnabled(new URLSearchParams(''), store(null)), false);
  assert.equal(diagEnabled(new URLSearchParams(''), { getItem: () => { throw new Error('blocked'); } }), false);
});

test('video samples keep only the counters, rounded', () => {
  const s = videoSample({ type: 'inbound-rtp', framesDecoded: 10, pliCount: 2, totalDecodeTime: 0.0123456, codecId: 'x' });
  assert.deepEqual(s, { framesDecoded: 10, pliCount: 2, totalDecodeTime: 0.0123 });
});
