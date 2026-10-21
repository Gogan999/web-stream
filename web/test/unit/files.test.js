import test from 'node:test';
import assert from 'node:assert/strict';
import { Files, Uploads, parseChunk, safeName, formatSize, ACK_EVERY, HEADER, BLOB_MAX, WINDOW, CHUNK } from '../../js/files.js';

function chunk(id, offset, bytes) {
  const buf = new ArrayBuffer(HEADER + bytes.length);
  const v = new DataView(buf);
  v.setBigUint64(0, BigInt(id));
  v.setBigUint64(8, BigInt(offset));
  new Uint8Array(buf, HEADER).set(bytes);
  return buf;
}

const tick = () => new Promise((r) => setTimeout(r, 0));

// A Files with a stand-in save dialog that collects what is written.
function setup({ disk = true } = {}) {
  const sent = [];
  const written = [];
  const downloads = [];
  let closed = false;
  const files = new Files({
    sendControl: (m) => sent.push(m),
    pickFile: disk ? async () => ({
      write: async (d) => { for (const b of d) written.push(b); },
      close: async () => { closed = true; },
      abort: async () => {},
    }) : null,
    download: (blob, name) => downloads.push({ blob, name }),
  });
  return { files, sent, written, downloads, isClosed: () => closed };
}

test('chunks carry id and offset', () => {
  const c = parseChunk(chunk(2 ** 53 - 1, 5, [1, 2, 3]));
  assert.equal(c.id, 2 ** 53 - 1);
  assert.equal(c.offset, 5);
  assert.deepEqual([...c.data], [1, 2, 3]);
  assert.equal(parseChunk(new ArrayBuffer(15)), null);
  assert.equal(parseChunk('text'), null);
});

test('names are safe to save', () => {
  assert.equal(safeName('report.pdf'), 'report.pdf');
  assert.equal(safeName('..\\..\\evil.exe'), 'evil.exe');
  assert.equal(safeName('a/b/c:d?.txt'), 'c_d_.txt');
  assert.equal(safeName('CON.txt'), '_CON.txt');
  assert.equal(safeName(' .. '), 'file');
  assert.equal(safeName('x'.repeat(300)).length, 200);
  assert.equal(safeName('pic\u202Egpj.exe'), 'picgpj.exe');
  assert.equal(safeName('COM¹.txt'), '_COM¹.txt');
  assert.equal(safeName('conout$'), '_conout$');
  assert.equal(safeName('a' + '😀'.repeat(150)).length, 199, 'no half emoji');
});

test('sizes read well', () => {
  assert.equal(formatSize(512), '512 B');
  assert.equal(formatSize(2.5 * 1024 * 1024), '2.5 MB');
  assert.equal(formatSize(300 * 1024), '300 KB');
});

test('saves an offered file in order, acks and finishes', async () => {
  const { files, sent, written, isClosed } = setup();
  const size = ACK_EVERY + 10;
  files.onControl({ type: 'fileOffer', id: 7, name: 'a/b.bin', size });
  assert.equal(files.list()[0].name, 'b.bin');
  await files.save(7);
  assert.deepEqual(sent.shift(), { type: 'requestFile', id: 7 });
  files.onChunk(chunk(7, 0, new Uint8Array(ACK_EVERY).fill(1)));
  // A stale chunk from an earlier try is dropped.
  files.onChunk(chunk(7, 5, [9, 9]));
  files.onChunk(chunk(8, ACK_EVERY, [9]));
  await tick();
  assert.deepEqual(sent.shift(), { type: 'fileAck', id: 7, received: ACK_EVERY });
  files.onChunk(chunk(7, ACK_EVERY, new Uint8Array(10).fill(2)));
  await tick();
  await tick();
  assert.equal(written.length, size);
  assert.equal(written.at(-1), 2);
  assert.ok(isClosed());
  assert.deepEqual(sent.shift(), { type: 'fileAck', id: 7, received: size });
  assert.equal(files.list()[0].state, 'saved');
});

test('without a save dialog the file goes to downloads', async () => {
  const { files, downloads } = setup({ disk: false });
  files.onControl({ type: 'fileOffer', id: 1, name: 'n.txt', size: 3 });
  await files.save(1);
  files.onChunk(chunk(1, 0, [104, 105, 33]));
  await tick();
  await tick();
  assert.equal(downloads.length, 1);
  assert.equal(downloads[0].name, 'n.txt');
  assert.equal(await downloads[0].blob.text(), 'hi!');
  files.onControl({ type: 'fileOffer', id: 2, name: 'big', size: BLOB_MAX + 1 });
  await files.save(2);
  assert.equal(files.files.get(2).state, 'failed');
});

