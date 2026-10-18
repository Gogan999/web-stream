// WebSocket signaling client. JSON text frames; see docs/signaling.md.
// Auth is the HttpOnly pairing cookie, which the browser sends on the upgrade by itself.

export class Signaling extends EventTarget {
  constructor(url) {
    super();
    this.url = url;
    this.ws = null;
  }

  get isOpen() {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  open() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      ws.onopen = () => resolve();
      ws.onerror = () => reject(new Error('Could not reach host (or not paired)'));
      ws.onclose = (e) => this.dispatchEvent(new CustomEvent('close', { detail: e }));
      ws.onmessage = (e) => {
        let msg;
        try { msg = JSON.parse(e.data); } catch { return; }
        if (msg && typeof msg.type === 'string') {
          this.dispatchEvent(new CustomEvent('message', { detail: msg }));
        }
      };
    });
  }

  send(msg) {
    if (this.isOpen) this.ws.send(JSON.stringify(msg));
  }

  close() {
    if (this.ws) this.ws.close();
    this.ws = null;
  }
}

// Same origin as the page, /ws.
export function defaultSignalUrl(loc = location) {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${loc.host}/ws`;
}
