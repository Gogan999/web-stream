// Clipboard sync with the host: text only, both ways, while connected. See docs/dev/protocol.md
// (capabilities.clipboard, clipboard, setClipboard, clipboardSet).
//
// What a browser lets a page do decides the shape of this:
//  - Writing the browser clipboard (host -> here) needs the page focused and, on iOS Safari, a user
//    gesture. When a write is refused the text waits as `pending`, the Clipboard button says "Copy
//    from PC", and any tap on the stream or the button writes it.
//  - Reading it (here -> host) needs a permission prompt in Chromium (asked once, on the first
//    Ctrl+V or button press). Elsewhere (iOS Safari) readText() is only for a tap, which shows
//    iOS's own Paste bubble. So reading on focus, and holding back Ctrl+V until the host has the
//    text, happen only where the browser reports a 'clipboard-read' permission (Chromium).
// Nothing here throws: any refusal just leaves the feature on its button.

export const MAX_BYTES = 256 * 1024; // the host's limit (ws_proto::CLIPBOARD_MAX)
const PREF_KEY = 'ws-clipboard-sync';

const byteLength = (s) => new TextEncoder().encode(s).length;

export class ClipboardSync {
  // sendControl(msg): to the host. onChange(): the state shown on the button changed.
  // notify(text): a short message for the status line.
  constructor({ sendControl, onChange = () => {}, notify = () => {}, nav = globalThis.navigator,
    doc = globalThis.document, storage = globalThis.localStorage, ackTimeoutMs = 300 } = {}) {
    this.sendControl = sendControl;
    this.onChange = onChange;
    this.notify = notify;
    this.nav = nav;
    this.doc = doc;
    this.storage = storage;
    this.ackTimeoutMs = ackTimeoutMs;
    this.available = false; // the host shares its clipboard and can open it
    this.enabled = this.loadPref();
    this.pending = null; // host text this browser has not been able to write yet
    this.lastSynced = null; // what both clipboards last agreed on, so nothing echoes back
    this.waiters = []; // setClipboard sent, answer not yet back
    this.permission = null; // 'granted' | 'prompt' | 'denied', or null where it can't be asked
    this.watchPermission();
  }

  loadPref() {
    try { return this.storage?.getItem(PREF_KEY) !== 'off'; } catch { return true; }
  }

  // Whether sync works right now: the host offers it and the user has not turned it off.
  get usable() { return this.available && this.enabled; }

  // Where the browser can say whether it may read the clipboard (Chromium), it reads on focus and
  // before Ctrl+V; elsewhere only buttons read.
  get autoRead() { return this.permission !== null && this.permission !== 'denied'; }

  setEnabled(on) {
    this.enabled = !!on;
    try { this.storage?.setItem(PREF_KEY, this.enabled ? 'on' : 'off'); } catch { /* not kept */ }
    if (!this.enabled) this.pending = null;
    this.onChange();
  }

  async watchPermission() {
    try {
      const status = await this.nav?.permissions?.query({ name: 'clipboard-read' });
      if (!status) return;
      this.permission = status.state;
      status.onchange = () => { this.permission = status.state; };
    } catch { /* Safari and Firefox don't know this permission */ }
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

  // The page or the stream got focus: Chromium may write what was waiting and read what changed here.
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

  // Sends `text` as the host's clipboard (it came from here: a read, or a paste into the keyboard
  // field). Waits for the host's answer; a text the host already has is not sent again.
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

  // The toolbar button: on a waiting host text, copy it here; otherwise send this clipboard.
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
