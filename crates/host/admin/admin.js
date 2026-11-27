// The host's settings page: status, pairing, paired devices and settings.

const $ = (id) => document.getElementById(id);
const SVG = 'http://www.w3.org/2000/svg';

/** Settings form fields, by section: [key, label, kind, extra]. */
const FIELDS = [
  ['Network', [
    ['port', 'HTTPS and media port', 'number', { min: 1, max: 65534 }],
    ['ice_tcp', 'ICE-TCP fallback', 'check'],
    ['ice_tcp_port', 'ICE-TCP port', 'number', { min: 1, max: 65535, optional: true, help: 'Empty: the port after the HTTPS port.' }],
    ['extra_udp_ports', 'Extra UDP media ports', 'text', { help: 'Separated by spaces or commas, e.g. 443.' }],
    ['public_ips', 'Public addresses', 'text', { help: 'Addresses that forward to this PC.' }],
    ['hostnames', 'Host names', 'text', { help: 'Names browsers reach this PC by (a DDNS name), for the certificate.' }],
    ['allow_origins', 'Other allowed origins', 'text', { help: 'https://name of a reverse proxy.' }],
    ['upnp', 'Ask the router to forward the ports (UPnP)', 'check'],
  ]],
  ['Cloudflare Tunnel', [
    ['tunnel', 'Tunnel', 'select', { options: [['off', 'Off'], ['named', 'Named (token)'], ['quick', 'Quick (no account)']] }],
    ['tunnel_hostname', 'Named tunnel host name', 'text', { help: 'Empty: the one cloudflared reports.' }],
    ['tunnel_token', 'Named tunnel token', 'password', { help: 'Leave empty to keep the saved token.' }],
  ]],
  ['Devices', [
    ['forget_after_days', 'Unpair a device unused for (days)', 'number', { min: 1, max: 3650 }],
    ['remote_admin', 'Paired devices may open these settings', 'check'],
  ]],
  ['Video', [
    ['display', 'Display', 'number', { min: 0, optional: true, help: 'Empty: the primary display.' }],
    ['codec', 'Codec', 'select', { options: [['h264', 'H.264'], ['h265', 'H.265'], ['av1', 'AV1']] }],
    ['width', 'Width', 'number', { min: 320, max: 7680, step: 2 }],
    ['height', 'Height', 'number', { min: 180, max: 4320, step: 2 }],
    ['fps', 'Frames per second', 'number', { min: 1, max: 240 }],
    ['kbps', 'Bitrate (kb/s)', 'number', { min: 500, max: 200000 }],
    ['quality', 'Encoder effort', 'select', { options: [['speed', 'Speed'], ['balanced', 'Balanced'], ['clarity', 'Clarity']] }],
    ['allow_resolution_change', 'Let the browser change the resolution', 'check'],
  ]],
  ['Audio', [
    ['game_audio', 'Send the PC\'s sound', 'check'],
    ['audio_device', 'Output to capture', 'text', { list: 'audio-devices', help: 'Empty: the default output.' }],
    ['audio_kbps', 'Audio bitrate (kb/s)', 'number', { min: 32, max: 510 }],
    ['audio_frame_ms', 'Audio frame', 'select', { options: [['10', '10 ms (sooner)'], ['20', '20 ms (better)']], number: true }],
    ['mic', 'Play the browser\'s microphone', 'check'],
    ['mic_device', 'Microphone device', 'text', { list: 'audio-devices', help: '"auto" picks a virtual microphone.' }],
  ]],
  ['Input and sharing', [
    ['gamepad', 'Virtual gamepad', 'select', { options: [['xbox360', 'Xbox 360'], ['ds4', 'DualShock 4']] }],
    ['clipboard', 'Share the clipboard (text)', 'check'],
    ['allow_upload', 'Receive files from the browser', 'check', { help: 'Saved in the uploads folder of the data directory.' }],
  ]],
];

let loaded = null;

async function api(path, body) {
  const init = body === undefined
    ? { credentials: 'same-origin' }
    : {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      };
  const res = await fetch(path, init);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(data.error || `HTTP ${res.status}`);
    err.status = res.status;
    throw err;
  }
  return data;
}

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    e.setAttribute(k, v === true ? '' : String(v));
  }
  for (const c of children) e.append(c);
  return e;
}

function when(secs) {
  return secs ? new Date(secs * 1000).toLocaleString() : '';
}

