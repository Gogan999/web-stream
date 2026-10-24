import test from 'node:test';
import assert from 'node:assert/strict';
import { takePairingLink } from '../../js/pairing.js';

function fakeLocation(hash) {
  const calls = [];
  const loc = { hash, pathname: '/', search: '?autoconnect' };
  const hist = { replaceState: (_s, _t, url) => calls.push(url) };
  return { loc, hist, calls };
}

test('a pairing link is taken off the address', () => {
  const secret = 'Ab3_-'.repeat(8) + 'xyz';
  const { loc, hist, calls } = fakeLocation(`#pair=${secret}`);
  assert.equal(takePairingLink(loc, hist), secret);
  assert.deepEqual(calls, ['/?autoconnect']);
});

test('a malformed pairing link is dropped, other fragments are left alone', () => {
  let f = fakeLocation('#pair=<script>');
  assert.equal(takePairingLink(f.loc, f.hist), null);
  assert.equal(f.calls.length, 1);
  f = fakeLocation('#pair=short');
  assert.equal(takePairingLink(f.loc, f.hist), null);
  f = fakeLocation('#settings');
  assert.equal(takePairingLink(f.loc, f.hist), null);
  assert.deepEqual(f.calls, []);
  f = fakeLocation('');
  assert.equal(takePairingLink(f.loc, f.hist), null);
});
