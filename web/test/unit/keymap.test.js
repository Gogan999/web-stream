import test from 'node:test';
import assert from 'node:assert/strict';
import { hidUsage, HID } from '../../js/keymap.js';

test('known HID usages', () => {
  assert.equal(hidUsage('KeyA'), 0x04);
  assert.equal(hidUsage('KeyZ'), 0x1d);
  assert.equal(hidUsage('Digit1'), 0x1e);
  assert.equal(hidUsage('Digit9'), 0x26);
  assert.equal(hidUsage('Digit0'), 0x27);
  assert.equal(hidUsage('F1'), 0x3a);
  assert.equal(hidUsage('F12'), 0x45);
  assert.equal(hidUsage('F13'), 0x68);
  assert.equal(hidUsage('F24'), 0x73);
  assert.equal(hidUsage('Numpad1'), 0x59);
  assert.equal(hidUsage('Numpad9'), 0x61);
  assert.equal(hidUsage('Numpad0'), 0x62);
  assert.equal(hidUsage('MetaLeft'), 0xe3);
  assert.equal(hidUsage('Escape'), 0x29);
  assert.equal(hidUsage('NoSuchKey'), 0);
});

test('no two codes share a usage', () => {
  const seen = new Map();
  for (const [code, u] of Object.entries(HID)) {
    assert.ok(!seen.has(u), `${code} and ${seen.get(u)} both map to ${u}`);
    seen.set(u, code);
  }
});
