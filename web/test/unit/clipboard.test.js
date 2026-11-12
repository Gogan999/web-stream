import test from 'node:test';
import assert from 'node:assert/strict';
import { ClipboardSync, MAX_BYTES } from '../../js/clipboard.js';
import { InputCapture } from '../../js/input.js';
import { HID } from '../../js/keymap.js';

// A browser stand-in: a clipboard that can refuse writes (iOS without a gesture) or reads.
function setup({ permission = 'granted', writes = true, reads = true, text = '' } = {}) {
  const sent = [];
  const notes = [];
  let changes = 0;
  const board = { text, writeCalls: 0 };
  const nav = {
    clipboard: {
      async writeText(t) {
        board.writeCalls++;
        if (!board.allowWrite && !writes) throw new DOMException('no gesture', 'NotAllowedError');
        board.text = t;
      },
      async readText() {
        if (!reads) throw new DOMException('denied', 'NotAllowedError');
        return board.text;
      },
    },
    permissions: permission === null ? undefined : {
      query: async () => ({ state: permission, onchange: null }),
    },
  };
  const doc = { hasFocus: () => true };
  const storage = new Map();
  const clip = new ClipboardSync({
    sendControl: (m) => sent.push(m), notify: (t) => notes.push(t), onChange: () => changes++,
    nav, doc, ackTimeoutMs: 20,
    storage: { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) },
  });
  return { clip, sent, notes, board, storage, changes: () => changes };
}
const settle = () => new Promise((r) => setTimeout(r, 0));

test('unavailable until the host says it shares its clipboard', async () => {
  const { clip, sent } = setup();
  await settle();
  assert.equal(clip.usable, false);
  assert.equal(await clip.toHost(), false);
  assert.equal(clip.beforePaste(), null);
  assert.deepEqual(sent, []);
  clip.setAvailable(true);
  assert.equal(clip.usable, true);
  clip.setAvailable(false);
  assert.equal(clip.usable, false);
});

test('host text is written to the browser clipboard, and not sent back', async () => {
  const { clip, sent, board } = setup({ text: 'old' });
  clip.setAvailable(true);
  assert.equal(clip.onControl({ type: 'clipboard', text: 'from pc' }), true);
  await settle();
  assert.equal(board.text, 'from pc');
  assert.equal(clip.pending, null);
  // Focus returns: the clipboard still holds what the host sent, so nothing is sent back.
  await clip.onFocus();
  assert.deepEqual(sent, []);
});

test('a refused write waits for a gesture and lights the button', async () => {
  const { clip, board, notes } = setup({ writes: false, permission: null });
  clip.setAvailable(true);
  clip.onControl({ type: 'clipboard', text: 'later' });
  await settle();
  assert.equal(clip.pending, 'later');
  assert.equal(board.text, '');
  board.allowWrite = true; // the user taps
  await clip.button();
  assert.equal(board.text, 'later');
  assert.equal(clip.pending, null);
  assert.match(notes.at(-1), /Copied/);
});

test('this clipboard goes to the host and the paste waits for the answer', async () => {
  const { clip, sent } = setup({ text: 'héllo' });
  clip.setAvailable(true);
  await settle(); // permission query
  assert.equal(clip.autoRead, true);
  const wait = clip.beforePaste();
  assert.ok(wait);
  let done = false;
  wait.then(() => { done = true; });
  await settle();
  assert.deepEqual(sent, [{ type: 'setClipboard', text: 'héllo' }]);
  assert.equal(done, false, 'still waiting for clipboardSet');
  clip.onControl({ type: 'clipboardSet', ok: true });
  assert.equal(await wait, true);
  // The same text again is not sent twice.
  assert.equal(await clip.toHost(), true);
  assert.equal(sent.length, 1);
});

test('the paste goes ahead after the timeout if the host never answers', async () => {
  const { clip } = setup({ text: 'x' });
  clip.setAvailable(true);
  await settle();
  assert.equal(await clip.beforePaste(), false);
});

