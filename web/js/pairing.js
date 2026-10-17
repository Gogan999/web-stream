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
  throw new Error(status === 429 ? 'Too many attempts, wait a minute and try again' : `Pairing failed (${status})`);
}

export async function confirmPairing(pairingId, code) {
  const { status, data } = await call('POST', '/api/pair/confirm', { pairing_id: pairingId, code });
  if (status === 200) return true;
  if (status === 403) throw new Error(`Wrong code, ${data.attempts_left ?? 0} attempts left`);
  if (status === 410) throw new Error('Code expired, request a new one');
  if (status === 429) throw new Error('Too many attempts, wait a minute and try again');
  throw new Error(`Pairing failed (${status})`);
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
