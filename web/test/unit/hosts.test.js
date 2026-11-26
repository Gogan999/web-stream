import test from 'node:test';
import assert from 'node:assert/strict';
import * as hosts from '../../js/hosts.js';

function memory() {
  const m = new Map();
  return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)) };
}

test('addresses get the host port; full URLs keep theirs; pairing links are recognized', () => {
  assert.deepEqual(hosts.parseTarget('192.168.1.20'), { origin: 'https://192.168.1.20:8443', link: null });
  assert.deepEqual(hosts.parseTarget(' 192.168.1.20:9000 '), { origin: 'https://192.168.1.20:9000', link: null });
  assert.deepEqual(hosts.parseTarget('https://pc.example.com/'), { origin: 'https://pc.example.com', link: null });
  const secret = 'A'.repeat(43);
  assert.deepEqual(hosts.parseTarget(`https://10.0.0.5:8443/#pair=${secret}`), { origin: 'https://10.0.0.5:8443', link: secret });
  assert.deepEqual(hosts.parseTarget('http://localhost:8080'), { origin: 'http://localhost:8080', link: null });
  assert.equal(hosts.parseTarget(''), null);
  assert.equal(hosts.parseTarget('ftp://x'), null);
  assert.equal(hosts.parseTarget('https://'), null);
});

test('signaling URL follows the PC origin', () => {
  assert.equal(hosts.signalUrlFor('https://10.0.0.5:8443'), 'wss://10.0.0.5:8443/ws');
  assert.equal(hosts.signalUrlFor('http://localhost:8080'), 'ws://localhost:8080/ws');
});

test('saved PCs round-trip, are replaced by origin, renamed and forgotten', () => {
  const store = memory();
  let [list, a] = hosts.upsertHost([], { origin: 'https://a:8443', token: 't1' }, 1);
  [list] = hosts.upsertHost(list, { origin: 'https://b:8443', token: 't2', name: 'Office' }, 2);
  assert.equal(a.name, 'a:8443');
  let again;
  [list, again] = hosts.upsertHost(list, { origin: 'https://a:8443', token: 't3' }, 3);
  assert.equal(again.id, a.id);
  assert.equal(again.token, 't3');
  assert.equal(again.added, 1);
  assert.equal(list.length, 2);
  list = hosts.renameHost(list, a.id, 'Gaming PC');
  hosts.saveHosts(list, store);
  const loaded = hosts.loadHosts(store);
  assert.deepEqual(loaded.map((h) => h.name).sort(), ['Gaming PC', 'Office']);
  assert.equal(hosts.forgetToken(loaded, a.id).find((h) => h.id === a.id).token, null);
  assert.equal(hosts.removeHost(loaded, a.id).length, 1);
});

test('broken or missing storage reads as empty', () => {
  const store = memory();
  store.setItem('ws-hosts', '{not json');
  assert.deepEqual(hosts.loadHosts(store), []);
  assert.deepEqual(hosts.loadHosts(null), []);
  store.setItem('ws-hosts', JSON.stringify([{ id: 'local', origin: 'x' }, { id: 'ok', origin: 'https://x' }, null]));
  assert.deepEqual(hosts.loadHosts(store).map((h) => h.id), ['ok']);
  hosts.setLastHost('ok', store);
  assert.equal(hosts.lastHost(store), 'ok');
  const throwing = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  assert.deepEqual(hosts.loadHosts(throwing), []);
  hosts.saveHosts([], throwing);
});
