import { Session } from './session.js';
import { defaultSignalUrl } from './signaling.js';
import { InputCapture } from './input.js';
import { snapshot, compute, format, FrameTimer } from './stats.js';
import * as pairing from './pairing.js';

const $ = (id) => document.getElementById(id);
const ui = {
  connect: $('connect'), status: $('status'), display: $('display'), mouseMode: $('mouse-mode'),
  fullscreen: $('fullscreen'), statsToggle: $('stats-toggle'), stage: $('stage'), video: $('video'), stats: $('stats'),
  unpair: $('unpair'), pair: $('pair'), deviceName: $('device-name'), pairStart: $('pair-start'),
  pairCode: $('pair-code'), pairConfirm: $('pair-confirm'), pairMsg: $('pair-msg'),
};

const params = new URLSearchParams(location.search);
const signalUrl = defaultSignalUrl();

let session = null;
let input = null;
let statsTimer = 0;
let prevSnap = null;
const frameTimer = new FrameTimer(ui.video);

function setStatus(text, state) {
  ui.status.textContent = text;
  ui.status.dataset.state = state;
}

async function connect() {
  disconnect();
  session = new Session({ signalUrl, video: ui.video });
  session.addEventListener('status', (e) => setStatus(e.detail.text, e.detail.state));
  session.addEventListener('control', (e) => onControl(e.detail));
  session.addEventListener('closed', () => {
    ui.connect.textContent = 'Connect';
    ui.display.disabled = true;
  });
  // The host sends "bye" with reason "revoked" when this browser is unpaired.
  session.addEventListener('status', (e) => { if (/unpaired/.test(e.detail.text)) checkPairing(); });
  input = new InputCapture({ session, video: ui.video, stage: ui.stage, onHotkey });
  input.setMode(ui.mouseMode.value);
  input.start();
  ui.connect.textContent = 'Disconnect';
  if (!ui.stats.hidden) startStats();
  try {
    await session.start();
  } catch (err) {
    session.fail(err.message);
    // A refused upgrade usually means the pairing was revoked; show the pairing screen if so.
    checkPairing();
  }
}

function disconnect() {
  if (input) input.stop();
  if (session) session.close();
  input = null;
  session = null;
  stopStats();
  ui.display.replaceChildren();
  setStatus('Not connected', 'idle');
}

function onControl(msg) {
  if (msg.type === 'displays' && Array.isArray(msg.displays)) {
    ui.display.replaceChildren(...msg.displays.map((d, i) => {
      const o = document.createElement('option');
      o.value = String(d.id);
      const mode = d.width && d.height ? ` ${d.width}x${d.height}${d.refresh ? '@' + Math.round(d.refresh) : ''}` : '';
      o.textContent = `${i + 1}: ${d.name || d.id}${mode}${d.primary ? ' (primary)' : ''}`;
      return o;
    }));
    if (msg.active != null) ui.display.value = String(msg.active);
    ui.display.disabled = msg.displays.length < 2;
  }
}

function selectDisplay(id) {
  if (!session) return;
  ui.display.value = id;
  session.sendControl({ type: 'select-display', id: displayIdOf(id) });
}

// Keep the host's id type (number or string) when echoing it back.
function displayIdOf(value) {
  return /^\d+$/.test(value) ? Number(value) : value;
}

async function toggleFullscreen() {
  if (document.fullscreenElement) {
    await document.exitFullscreen();
    return;
  }
  await ui.stage.requestFullscreen({ navigationUI: 'hide' });
  // Keyboard Lock (Chromium, fullscreen only) lets Esc, Alt+Tab and the Windows key reach the host.
  // Hold Esc to leave fullscreen.
  try { await navigator.keyboard?.lock(); } catch { /* unsupported */ }
}

document.addEventListener('fullscreenchange', () => {
  if (!document.fullscreenElement) navigator.keyboard?.unlock?.();
});

function toggleStats() {
  ui.stats.hidden = !ui.stats.hidden;
  ui.statsToggle.setAttribute('aria-pressed', String(!ui.stats.hidden));
  if (ui.stats.hidden) stopStats();
  else startStats();
}

