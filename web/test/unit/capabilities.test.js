import test from 'node:test';
import assert from 'node:assert/strict';
import { applyCapabilities, NO_CAPABILITIES, parseCapabilities } from '../../js/capabilities.js';

test('parses the host message and distrusts the rest', () => {
  assert.deepEqual(
    parseCapabilities({ type: 'capabilities', clipboard: true, sas: true, hostName: 'DESKTOP', version: '1.0.0' }),
    { clipboard: true, sas: true, hostName: 'DESKTOP', version: '1.0.0' },
  );
  assert.deepEqual(parseCapabilities({ type: 'capabilities', sas: 'yes', hostName: 5 }), NO_CAPABILITIES);
  assert.deepEqual(parseCapabilities(null), NO_CAPABILITIES);
});

test('Ctrl+Alt+Del buttons show only when the host can send it', () => {
  const buttons = [{ hidden: false }, { hidden: false }];
  const doc = { title: '' };
  applyCapabilities(NO_CAPABILITIES, { sasButtons: buttons, doc });
  assert.deepEqual(buttons.map((b) => b.hidden), [true, true]);
  assert.equal(doc.title, 'web-stream');
  applyCapabilities(parseCapabilities({ type: 'capabilities', sas: true, hostName: 'PC' }), { sasButtons: buttons, doc });
  assert.deepEqual(buttons.map((b) => b.hidden), [false, false]);
  assert.equal(doc.title, 'PC - web-stream');
});
