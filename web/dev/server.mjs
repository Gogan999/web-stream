// Development server: serves web/ and relays signaling between one mock host page
// (/signal?role=host) and one client (/signal). Not for production; the real host
// program serves the client and terminates signaling itself.
//
//   node dev/server.mjs [port]     then open http://localhost:8080/dev/mock-host.html
//                                  and http://localhost:8080/

import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = Number(process.argv[2] || process.env.PORT || 8080);
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' };

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
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

let host = null;
let client = null;
const wss = new WebSocketServer({ server, path: '/signal' });
wss.on('connection', (ws, req) => {
  const isHost = new URL(req.url, 'http://x').searchParams.get('role') === 'host';
  if (isHost) { host?.close(); host = ws; } else { client?.close(); client = ws; }
  const peer = () => (isHost ? client : host);
  if (!isHost) host?.send(JSON.stringify({ type: 'client-connected' }));
  ws.on('message', (data) => peer()?.send(data.toString()));
  ws.on('close', () => {
    if (isHost && host === ws) host = null;
    if (!isHost && client === ws) { client = null; host?.send(JSON.stringify({ type: 'client-disconnected' })); }
  });
});

server.listen(port, () => console.log(`dev server on http://localhost:${port}`));
