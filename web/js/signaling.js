// WebSocket signaling client.

export const PROTOCOL = 'web-stream';
export const AUTH_PREFIX = 'web-stream.auth.';

export function protocolsFor(token) {
  return token ? [PROTOCOL, AUTH_PREFIX + token] : undefined;
}

export class Signaling extends EventTarget {
  constructor(url, token = null) {
    super();
    this.url = url;
    this.token = token;
    this.ws = null;
  }

  get isOpen() {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  open() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url, protocolsFor(this.token));
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

export function defaultSignalUrl(loc = location) {
  const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${loc.host}/ws`;
}