/** Draws QR rows ('1' dark) as an SVG with a quiet zone. */
function qrSvg(rows) {
  const n = rows.length + 8;
  const svg = document.createElementNS(SVG, 'svg');
  svg.setAttribute('viewBox', `0 0 ${n} ${n}`);
  svg.setAttribute('shape-rendering', 'crispEdges');
  const bg = document.createElementNS(SVG, 'rect');
  bg.setAttribute('width', n);
  bg.setAttribute('height', n);
  bg.setAttribute('fill', '#fff');
  svg.append(bg);
  let d = '';
  rows.forEach((row, y) => {
    for (let x = 0; x < row.length; x++) {
      if (row[x] === '1') d += `M${x + 4} ${y + 4}h1v1h-1z`;
    }
  });
  const path = document.createElementNS(SVG, 'path');
  path.setAttribute('d', d);
  path.setAttribute('fill', '#000');
  svg.append(path);
  return svg;
}

async function gate() {
  const who = await api('/api/admin/whoami');
  const key = new URLSearchParams(location.hash.slice(1)).get('key');
  if (key) history.replaceState(null, '', location.pathname);
  if (!who.admin && key && who.local) {
    try {
      await api('/api/admin/login', { token: key });
      return true;
    } catch (e) {
      showGate(`That key was not accepted (${e.message}). Print a new address with "sudo web-stream settings".`);
      return false;
    }
  }
  if (who.admin) return true;
  if (who.device && who.remote_admin) {
    showGate(`This device (${who.device}) is paired with the host. It can open the host's settings: anything changed here applies to the whole PC.`);
    $('confirm').hidden = false;
    return false;
  }
  if (who.device) {
    showGate('Opening the settings from a paired device is turned off on this host. Open them on the PC itself.');
  } else if (who.local) {
    showGate('Open the address that "sudo web-stream settings" prints, with its key.');
  } else {
    showGate('These settings open on the PC itself (run "sudo web-stream settings" there), or on a device paired with it.');
  }
  return false;
}

function showGate(text) {
  $('gate-text').textContent = text;
  $('gate').hidden = false;
  $('admin').hidden = true;
}

async function refreshStatus() {
  const s = await api('/api/admin/status');
  $('version').textContent = `${s.version}${s.service ? ' · service' : ''}`;
  const dl = $('status');
  dl.replaceChildren();
  const row = (k, v) => { if (v) dl.append(el('dt', {}, k), el('dd', {}, v)); };
  row('Addresses', s.addresses.join('  '));
  row('Cloudflare Tunnel', s.tunnel_url);
  row('Certificate SHA-256', `${s.fingerprint}${s.self_signed ? ' (self-signed: browsers warn until you accept it)' : ''}`);
  row('Connected', s.connected || 'nobody');
  row('UPnP', s.upnp);
  if (s.locked) row('Pairing', 'locked after wrong codes: open pairing to unlock');
  $('pair-state').textContent = s.pairing_secs ? `Pairing is open for ${s.pairing_secs} more seconds.` : '';
  const code = $('pair-code');
  code.hidden = !s.code;
  if (s.code) code.textContent = `${s.code.code}  (for ${s.code.device})`;
  if (!s.pairing_secs) $('codes').replaceChildren();

  const body = $('devices').tBodies[0];
  body.replaceChildren();
  for (const d of s.devices) {
    const btn = el('button', { type: 'button', class: 'small' }, 'Unpair');
    btn.addEventListener('click', async () => {
      const self = d.id === s.this_device;
      if (!confirm(`Unpair ${d.name}?${self ? ' This is the device you are using.' : ''}`)) return;
      await api('/api/admin/unpair', { id: d.id });
      await refreshStatus().catch(onError);
    });
    const name = d.id === s.this_device ? `${d.name} (this device)` : d.name;
    body.append(el('tr', {}, el('td', {}, name), el('td', {}, when(d.paired_at)), el('td', {}, when(d.last_used)), el('td', {}, btn)));
  }
  if (!s.devices.length) body.append(el('tr', {}, el('td', { colspan: 4, class: 'note' }, 'No paired devices.')));
}

