// Files the host user shares with this browser, and saving them. See docs/protocol.md (File transfer).
//
// The host offers a file on the control channel; Save asks for it, and its bytes come on the
// "file" channel as chunks: 8-byte id, 8-byte offset (both big-endian), then data. A chunk is kept
// only if it starts where the file left off, so bytes still in flight from a cancelled transfer
// are dropped. Chrome writes straight to the file the user picked; without that (Android) the file
// is collected in memory and handed to the browser's downloads.

export const HEADER = 16;
// Stored bytes between acks; the host sends at most 1 MiB past the last one.
export const ACK_EVERY = 256 * 1024;
// Largest file collected in memory where the page can't write to disk as it arrives.
export const BLOB_MAX = 512 * 1024 * 1024;

export function parseChunk(buf) {
  if (!(buf instanceof ArrayBuffer) || buf.byteLength < HEADER) return null;
  const v = new DataView(buf);
  return { id: Number(v.getBigUint64(0)), offset: Number(v.getBigUint64(8)), data: new Uint8Array(buf, HEADER) };
}

// A name safe to suggest when saving: no folders, no characters Windows refuses, no reserved names.
export function safeName(name) {
  let n = String(name ?? '').split(/[\\/]/).pop();
  n = n.replace(/[\x00-\x1f\x7f<>:"|?*]/g, '_').replace(/^[\s.]+|[\s.]+$/g, '').slice(0, 200);
  if (/^(con|prn|aux|nul|com\d|lpt\d)(\.|$)/i.test(n)) n = '_' + n;
  return n || 'file';
}

export function formatSize(n) {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = -1;
  do { v /= 1024; i++; } while (v >= 1024 && i < units.length - 1);
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

// Writes to the file the user picked, in order.
class DiskSink {
  constructor(writable) {
    this.writable = writable;
    this.chain = Promise.resolve();
  }

  write(data) { this.chain = this.chain.then(() => this.writable.write(data)); return this.chain; }
  async finish() { await this.chain; await this.writable.close(); }
  abort() { this.chain.catch(() => {}); this.writable.abort().catch(() => {}); }
}

// Collects the file, then hands it to the browser's downloads.
class BlobSink {
  constructor(name, download) {
    this.name = name;
    this.download = download;
    this.parts = [];
  }

  write(data) { this.parts.push(data); return Promise.resolve(); }
  async finish() { this.download(new Blob(this.parts), this.name); this.parts = []; }
  abort() { this.parts = []; }
}

export function downloadBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

// Offered files by id: { id, name, size, state, received, stored, error, startedAt }.
// state: 'offered' | 'receiving' | 'saved' | 'failed'. Fires 'change' on any change and
// 'offer' (detail: file) for a new offer.
export class Files extends EventTarget {
  constructor({ sendControl, pickFile = defaultPicker(), download = downloadBlob }) {
    super();
    this.sendControl = sendControl;
    this.pickFile = pickFile;
    this.download = download;
    this.files = new Map();
  }

  list() { return [...this.files.values()]; }

  changed() { this.dispatchEvent(new Event('change')); }

  // Handles a control message; true if it was about files.
  onControl(msg) {
    if (msg.type === 'fileOffer' && Number.isSafeInteger(msg.id) && Number.isSafeInteger(msg.size) && msg.size >= 0) {
      const file = { id: msg.id, name: safeName(msg.name), size: msg.size, state: 'offered', received: 0, stored: 0, error: null };
      this.files.set(msg.id, file);
      this.dispatchEvent(new CustomEvent('offer', { detail: file }));
      this.changed();
      return true;
    }
    if (msg.type === 'fileWithdrawn') {
      const f = this.files.get(msg.id);
      if (f) {
        f.sink?.abort();
        this.files.delete(msg.id);
        this.changed();
      }
      return true;
    }
    if (msg.type === 'fileFailed') {
      const f = this.files.get(msg.id);
      if (f && f.state === 'receiving') this.fail(f, String(msg.message ?? 'failed'), false);
      return true;
    }
    return false;
  }

  // Picks where to save and asks the host for the file. Must run in a click handler: the save
  // dialog needs the user's gesture.
  async save(id) {
    const f = this.files.get(id);
    if (!f || f.state === 'receiving') return;
    let sink;
    try {
      const writable = await this.pickFile?.(f.name);
      if (writable) sink = new DiskSink(writable);
    } catch (err) {
      if (err?.name === 'AbortError') return; // the user closed the save dialog
      this.fail(f, `could not save: ${err.message}`, false);
      return;
    }
    if (!sink) {
      if (f.size > BLOB_MAX) {
        this.fail(f, `too big for this browser (over ${formatSize(BLOB_MAX)})`, false);
        return;
      }
      sink = new BlobSink(f.name, this.download);
    }
    Object.assign(f, { sink, state: 'receiving', received: 0, stored: 0, acked: 0, error: null, startedAt: performance.now() });
    this.sendControl({ type: 'requestFile', id });
    this.changed();
    if (f.size === 0) this.complete(f);
  }

  cancel(id) {
    const f = this.files.get(id);
    if (!f || f.state !== 'receiving') return;
    this.sendControl({ type: 'cancelFile', id });
    f.sink.abort();
    Object.assign(f, { sink: null, state: 'offered', received: 0, stored: 0 });
    this.changed();
  }

  onChunk(buf) {
    const c = parseChunk(buf);
    if (!c) return;
    const f = this.files.get(c.id);
    if (!f || f.state !== 'receiving' || c.offset !== f.received) return;
    const len = Math.min(c.data.byteLength, f.size - f.received);
    if (len <= 0) return;
    f.received += len;
    const sink = f.sink;
    sink.write(c.data.subarray(0, len)).then(() => {
      if (f.sink !== sink) return;
      f.stored += len;
      if (f.stored === f.size) this.complete(f);
      else if (f.stored - f.acked >= ACK_EVERY) this.ack(f);
      this.changed();
    }, (err) => {
      if (f.sink === sink) this.fail(f, `could not write: ${err.message}`, true);
    });
  }

  ack(f) {
    f.acked = f.stored;
    this.sendControl({ type: 'fileAck', id: f.id, received: f.stored });
  }

  async complete(f) {
    const sink = f.sink;
    try {
      await sink.finish();
    } catch (err) {
      if (f.sink === sink) this.fail(f, `could not write: ${err.message}`, true);
      return;
    }
    if (f.sink !== sink) return;
    this.ack(f);
    Object.assign(f, { sink: null, state: 'saved' });
    this.changed();
  }

  fail(f, message, tellHost) {
    if (tellHost && f.state === 'receiving') this.sendControl({ type: 'cancelFile', id: f.id });
    f.sink?.abort();
    Object.assign(f, { sink: null, state: 'failed', error: message });
    this.changed();
  }

  // The session ended: nothing is on offer any more.
  clear() {
    for (const f of this.files.values()) f.sink?.abort();
    this.files.clear();
    this.changed();
  }
}

// Chrome's save dialog, writing as the file arrives; null where there is none (then the file is
// collected in memory).
function defaultPicker() {
  if (typeof window === 'undefined' || typeof window.showSaveFilePicker !== 'function') return null;
  return async (name) => {
    const handle = await window.showSaveFilePicker({ suggestedName: name });
    return handle.createWritable();
  };
}