function startStats() {
  stopStats();
  frameTimer.start();
  ui.stats.textContent = 'waiting for stream…';
  statsTimer = setInterval(async () => {
    const pc = session?.pc;
    if (!pc) return;
    const snap = snapshot(await pc.getStats(), performance.now());
    if (prevSnap) ui.stats.textContent = format(compute(prevSnap, snap), frameTimer.avgMs);
    prevSnap = snap;
  }, 500);
}

function stopStats() {
  clearInterval(statsTimer);
  frameTimer.stop();
  prevSnap = null;
}

function setMouseMode(mode) {
  ui.mouseMode.value = mode;
  input?.setMode(mode);
}

// Ctrl+Alt+Shift+<key>. Returns true when handled so the key is not sent to the host.
function onHotkey(code) {
  const digit = /^Digit([1-9])$/.exec(code);
  if (digit) {
    const opt = ui.display.options[Number(digit[1]) - 1];
    if (opt) selectDisplay(opt.value);
    return true;
  }
  switch (code) {
    case 'KeyS': toggleStats(); return true;
    case 'KeyF': toggleFullscreen(); return true;
    case 'KeyM': setMouseMode(ui.mouseMode.value === 'relative' ? 'absolute' : 'relative'); return true;
    case 'KeyQ': document.exitPointerLock(); return true;
  }
  return false;
}

ui.connect.addEventListener('click', () => (session && !session.closed ? disconnect() : connect()));
ui.display.addEventListener('change', () => selectDisplay(ui.display.value));
ui.mouseMode.addEventListener('change', () => setMouseMode(ui.mouseMode.value));
ui.fullscreen.addEventListener('click', toggleFullscreen);
ui.statsToggle.addEventListener('click', toggleStats);
// Toolbar controls keep focus otherwise, and Space/Enter would re-trigger them.
for (const el of [ui.connect, ui.fullscreen, ui.statsToggle]) el.addEventListener('mouseup', () => el.blur());

// Pairing: the host sets an HttpOnly cookie once the code shown on the host is confirmed.
let pairingId = null;

function showPairing(paired) {
  document.body.classList.toggle('unpaired', !paired);
  ui.pair.hidden = paired;
  ui.unpair.hidden = !paired;
  if (!paired) {
    disconnect();
    setStatus('Not paired', 'idle');
    if (!ui.deviceName.value) ui.deviceName.value = pairing.defaultDeviceName();
  }
}

async function checkPairing() {
  try {
    const s = await pairing.getSession();
    showPairing(s.paired);
    return s.paired;
  } catch {
    setStatus('Host unreachable', 'failed');
    return false;
  }
}

ui.pairStart.addEventListener('click', async () => {
  ui.pairMsg.textContent = '';
  try {
    const r = await pairing.startPairing(ui.deviceName.value.trim() || pairing.defaultDeviceName());
    pairingId = r.pairingId;
    ui.pairCode.disabled = false;
    ui.pairConfirm.disabled = false;
    ui.pairCode.value = '';
    ui.pairCode.focus();
    ui.pairMsg.textContent = `Enter the code shown on the host (valid ${r.expiresIn ?? 120} s).`;
  } catch (err) {
    ui.pairMsg.textContent = err.message;
  }
});

ui.pair.addEventListener('submit', async (e) => {
  e.preventDefault();
  if (!pairingId) return;
  try {
    await pairing.confirmPairing(pairingId, ui.pairCode.value.trim());
    pairingId = null;
    ui.pairCode.disabled = true;
    ui.pairConfirm.disabled = true;
    ui.pairMsg.textContent = '';
    showPairing(true);
    connect();
  } catch (err) {
    ui.pairMsg.textContent = err.message;
  }
});

ui.unpair.addEventListener('click', async () => {
  if (!confirm('Unpair this browser? You will need a new code from the host to connect again.')) return;
  disconnect();
  await pairing.unpair().catch(() => {});
  showPairing(false);
});

checkPairing().then((paired) => { if (paired && params.has('autoconnect')) connect(); });
