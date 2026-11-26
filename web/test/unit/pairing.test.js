import test from 'node:test';
import assert from 'node:assert/strict';
import { takePairingLink } from '../../js/pairing.js';

function fakeLocation(hash) {
  const calls = [];
  const loc = { hash, pathname: '/', search: '?autoconnect' };
  // Like a browser's: the fragment is gone once the address is replaced.
  const hist = { replaceState: (_s, _t, url) => { calls.push(url); loc.hash = ''; } };
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
  assert.equal(takePairingLink(f.loc, f.hist), '');
  assert.equal(f.calls.length, 1);
  f = fakeLocation('#pair=short');
  assert.equal(takePairingLink(f.loc, f.hist), '');
  f = fakeLocation('#settings');
  assert.equal(takePairingLink(f.loc, f.hist), null);
  assert.deepEqual(f.calls, []);
  f = fakeLocation('');
  assert.equal(takePairingLink(f.loc, f.hist), null);
});

test('pairing another PC asks for a token, sends no cookie and uses the bearer token', async () => {
  const calls = [];
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url, opts) => {
    calls.push({ url, opts });
    return { status: 200, json: async () => ({ paired: true, token: 'tok', pairing_id: 'p', expires_in: 120 }) };
  };
  try {
    const { startPairing, confirmPairing, getSession, unpair } = await import('../../js/pairing.js');
    const target = { origin: 'https://pc2:8443' };
    await startPairing('iPhone', target);
    assert.equal(await confirmPairing('p', '123456', target), 'tok');
    await getSession({ ...target, token: 'tok' });
    await unpair({ ...target, token: 'tok' });
    assert.equal(calls[0].url, 'https://pc2:8443/api/pair/start');
    assert.equal(calls[0].opts.credentials, 'omit');
    assert.equal(calls[0].opts.mode, 'cors');
    assert.deepEqual(JSON.parse(calls[0].opts.body), { device_name: 'iPhone', want_token: true });
    assert.equal(calls[2].opts.headers.Authorization, 'Bearer tok');
    assert.equal(calls[3].opts.headers.Authorization, 'Bearer tok');
    await getSession();
    assert.equal(calls[4].url, '/api/session');
    assert.equal(calls[4].opts.credentials, 'same-origin');
  } finally {
    globalThis.fetch = realFetch;
  }
});
