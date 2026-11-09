import test from 'node:test';
import assert from 'node:assert/strict';
import '../lib/stamp.js';

const { words, decode, age } = globalThis.wsStamp;

const bits = (ws) => ws.flatMap((w) => Array.from({ length: 32 }, (_, i) => ((w >>> (31 - i)) & 1 ? 235 : 16)));

test('same vector as the host (crates/capture/src/synthetic/stamp.rs)', () => {
  assert.deepEqual(words({ time: 0x23456789, display: 2, frame: 0xabcde }), [0x23456789, 0x2abcde4d]);
});

test('decodes what the host draws, and rejects a flipped bit', () => {
  const s = { time: 1762650000123 % 2 ** 32, display: 3, frame: 1048575 };
  const levels = bits(words(s));
  assert.deepEqual(decode(levels), s);
  levels[40] = levels[40] > 128 ? 16 : 235;
  assert.equal(decode(levels), null);
});

test('age works across the 32-bit wrap', () => {
  assert.equal(age(2 ** 32 - 10, 2 ** 32 + 15), 25);
  assert.equal(age(100, 90), -10);
});
