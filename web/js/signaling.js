// WebSocket signaling client. JSON text frames; see docs/protocol.md.

export class Signaling extends EventTarget {
  constructor(url, token) {
    super();
    this.url = url;
    this.token = token;
    this.ws = null;
  }

  open() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;
      ws.onopen = () => {
        if (this.token) this.send({ type: 'hello', token: this.token });
        resolve();
      };
      ws.onerror = () => reject(new Error('signaling connection failed'));
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
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
  }

  close() {
    if (this.ws) this.ws.close();
    this.ws = null;
  }
}

// Default endpoint: same origin as the page, /signal.
export function defaultSignalUrl(loc = location) {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${loc.host}/signal`;
}
