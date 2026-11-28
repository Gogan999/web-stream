// App shell cache: the page opens from cache when its own PC is off.

const CACHE = 'web-stream-shell-v1';
const NETWORK_TIMEOUT_MS = 4000;
const SHELL = [
  './', 'index.html', 'style.css', 'manifest.webmanifest', 'icons/icon-192.png', 'icons/apple-touch-icon.png',
  'js/autotune.js', 'js/capabilities.js', 'js/clipboard.js', 'js/cursor.js', 'js/decoders.js', 'js/diag.js', 'js/files.js',
  'js/gamepad.js', 'js/health.js', 'js/hosts.js', 'js/input.js', 'js/keymap.js', 'js/main.js', 'js/pairing.js',
  'js/picture.js', 'js/popout.js', 'js/protocol.js', 'js/session.js', 'js/settings.js', 'js/signaling.js',
  'js/softkeys.js', 'js/stats.js', 'js/telemetry.js', 'js/touch.js', 'js/watchdog.js', 'js/zoom.js',
];
const BYPASS = /^\/(api\/|ws$|ca\.crt$)/;

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).catch(() => {}).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
    .then(() => self.clients.claim()));
});

function withTimeout(promise, ms) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('timeout')), ms);
    promise.then((v) => { clearTimeout(t); resolve(v); }, (e) => { clearTimeout(t); reject(e); });
  });
}

// Network first; the cache when the host doesn't answer.
async function shell(request) {
  const cache = await caches.open(CACHE);
  try {
    const res = await withTimeout(fetch(request), NETWORK_TIMEOUT_MS);
    if (res.ok) await cache.put(request, res.clone());
    return res;
  } catch (err) {
    const hit = await cache.match(request, { ignoreSearch: true })
      ?? (request.mode === 'navigate' ? await cache.match('index.html') : undefined);
    if (hit) return hit;
    throw err;
  }
}

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== self.location.origin || BYPASS.test(url.pathname)) return;
  e.respondWith(shell(e.request));
});