test('without a readable permission (iOS) Ctrl+V is not held back, buttons still read', async () => {
  const { clip, sent } = setup({ permission: null, text: 'tap' });
  clip.setAvailable(true);
  await settle();
  assert.equal(clip.beforePaste(), null);
  const pressed = clip.button();
  await settle();
  clip.onControl({ type: 'clipboardSet', ok: true });
  await pressed;
  assert.deepEqual(sent, [{ type: 'setClipboard', text: 'tap' }]);
});

test('denied reads and oversize text are reported, not sent', async () => {
  const denied = setup({ reads: false });
  denied.clip.setAvailable(true);
  await settle();
  assert.equal(await denied.clip.toHost(), false);
  assert.deepEqual(denied.sent, []);

  const big = setup({ text: 'x'.repeat(MAX_BYTES + 1) });
  big.clip.setAvailable(true);
  await settle();
  assert.equal(await big.clip.toHost(), false);
  assert.deepEqual(big.sent, []);
  assert.match(big.notes.at(-1), /too big/);
  // Exactly the limit goes, counted in UTF-8 bytes: 2-byte characters over it do not.
  const sent = big.sent.length;
  big.clip.pushText('\u00e9'.repeat(MAX_BYTES / 2));
  assert.equal(big.sent.length, sent + 1);
  assert.equal(await big.clip.pushText('\u00e9'.repeat(MAX_BYTES / 2 + 1)), false);
  assert.equal(big.sent.length, sent + 1);
});

test('turning it off stops everything and is remembered', async () => {
  const { clip, sent, storage } = setup({ text: 'a' });
  clip.setAvailable(true);
  clip.setEnabled(false);
  assert.equal(storage.get('ws-clipboard-sync'), 'off');
  clip.onControl({ type: 'clipboard', text: 'ignored' });
  assert.equal(clip.pending, null);
  assert.equal(await clip.toHost(), false);
  assert.deepEqual(sent, []);
});

test('Ctrl+V waits for the clipboard, and keys behind it keep their order', async () => {
  const log = [];
  let release;
  const cap = {
    heldKeys: new Set([HID.ControlLeft]),
    keyGate: null,
    releaseEpoch: 0,
    onBeforePaste: () => new Promise((r) => { release = r; }),
    onKeySent: () => {},
    rel: (buf) => log.push([...new Uint8Array(buf)].slice(1, 4).join(',')),
  };
  cap.sendKey = InputCapture.prototype.sendKey;
  const V = HID.KeyV;
  cap.heldKeys.add(V);
  cap.sendKey(V, true);
  // V released at once: it must not overtake the paste.
  cap.heldKeys.delete(V);
  cap.sendKey(V, false);
  assert.deepEqual(log, []);
  release(true);
  await settle();
  await settle();
  assert.deepEqual(log, [`${V},0,1`, `${V},0,0`]);
  assert.equal(cap.keyGate, null);
});

test('a key released while the paste waits is not pressed late', async () => {
  const log = [];
  let release;
  const cap = {
    heldKeys: new Set([HID.ControlLeft, HID.KeyV]),
    keyGate: null,
    releaseEpoch: 0,
    onBeforePaste: () => new Promise((r) => { release = r; }),
    rel: (buf) => log.push(new Uint8Array(buf)[0]),
  };
  cap.sendKey = InputCapture.prototype.sendKey;
  cap.sendKey(HID.KeyV, true);
  cap.heldKeys.clear(); // focus lost: everything released
  cap.releaseEpoch++;
  release();
  await settle();
  await settle();
  assert.deepEqual(log, []);
});

test('other keys are not delayed', () => {
  const log = [];
  const cap = {
    heldKeys: new Set([HID.ControlLeft, HID.KeyC]),
    keyGate: null,
    releaseEpoch: 0,
    onBeforePaste: () => assert.fail('only paste waits'),
    rel: (buf) => log.push(new Uint8Array(buf)[0]),
  };
  cap.sendKey = InputCapture.prototype.sendKey;
  cap.sendKey(HID.KeyC, true);
  assert.deepEqual(log, [0x05]);
});
