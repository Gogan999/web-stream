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
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Chrome\//.test(ua) ? 'Chrome' : 'Browser';
  const os = /Windows/.test(ua) ? 'Windows' : /Mac OS X/.test(ua) ? 'macOS' : /Android/.test(ua) ? 'Android'
    : /CrOS/.test(ua) ? 'ChromeOS' : /Linux/.test(ua) ? 'Linux' : 'unknown OS';
  return `${browser} on ${os}`;
}
