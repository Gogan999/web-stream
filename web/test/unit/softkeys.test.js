import test from 'node:test';
import assert from 'node:assert/strict';
import { charKey, diffText, typeDiff } from '../../js/softkeys.js';
import { HID } from '../../js/keymap.js';

test('characters map to US-layout keys with shift', () => {
  assert.deepEqual(charKey('a'), { usage: HID.KeyA, shift: false });
  assert.deepEqual(charKey('A'), { usage: HID.KeyA, shift: true });
  assert.deepEqual(charKey('@'), { usage: HID.Digit2, shift: true });
  assert.deepEqual(charKey('/'), { usage: HID.Slash, shift: false });
  assert.deepEqual(charKey('\n'), { usage: HID.Enter, shift: false });
  assert.deepEqual(charKey('’'), { usage: HID.Quote, shift: false }); // smart quote
  assert.equal(charKey('é'), null);
});

test('text diffs: typing, deleting, and autocorrect replacing a word', () => {
  assert.deepEqual(diffText('xx', 'xxhi'), { backspaces: 0, text: 'hi' });
  assert.deepEqual(diffText('xx', 'x'), { backspaces: 1, text: '' });
  assert.deepEqual(diffText('xxteh', 'xxthe '), { backspaces: 2, text: 'he ' });
});

test('typed text becomes key presses', () => {
  const sent = [];
  typeDiff({ backspaces: 1, text: 'Hi!' }, (u, d) => sent.push([u, d]));
  const S = HID.ShiftLeft;
  assert.deepEqual(sent, [
    [HID.Backspace, true], [HID.Backspace, false],
    [S, true], [HID.KeyH, true], [HID.KeyH, false], [S, false],
    [HID.KeyI, true], [HID.KeyI, false],
    [S, true], [HID.Digit1, true], [HID.Digit1, false], [S, false],
  ]);
  // With a sticky Shift already down, no extra Shift presses.
  sent.length = 0;
  typeDiff({ backspaces: 0, text: 'A' }, (u, d) => sent.push([u, d]), true);
  assert.deepEqual(sent, [[HID.KeyA, true], [HID.KeyA, false]]);
});

test('characters with no US key are typed as Unicode, in order with key presses', () => {
  const keys = [];
  const texts = [];
  const log = [];
  typeDiff({ backspaces: 0, text: 'a\u00e9\u4e16b\u200b' }, (u, d) => { keys.push([u, d]); log.push('k' + u + d); }, false, (t) => { texts.push(t); log.push('t:' + t); });
  assert.deepEqual(texts, ['\u00e9\u4e16']);
  assert.deepEqual(log, [`k${HID.KeyA}true`, `k${HID.KeyA}false`, 't:\u00e9\u4e16', `k${HID.KeyB}true`, `k${HID.KeyB}false`]);
  // Trailing unmapped text is flushed; without a text sink it is dropped as before.
  const tail = [];
  typeDiff({ backspaces: 0, text: '\u{1f600}' }, () => {}, false, (t) => tail.push(t));
  assert.deepEqual(tail, ['\u{1f600}']);
  const none = [];
  typeDiff({ backspaces: 0, text: '\u00e9' }, (u, d) => none.push([u, d]));
  assert.deepEqual(none, []);
});