function buildForm(settings, audioDevices, tokenSet) {
  const form = $('settings');
  form.replaceChildren();
  const list = el('datalist', { id: 'audio-devices' });
  for (const d of ['auto', ...audioDevices]) list.append(el('option', { value: d }));
  form.append(list);
  for (const [legend, fields] of FIELDS) {
    const set = el('fieldset', {}, el('legend', {}, legend));
    for (const [key, label, kind, extra = {}] of fields) {
      const id = `f-${key}`;
      let input;
      if (kind === 'select') {
        input = el('select', { id });
        for (const [v, text] of extra.options) input.append(el('option', { value: v }, text));
        input.value = String(settings[key]);
      } else if (kind === 'check') {
        input = el('input', { id, type: 'checkbox' });
        input.checked = !!settings[key];
      } else {
        input = el('input', { id, type: kind, min: extra.min, max: extra.max, step: extra.step, list: extra.list, autocomplete: 'off' });
        const v = settings[key];
        input.value = v === null || v === undefined ? '' : String(v);
        if (key === 'tunnel_token' && tokenSet) input.placeholder = 'saved';
      }
      input.dataset.key = key;
      input.dataset.kind = kind;
      if (extra.optional) input.dataset.optional = '1';
      if (extra.number) input.dataset.number = '1';
      const field = el('div', { class: 'field' }, el('label', { for: id }, label), input);
      if (extra.help) field.append(el('small', {}, extra.help));
      set.append(field);
    }
    form.append(set);
  }
}

function readForm() {
  const settings = { ...loaded };
  let token = '';
  for (const input of $('settings').querySelectorAll('[data-key]')) {
    const key = input.dataset.key;
    if (key === 'tunnel_token') {
      token = input.value.trim();
      continue;
    }
    if (input.dataset.kind === 'check') settings[key] = input.checked;
    else if (input.dataset.kind === 'number') {
      const v = input.value.trim();
      settings[key] = v === '' && input.dataset.optional ? null : Number(v);
    } else if (input.dataset.number) settings[key] = Number(input.value);
    else settings[key] = input.value;
  }
  return { settings, tunnel_token: token };
}

async function loadSettings() {
  const s = await api('/api/admin/settings');
  loaded = s.settings;
  buildForm(s.settings, s.audio_devices, s.tunnel_token_set);
}

async function save() {
  const state = $('save-state');
  state.className = 'note';
  $('save').disabled = true;
  try {
    await api('/api/admin/settings', readForm());
    state.textContent = 'Saved. The host is restarting…';
    await waitForRestart();
    state.textContent = 'Saved and restarted.';
    await Promise.all([refreshStatus(), loadSettings()]);
  } catch (e) {
    state.className = 'error';
    state.textContent = `Not saved: ${e.message}`;
  } finally {
    $('save').disabled = false;
  }
}

/** Waits for the host to go away and come back. */
async function waitForRestart() {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  let gone = false;
  for (let i = 0; i < 120; i++) {
    await sleep(500);
    try {
      await api('/api/admin/whoami');
      if (gone || i > 10) return;
    } catch {
      gone = true;
    }
  }
  throw new Error('the host did not come back; check "systemctl status web-stream"');
}

async function openPairing() {
  const r = await api('/api/admin/pair', {});
  const box = $('codes');
  box.replaceChildren();
  for (const c of r.codes) {
    const fig = el('figure');
    if (c.rows) fig.append(qrSvg(c.rows));
    fig.append(el('figcaption', {}, c.url));
    box.append(fig);
  }
  await refreshStatus();
}

function onError(e) {
  if (e.status === 401) {
    showGate('Your session on this page ended. Reload to sign in again.');
    return;
  }
  const state = $('save-state');
  state.className = 'error';
  state.textContent = e.message;
}

async function start() {
  if (!(await gate())) return;
  $('gate').hidden = true;
  $('admin').hidden = false;
  $('logout').hidden = false;
  await Promise.all([refreshStatus(), loadSettings()]);
  setInterval(() => refreshStatus().catch(onError), 3000);
}

$('confirm').addEventListener('click', async () => {
  try {
    await api('/api/admin/confirm', {});
    await start();
  } catch (e) {
    showGate(`Not allowed: ${e.message}`);
  }
});
$('logout').addEventListener('click', async () => {
  await api('/api/admin/logout', {}).catch(() => {});
  location.reload();
});
$('pair').addEventListener('click', () => openPairing().catch(onError));
$('save').addEventListener('click', () => save());
$('unpair-all').addEventListener('click', async () => {
  if (!confirm('Unpair every device? Each has to pair again.')) return;
  await api('/api/admin/unpair-all', {}).catch(onError);
  await refreshStatus().catch(onError);
});

start().catch((e) => showGate(`The host did not answer: ${e.message}`));
