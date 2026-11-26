// Saved PCs: address, name and device token for each host this app can connect to.

export const LOCAL = 'local';
const KEY = 'ws-hosts';
const LAST_KEY = 'ws-host-last';
const LOCAL_NAME_KEY = 'ws-host-local-name';
export const DEFAULT_PORT = 8443;

function read(storage, key, fallback) {
  try {
    const v = storage?.getItem(key);
    return v == null ? fallback : JSON.parse(v);
  } catch {
    return fallback;
  }
}

function write(storage, key, value) {
  try { storage?.setItem(key, JSON.stringify(value)); } catch { /* storage unavailable */ }
}

const valid = (h) => h && typeof h.id === 'string' && typeof h.origin === 'string' && h.id !== LOCAL;

export function loadHosts(storage = globalThis.localStorage) {
  const list = read(storage, KEY, []);
  return Array.isArray(list) ? list.filter(valid) : [];
}

export function saveHosts(list, storage = globalThis.localStorage) {
  write(storage, KEY, list.filter(valid));
}

export function lastHost(storage = globalThis.localStorage) {
  const id = read(storage, LAST_KEY, null);
  return typeof id === 'string' ? id : null;
}

export function setLastHost(id, storage = globalThis.localStorage) {
  write(storage, LAST_KEY, id);
}

export function localName(storage = globalThis.localStorage) {
  const n = read(storage, LOCAL_NAME_KEY, null);
  return typeof n === 'string' ? n : null;
}

export function setLocalName(name, storage = globalThis.localStorage) {
  write(storage, LOCAL_NAME_KEY, name);
}

// Address or pairing link -> { origin, link }; null when it is not one.
// "192.168.1.20" and "pc.lan" get the host port; "https://name" keeps its own.
export function parseTarget(text) {
  const raw = String(text ?? '').trim();
  if (!raw) return null;
  const hasScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(raw);
  let url;
  try {
    url = new URL(hasScheme ? raw : `https://${raw}`);
  } catch {
    return null;
  }
  if (!/^https?:$/.test(url.protocol) || !url.hostname) return null;
  if (!hasScheme && !url.port) url.port = String(DEFAULT_PORT);
  const m = /^#pair=([A-Za-z0-9_-]{16,64})$/.exec(url.hash);
  return { origin: url.origin, link: m ? m[1] : null };
}

export function signalUrlFor(origin) {
  const u = new URL(origin);
  return `${u.protocol === 'https:' ? 'wss:' : 'ws:'}//${u.host}/ws`;
}

export function hostLabel(origin) {
  try { return new URL(origin).host; } catch { return origin; }
}

function newId() {
  const b = new Uint8Array(8);
  globalThis.crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
}

// Adds or replaces the entry for `origin`; returns [list, entry].
export function upsertHost(list, { origin, token, name }, now = Date.now()) {
  const existing = list.find((h) => h.origin === origin);
  const entry = {
    id: existing?.id ?? newId(),
    origin,
    token: token ?? existing?.token ?? null,
    name: name || existing?.name || hostLabel(origin),
    added: existing?.added ?? now,
  };
  return [[...list.filter((h) => h.origin !== origin), entry], entry];
}

export function removeHost(list, id) {
  return list.filter((h) => h.id !== id);
}

export function renameHost(list, id, name) {
  if (!name) return list;
  return list.map((h) => (h.id === id ? { ...h, name } : h));
}

export function forgetToken(list, id) {
  return list.map((h) => (h.id === id ? { ...h, token: null } : h));
}
