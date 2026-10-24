// Pairing API client (same origin). See docs/signaling.md.

async function call(method, path, body) {
  const res = await fetch(path, {
    method,
    credentials: 'same-origin',
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  let data = {};
  try { data = await res.json(); } catch { /* empty body */ }
  return { status: res.status, data };
}

export async function getSession() {
  const { status, data } = await call('GET', '/api/session');
  return status === 200 && data.paired ? { paired: true, device: data.device } : { paired: false };
}

export async function startPairing(deviceName) {
  const { status, data } = await call('POST', '/api/pair/start', { device_name: deviceName });
  if (status === 200) return { pairingId: data.pairing_id, expiresIn: data.expires_in };
  throw new Error(pairingError(status, data));
}

export async function confirmPairing(pairingId, code) {
  const { status, data } = await call('POST', '/api/pair/confirm', { pairing_id: pairingId, code });
  if (status === 200) return true;
  if (status === 403 && data.error === 'bad_code') throw new Error(`Wrong code, ${data.attempts_left ?? 0} attempts left`);
  throw new Error(pairingError(status, data));
}

// The secret from a pairing link (the host's QR code): pairs at once while pairing is open.
export async function pairWithLink(link, deviceName) {
  const { status, data } = await call('POST', '/api/pair/link', { link, device_name: deviceName });
  if (status === 200) return true;
  if (status === 403 && data.error === 'pairing_closed') {
    throw new Error('This pairing QR code has expired or was used. Press "Pair a browser" on the host for a new one.');
  }
  throw new Error(pairingError(status, data));
}

// Takes `#pair=<secret>` off the address, so it doesn't stay in history or a bookmark. null when
// there is none, '' when it is damaged (cut short by a QR reader, say).
export function takePairingLink(loc = location, hist = history) {
  if (!loc.hash.startsWith('#pair=')) return null;
  // Read before replaceState, which empties location.hash.
  const m = /^#pair=([A-Za-z0-9_-]{16,64})$/.exec(loc.hash);
  hist.replaceState(null, '', loc.pathname + loc.search);
  return m ? m[1] : '';
}

export function pairingError(status, data = {}) {
  if (status === 403 && data.error === 'pairing_closed') return 'Pairing is closed. Open pairing on the host first, then request a code.';
  if (status === 409) return 'Another pairing is in progress. Try again in a couple of minutes, or reopen pairing on the host.';
  if (status === 423) return 'Too many wrong codes. Reopen pairing on the host, then request a new code.';
  if (status === 410) return 'Code expired, request a new one';
  if (status === 429) return 'Too many attempts, wait a minute and try again';
  return `Pairing failed (${status})`;
}

export async function unpair() {
  await call('POST', '/api/unpair', {});
}

export function defaultDeviceName(ua = navigator.userAgent) {
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Chrome\/|CriOS\//.test(ua) ? 'Chrome'
    : /Safari\//.test(ua) ? 'Safari' : 'Browser';
  const os = /Windows/.test(ua) ? 'Windows' : /iPhone/.test(ua) ? 'iPhone' : /iPad/.test(ua) ? 'iPad'
    : /Mac OS X/.test(ua) ? 'macOS' : /Android/.test(ua) ? 'Android'
    : /CrOS/.test(ua) ? 'ChromeOS' : /Linux/.test(ua) ? 'Linux' : 'unknown OS';
  return `${browser} on ${os}`;
}
