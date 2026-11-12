import test from 'node:test';
import assert from 'node:assert/strict';
import { applyCapabilities, NO_CAPABILITIES, parseCapabilities } from '../../js/capabilities.js';

function hooks() {
  const state = { sas: null, name: null, version: null };
  return {
    state,
    setSasVisible: (v) => (state.sas = v),
    setHostName: (v) => (state.name = v),
    setHostVersion: (v) => (state.version = v),
  };
}

test('parses the host message and distrusts the rest', () => {
  assert.deepEqual(
    parseCapabilities({ type: 'capabilities', clipboard: true, sas: true, hostName: 'DESKTOP', version: '1.0.0' }),
    { clipboard: true, sas: true, hostName: 'DESKTOP', version: '1.0.0' },
  );
  assert.deepEqual(parseCapabilities({ type: 'capabilities', sas: 'yes', hostName: 5 }), NO_CAPABILITIES);
  assert.deepEqual(parseCapabilities(null), NO_CAPABILITIES);
});

test('Ctrl+Alt+Del shows only when the host can send it; name and version follow', () => {
  const h = hooks();
  applyCapabilities(NO_CAPABILITIES, h);
  assert.deepEqual(h.state, { sas: false, name: '', version: '' });
  applyCapabilities(parseCapabilities({ type: 'capabilities', sas: true, hostName: 'PC', version: '0.2.0' }), h);
  assert.deepEqual(h.state, { sas: true, name: 'PC', version: '0.2.0' });
});
