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
