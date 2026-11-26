// Clipboard sync with the host: text only, both ways, while connected.

export const MAX_BYTES = 256 * 1024;
const PREF_KEY = 'ws-clipboard-sync';

const byteLength = (s) => new TextEncoder().encode(s).length;

export class ClipboardSync {
  constructor({ sendControl, onChange = () => {}, notify = () => {}, nav = globalThis.navigator,
    doc = globalThis.document, storage = globalThis.localStorage, ackTimeoutMs = 300 } = {}) {
    this.sendControl = sendControl;
    this.onChange = onChange;
    this.notify = notify;
    this.nav = nav;
    this.doc = doc;
    this.storage = storage;
    this.ackTimeoutMs = ackTimeoutMs;
    this.available = false;
    this.enabled = this.loadPref();
    this.pending = null;
    this.lastSynced = null;
    this.waiters = [];
    this.permission = null;
    this.watchPermission();
  }

  loadPref() {
    try { return this.storage?.getItem(PREF_KEY) !== 'off'; } catch { return true; }
  }

  get usable() { return this.available && this.enabled; }

  get autoRead() { return this.permission !== null && this.permission !== 'denied'; }

  setEnabled(on) {
    this.enabled = !!on;
    try { this.storage?.setItem(PREF_KEY, this.enabled ? 'on' : 'off'); } catch { }
    if (!this.enabled) this.pending = null;
    this.onChange();
  }

  async watchPermission() {
    try {
      const status = await this.nav?.permissions?.query({ name: 'clipboard-read' });
      if (!status) return;
      this.permission = status.state;
      status.onchange = () => { this.permission = status.state; };
    } catch { }
  }

  // Forget the session (it closed): a new one starts from what the host says.
  reset() {
    this.available = false;
    this.pending = null;
    this.lastSynced = null;
    for (const w of this.waiters.splice(0)) { clearTimeout(w.timer); w.resolve(false); }
    this.onChange();
  }

  // The host's `capabilities` message (capabilities.js) says whether it shares its clipboard.
  setAvailable(on) {
    this.available = on === true;
    this.onChange();
  }

  // A control message for this module; true if it was one.
  onControl(msg) {
    switch (msg.type) {
      case 'clipboard':
        if (typeof msg.text === 'string') this.onHostText(msg.text);
        return true;
      case 'clipboardSet': {
        const w = this.waiters.shift();
        if (w) { clearTimeout(w.timer); w.resolve(msg.ok !== false); }
        if (msg.ok === false) this.notify('The host could not set its clipboard');
        return true;
      }
      default:
        return false;
    }
  }

  // The host's clipboard changed: put it on this one if the browser allows, else keep it for a tap.
  async onHostText(text) {
    if (!this.usable) return;
    this.lastSynced = text;
    this.pending = text;
    this.onChange();
    if (this.doc?.hasFocus?.() !== false) await this.writePending();
  }

  // Writes the waiting host text; false if the browser refused (it stays pending).
  async writePending() {
    const text = this.pending;
    if (text == null) return true;
    try {
      await this.nav.clipboard.writeText(text);
    } catch {
      return false;
    }
    if (this.pending === text) this.pending = null;
    this.onChange();
    return true;
  }

  // Any tap or click: a user gesture, which iOS wants for a write.
  async onGesture() {
    if (this.pending != null) await this.writePending();
  }

  // The page or the stream got focus: Chromium may write what was waiting and read what changed
  // here.
  async onFocus() {
    if (!this.usable) return;
    if (this.pending != null) await this.writePending();
    if (this.permission === 'granted') await this.toHost();
  }

  async readLocal() {
    try {
      return await this.nav.clipboard.readText();
    } catch {
      return null;
    }
  }

  // Sends this browser's clipboard to the host and resolves once the host has set it (or after
  // ackTimeoutMs): true if the host has the text, false if there is nothing or it did not work.
  async toHost() {
    if (!this.usable) return false;
    const text = await this.readLocal();
    if (!text) return false;
    return this.pushText(text);
  }

  // Sends `text` as the host's clipboard.
  pushText(text) {
    if (!this.usable) return Promise.resolve(false);
    if (text === this.lastSynced) return Promise.resolve(true);
    if (byteLength(text) > MAX_BYTES) {
      this.notify('The clipboard is too big to share (over 256 KiB)');
      return Promise.resolve(false);
    }
    this.lastSynced = text;
    this.pending = null;
    return new Promise((resolve) => {
      const waiter = { resolve, timer: 0 };
      waiter.timer = setTimeout(() => {
        const i = this.waiters.indexOf(waiter);
        if (i >= 0) this.waiters.splice(i, 1);
        resolve(false);
      }, this.ackTimeoutMs);
      this.waiters.push(waiter);
      this.sendControl({ type: 'setClipboard', text });
    });
  }

  // Before the paste keys (Ctrl+V) go to the host: a promise to wait for, or null to send them now.
  beforePaste() {
    if (!this.usable || !this.autoRead || !this.nav?.clipboard?.readText) return null;
    return this.toHost().catch(() => false);
  }

  // The toolbar button: on a waiting host text, copy it here.
  async button() {
    if (!this.usable) return;
    if (this.pending != null) {
      if (await this.writePending()) this.notify("Copied the host's clipboard");
      else this.notify('The browser did not allow it; tap again');
      return;
    }
    const text = await this.readLocal();
    if (!text) { this.notify('Nothing to send, or the browser did not allow reading the clipboard'); return; }
    if (await this.pushText(text)) this.notify('Sent the clipboard to the host');
  }
}
