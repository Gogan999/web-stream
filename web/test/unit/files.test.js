import test from 'node:test';
import assert from 'node:assert/strict';
import { Files, parseChunk, safeName, formatSize, ACK_EVERY, HEADER, BLOB_MAX } from '../../js/files.js';

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
