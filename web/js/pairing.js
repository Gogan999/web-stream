// Pairing API client. Same origin by cookie, or another PC's origin by device token.
// See docs/dev/signaling.md.

async function call(method, path, body, { origin = '', token = null } = {}) {
  const remote = !!origin;
  const headers = {};
  if (body) headers['Content-Type'] = 'application/json';
  if (token) headers.Authorization = `Bearer ${token}`;
  const res = await fetch(origin + path, {
    method,
    mode: remote ? 'cors' : 'same-origin',
    credentials: remote ? 'omit' : 'same-origin',
    cache: 'no-store',
    headers,
    body: body ? JSON.stringify(remote ? { ...body, want_token: true } : body) : undefined,
  });
  let data = {};
  try { data = await res.json(); } catch { /* empty body */ }
  return { status: res.status, data };
}

export async function getSession(target) {
  const { status, data } = await call('GET', '/api/session', null, target);
  return status === 200 && data.paired ? { paired: true, device: data.device } : { paired: false };
}

export async function startPairing(deviceName, target) {
  const { status, data } = await call('POST', '/api/pair/start', { device_name: deviceName }, target);
  if (status === 200) return { pairingId: data.pairing_id, expiresIn: data.expires_in };
  throw new Error(pairingError(status, data));
}

// Resolves to the device token when pairing another PC, true for this one.
export async function confirmPairing(pairingId, code, target) {
  const { status, data } = await call('POST', '/api/pair/confirm', { pairing_id: pairingId, code }, target);
  if (status === 200) return data.token ?? true;
  if (status === 403 && data.error === 'bad_code') throw new Error(`Wrong code, ${data.attempts_left ?? 0} attempts left`);
  throw new Error(pairingError(status, data));
}

export async function pairWithLink(link, deviceName, target) {
  const { status, data } = await call('POST', '/api/pair/link', { link, device_name: deviceName }, target);
  if (status === 200) return data.token ?? true;
  if (status === 403 && data.error === 'pairing_closed') {
    throw new Error('This pairing QR code has expired or was used. Press "Pair a device" on the host for a new one.');
  }
  throw new Error(pairingError(status, data));
}

// Takes `#pair=<secret>` off the address: null when there is none, '' when it is damaged.
export function takePairingLink(loc = location, hist = history) {
  if (!loc.hash.startsWith('#pair=')) return null;
  const m = /^#pair=([A-Za-z0-9_-]{16,64})$/.exec(loc.hash);
  hist.replaceState(null, '', loc.pathname + loc.search);
  return m ? m[1] : '';
}

export function pairingError(status, data = {}) {
  if (status === 403 && data.error === 'pairing_closed') return 'Pairing is closed. Press "Pair a device" on the host, then request a code here.';
  if (status === 409) return 'Another device is pairing right now; try again in a minute.';
  if (status === 423) return 'Too many wrong codes. Press "Pair a device" on the host again, then request a new code.';
  if (status === 410) return 'That code expired. Press Request code for a new one.';
  if (status === 429) return 'Too many attempts, wait a minute and try again';
  return `Pairing failed (${status})`;
}

export async function unpair(target) {
  await call('POST', '/api/unpair', {}, target);
}

export function defaultDeviceName(ua = navigator.userAgent) {
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Chrome\/|CriOS\//.test(ua) ? 'Chrome'
    : /Safari\//.test(ua) ? 'Safari' : 'Browser';
  const os = /Windows/.test(ua) ? 'Windows' : /iPhone/.test(ua) ? 'iPhone' : /iPad/.test(ua) ? 'iPad'
    : /Mac OS X/.test(ua) ? 'macOS' : /Android/.test(ua) ? 'Android'
    : /CrOS/.test(ua) ? 'ChromeOS' : /Linux/.test(ua) ? 'Linux' : 'unknown OS';
  return `${browser} on ${os}`;
}