test('cancel, host failure and withdrawal', async () => {
  const { files, sent } = setup();
  files.onControl({ type: 'fileOffer', id: 3, name: 'x', size: 100 });
  await files.save(3);
  files.cancel(3);
  assert.deepEqual(sent.at(-1), { type: 'cancelFile', id: 3 });
  assert.equal(files.files.get(3).state, 'offered');
  files.onChunk(chunk(3, 0, [1]));
  assert.equal(files.files.get(3).received, 0, 'chunks after cancel are ignored');
  await files.save(3);
  files.onControl({ type: 'fileFailed', id: 3, message: 'gone' });
  assert.equal(files.files.get(3).state, 'failed');
  assert.equal(files.files.get(3).error, 'gone');
  files.onControl({ type: 'fileWithdrawn', id: 3 });
  assert.equal(files.list().length, 0);
});

test('closing the save dialog does nothing', async () => {
  const { files, sent } = setup();
  files.pickFile = async () => { throw Object.assign(new Error('closed'), { name: 'AbortError' }); };
  files.onControl({ type: 'fileOffer', id: 4, name: 'x', size: 1 });
  await files.save(4);
  assert.equal(sent.length, 0);
  assert.equal(files.files.get(4).state, 'offered');
});

test('a save dialog that refuses to open falls back to downloads', async () => {
  const { files, sent, downloads } = setup();
  files.pickFile = async () => { throw Object.assign(new Error('blocked by policy'), { name: 'NotAllowedError' }); };
  files.onControl({ type: 'fileOffer', id: 5, name: 'y.txt', size: 2 });
  await files.save(5);
  assert.deepEqual(sent, [{ type: 'requestFile', id: 5 }]);
  files.onChunk(chunk(5, 0, [7, 8]));
  for (let i = 0; i < 10; i++) await tick();
  assert.equal(files.files.get(5).state, 'saved');
  assert.equal(downloads[0].name, 'y.txt');
  assert.deepEqual([...new Uint8Array(await downloads[0].blob.arrayBuffer())], [7, 8]);
});

function uploadSetup() {
  const sent = [];
  const chunks = [];
  let buffered = 0;
  const u = new Uploads({
    sendControl: (m) => sent.push(m),
    sendFile: (b) => { chunks.push(parseChunk(b)); return true; },
    fileBuffered: () => buffered,
    sleep: () => new Promise((r) => setTimeout(r, 1)),
  });
  return { u, sent, chunks, setBuffered: (n) => { buffered = n; } };
}

const until = async (f) => { for (let i = 0; i < 500 && !f(); i++) await new Promise((r) => setTimeout(r, 2)); assert.ok(f()); };

test('uploads wait for the host to allow them, then go one at a time within the window', async () => {
  const { u, sent, chunks } = uploadSetup();
  const big = new Blob([new Uint8Array(WINDOW + 3 * CHUNK).fill(7)]);
  u.add([Object.assign(big, { name: 'x' })]);
  assert.equal(u.list().length, 0, 'nothing until the host takes files');
  u.onControl({ type: 'uploads', enabled: true, maxSize: 10 * WINDOW });
  const a = new File([big], 'C:\\dir\\a.bin');
  const b = new File(['hi'], 'b.txt');
  u.add([a, b]);
  assert.deepEqual(sent, [{ type: 'uploadFile', id: 1, name: 'a.bin', size: a.size }]);
  u.onControl({ type: 'uploadAccepted', id: 1 });
  await until(() => chunks.length === WINDOW / CHUNK);
  await new Promise((r) => setTimeout(r, 20));
  assert.equal(chunks.length, WINDOW / CHUNK, 'stops at the window');
  assert.ok(chunks.every((c, i) => c.id === 1 && c.offset === i * CHUNK));
  u.onControl({ type: 'uploadAck', id: 1, received: WINDOW });
  await until(() => chunks.length === WINDOW / CHUNK + 3);
  u.onControl({ type: 'uploadSaved', id: 1, name: 'a (2).bin' });
  assert.equal(u.items.get(1).savedAs, 'a (2).bin');
  assert.deepEqual(sent.at(-1), { type: 'uploadFile', id: 2, name: 'b.txt', size: 2 });
  u.onControl({ type: 'uploadFailed', id: 2, message: 'disk full' });
  assert.equal(u.items.get(2).error, 'disk full');
});

test('uploads over the host limit fail at once; cancel tells the host', async () => {
  const { u, sent } = uploadSetup();
  u.onControl({ type: 'uploads', enabled: true, maxSize: 1 });
  u.add([new File(['too big'], 'big.txt'), new File(['x'], 'ok.txt')]);
  assert.equal(u.items.get(1).state, 'failed');
  assert.equal(sent.at(-1).name, 'ok.txt');
  u.cancel(2);
  assert.deepEqual(sent.at(-1), { type: 'cancelUpload', id: 2 });
  assert.equal(u.items.get(2).state, 'failed');
});
