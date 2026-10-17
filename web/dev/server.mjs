// Development server standing in for the host's built-in HTTPS listener: serves web/,
// implements the pairing API (docs/signaling.md) in memory, and relays /signal between one
// mock host page (/signal?role=host) and one paired client. Not for production.
//
//   node dev/server.mjs [port]     then open http://localhost:8080/dev/mock-host.html
//                                  and http://localhost:8080/
//
// The pairing code is printed here and shown on the mock host page.

import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = Number(process.argv[2] || process.env.PORT || 8080);
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' };

const devices = new Map(); // cookie token -> device name
const pairings = new Map(); // pairing_id -> { code, name, expires, attempts }
let host = null;
let client = null;

const cookieToken = (req) => /(?:^|;\s*)ws_device=([^;]+)/.exec(req.headers.cookie || '')?.[1];
const paired = (req) => devices.has(cookieToken(req));
const json = (res, status, body, headers = {}) => {
  res.writeHead(status, { 'content-type': 'application/json', ...headers });
  res.end(JSON.stringify(body));
};
const readBody = async (req) => {
  let data = '';
  for await (const chunk of req) data += chunk;
  try { return JSON.parse(data || '{}'); } catch { return {}; }
};
const toHost = (msg) => host?.send(JSON.stringify(msg));

async function api(req, res, url) {
  if (req.method === 'GET' && url.pathname === '/api/session') {
    return paired(req) ? json(res, 200, { paired: true, device: devices.get(cookieToken(req)) }) : json(res, 401, { paired: false });
  }
  if (req.method !== 'POST') return json(res, 405, { error: 'method' });
  const body = await readBody(req);
  if (url.pathname === '/api/pair/start') {
    const id = crypto.randomUUID();
    const code = String(crypto.randomInt(0, 1e6)).padStart(6, '0');
    pairings.set(id, { code, name: String(body.device_name || 'unknown'), expires: Date.now() + 120000, attempts: 3 });
    console.log(`pairing code for "${body.device_name}": ${code}`);
    toHost({ type: 'dev-pairing-code', code });
    return json(res, 200, { pairing_id: id, expires_in: 120 });
  }
  if (url.pathname === '/api/pair/confirm') {
    const p = pairings.get(body.pairing_id);
    if (!p || p.expires < Date.now()) return json(res, 410, { error: 'expired' });
    if (p.code !== body.code) {
      p.attempts--;
      if (!p.attempts) pairings.delete(body.pairing_id);
      return json(res, 403, { error: 'bad_code', attempts_left: p.attempts });
    }
    pairings.delete(body.pairing_id);
    const token = crypto.randomBytes(24).toString('base64url');
    devices.set(token, p.name);
    return json(res, 200, { paired: true }, { 'set-cookie': `ws_device=${token}; HttpOnly; SameSite=Strict; Path=/` });
  }
  if (url.pathname === '/api/unpair') {
    devices.delete(cookieToken(req));
    return json(res, 200, {}, { 'set-cookie': 'ws_device=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0' });
  }
  return json(res, 404, { error: 'not_found' });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  if (url.pathname.startsWith('/api/')) return api(req, res, url);
  const rel = url.pathname === '/' ? 'index.html' : decodeURIComponent(url.pathname).replace(/^\/+/, '');
  const file = path.resolve(root, rel);
  if (!file.startsWith(root + path.sep) || file.includes(`${path.sep}node_modules${path.sep}`)) {
    res.writeHead(403).end();
    return;
  }
  try {
    const body = await fs.readFile(file);
    res.writeHead(200, { 'content-type': types[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});

const wss = new WebSocketServer({ noServer: true });
server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url, 'http://x');
  const isHost = url.searchParams.get('role') === 'host';
  if (url.pathname !== '/signal' || (!isHost && !paired(req))) {
    socket.end('HTTP/1.1 401 Unauthorized\r\n\r\n');
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    if (isHost) { host?.close(); host = ws; } else {
      client?.send(JSON.stringify({ type: 'bye', reason: 'replaced' }));
      client?.close();
      client = ws;
      toHost({ type: 'dev-client-connected' });
    }
    ws.on('message', (data) => (isHost ? client : host)?.send(data.toString()));
    ws.on('close', () => {
      if (isHost && host === ws) host = null;
      if (!isHost && client === ws) { client = null; toHost({ type: 'dev-client-disconnected' }); }
    });
  });
});

server.listen(port, '127.0.0.1', () => console.log(`dev server on http://localhost:${port}`));
