import { Session } from './session.js';
import { defaultSignalUrl } from './signaling.js';
import { InputCapture } from './input.js';
import { hidUsage } from './keymap.js';
import { CursorView } from './cursor.js';
import { snapshot, compute, format, formatHost, FrameTimer } from './stats.js';
import * as pairing from './pairing.js';
import * as settingsStore from './settings.js';
import { AutoTune, Cancelled } from './autotune.js';
import { SoftKeyboard } from './softkeys.js';
import * as P from './protocol.js';
import { Files, Uploads, formatSize } from './files.js';
import { DiagLog, diagEnabled } from './diag.js';
import { ZoomView } from './zoom.js';

const $ = (id) => document.getElementById(id);
const ui = {
  connect: $('connect'), status: $('status'), display: $('display'), mouseMode: $('mouse-mode'),
  fullscreen: $('fullscreen'), statsToggle: $('stats-toggle'), stage: $('stage'), video: $('video'), stats: $('stats'),
  cursor: $('cursor'),
  unpair: $('unpair'), pair: $('pair'), deviceName: $('device-name'), pairStart: $('pair-start'),
  pairCode: $('pair-code'), pairConfirm: $('pair-confirm'), pairMsg: $('pair-msg'),
  resolution: $('resolution'), bitrate: $('bitrate'), fps: $('fps'), quality: $('quality'),
  keyframeScale: $('keyframe-scale'), recoverDrops: $('recover-drops'),
  matchHost: $('match-host'), autotune: $('autotune'), mic: $('mic'),
  audioKbps: $('audio-kbps'), audioFrame: $('audio-frame'),
  moreToggle: $('more-toggle'), more: $('more'), kbdToggle: $('kbd-toggle'), keysToggle: $('keys-toggle'), keys: $('keys'),
  kbdInput: $('kbd-input'), touchFullscreen: $('touch-fullscreen'), view: $('view'), zoomReset: $('zoom-reset'),
  filesToggle: $('files-toggle'), files: $('files'), fileList: $('file-list'), fromHost: $('from-host'),
  toHost: $('to-host'), uploadPick: $('upload-pick'), uploadInput: $('upload-input'), uploadList: $('upload-list'),
  diagSave: $('diag-save'),
};

const params = new URLSearchParams(location.search);
const signalUrl = defaultSignalUrl();
const diag = diagEnabled(params, globalThis.localStorage) ? new DiagLog() : null;
if (diag) {
  ui.diagSave.hidden = false;
  ui.diagSave.addEventListener('click', () => diag.save());
}

const zoom = new ZoomView({ el: ui.view, stage: ui.stage, onChange: (on) => { ui.zoomReset.hidden = !on; } });
ui.zoomReset.addEventListener('click', () => zoom.reset());

let session = null;
let input = null;
let cursor = null;
let statsTimer = 0;
let prevSnap = null;
const frameTimer = new FrameTimer(ui.video);

// Stream settings and what the host reported. See settings.js.
const settings = settingsStore.load();
let host = null; // { displays, active, config, stats } for the current session
let lastSent = null;
let tuning = null; // running AutoTune
let tuneWindow = null; // host stats gathered while auto-tune measures
let micTrack = null; // this browser's microphone while the Mic setting is on and a session runs

function setStatus(text, state) {
  ui.status.textContent = text;
  ui.status.dataset.state = state;
}

async function connect() {
  disconnect();
  session = new Session({ signalUrl, video: ui.video });
  session.addEventListener('status', (e) => setStatus(e.detail.text, e.detail.state));
  session.addEventListener('control', (e) => onControl(e.detail));
  session.addEventListener('file-chunk', (e) => files.onChunk(e.detail));
  session.addEventListener('control-open', () => {
    // A new connection: the host offers its files afresh, and says whether it takes any.
    files.clear();
    uploads.clear();
    if (diag && session.pc) diag.start(session.pc, ui.video);
    session.sendControl({ type: 'listDisplays' });
    sendCursorMode();
    // The host keeps the last choice across sessions; tell it this browser's.
    session.sendControl({ type: 'setMicEnabled', enabled: settings.mic });
    session.sendControl(settingsStore.audioConfig(settings));
  });
  session.addEventListener('closed', () => {
    ui.connect.textContent = 'Connect';
    ui.display.disabled = true;
    tuning?.cancel();
    files.clear();
    uploads.clear();
  });
  host = { displays: [], active: null, config: null, stats: null, matched: false, awaitingMatch: false };
  lastSent = null;
  // The host sends "bye" with reason "revoked" when this browser is unpaired.
  session.addEventListener('status', (e) => { if (/unpaired/.test(e.detail.text)) checkPairing(); });
  input = new InputCapture({
    session, video: ui.video, stage: ui.stage, onHotkey,
    keyTarget: ui.kbdInput, onKeySent: (usage, down) => softKeys.keySent(usage, down),
    onReleaseAll: () => softKeys.forget(),
    displaySize: () => host?.displays.find((d) => d.id === host.active) ?? null,
    zoom,
  });
  input.setMode(ui.mouseMode.value);
  input.start();
  cursor = new CursorView({
    video: ui.video, overlay: ui.cursor, displaySize: () => host?.displays.find((d) => d.id === host.active) ?? null,
    onPlace: (x, y) => zoom.follow(x, y), // zoomed in, the view keeps the trackpad's pointer in sight
  });
  cursor.setMode(ui.mouseMode.value);
  ui.connect.textContent = 'Disconnect';
  if (!ui.stats.hidden) startStats();
  if (settings.mic) setMic(true);
  try {
    await session.start();
  } catch (err) {
    session.fail(err.message);
    // A refused upgrade usually means the pairing was revoked; show the pairing screen if so.
    checkPairing();
  }
}

function disconnect() {
  diag?.stop();
  if (input) input.stop();
  if (session) session.close();
  cursor?.stop();
  zoom.reset();
  micTrack?.stop();
  micTrack = null;
  input = null;
  cursor = null;
  session = null;
  stopStats();
  tuning?.cancel();
  files.clear();
  uploads.clear();
  host = null;
  ui.display.replaceChildren();
  setStatus('Not connected', 'idle');
}

function onControl(msg) {
  if (cursor?.onControl(msg)) return;
  if (files.onControl(msg)) return;
  if (uploads.onControl(msg)) return;
  if (msg.type === 'displays' && Array.isArray(msg.displays)) {
    ui.display.replaceChildren(...msg.displays.map((d, i) => {
      const o = document.createElement('option');
      o.value = String(d.id);
      const hz = d.refreshMhz ? '@' + Math.round(d.refreshMhz / 1000) : '';
      const mode = d.width && d.height ? ` ${d.width}x${d.height}${hz}` : '';
      o.textContent = `${i + 1}: ${d.name || d.id}${mode}${d.primary ? ' (primary)' : ''}`;
      return o;
    }));
    if (msg.active != null) ui.display.value = String(msg.active);
    ui.display.disabled = msg.displays.length < 2;
    if (host) {
      host.displays = msg.displays;
      if (msg.active != null) host.active = msg.active;
      host.awaitingMatch = false;
      applySettings();
    }
  } else if (msg.type === 'displayChanged' && msg.id != null) {
    ui.display.value = String(msg.id);
    if (host) {
      host.active = msg.id;
      if (msg.config) host.config = msg.config;
      applySettings();
    }
  } else if (msg.type === 'videoStats' && host) {
    host.stats = msg;
    diag?.add('host_stats', msg);
    if (tuneWindow) tuneWindow.push(msg);
  } else if (msg.type === 'micUnavailable') {
    // The host can't play the mic: stop sending it.
    setStatus(`Host: ${msg.message}`, ui.status.dataset.state);
    settings.mic = false;
    ui.mic.checked = false;
    settingsStore.save(settings);
    setMic(false);
  } else if (msg.type === 'error' && typeof msg.message === 'string') {
    setStatus(`Host: ${msg.message}`, ui.status.dataset.state);
    if (host) {
      // Whatever was asked for may not have applied; let the next change (or this one) go again.
      lastSent = null;
      if (host.awaitingMatch) {
        host.awaitingMatch = false;
        applySettings();
      }
    }
  }
}

// Asks the host for the stream the settings describe, once the host has said what it streams now.
function applySettings() {
  if (!session || !host?.config || tuning) return;
  if (settings.matchHost && !host.matched) {
    host.matched = true;
    host.awaitingMatch = true;
    const { width, height } = settingsStore.screenPixels();
    // The host answers with a fresh display list (or an error), which brings us back here; the
    // stream is sized for the new resolution then, not twice.
    session.sendControl({ type: 'setHostResolution', width, height });
  }
  if (host.awaitingMatch) return;
  sendConfig(currentConfig());
}

function currentConfig() {
  const display = host.displays.find((d) => d.id === host.active) ?? null;
  return settingsStore.videoConfig(settings, { screen: settingsStore.screenPixels(), display, codec: host.config.codec, rotate: isTouch() });
}

function sendConfig(config) {
  if (settingsStore.sameConfig(config, host.config) || settingsStore.sameConfig(config, lastSent)) return;
  lastSent = config;
  session.sendControl({ type: 'setVideoConfig', config });
}

function fillSelect(select, values, label) {
  select.replaceChildren(...values.map((v) => {
    const o = document.createElement('option');
    o.value = String(v);
    o.textContent = label(v);
    return o;
  }));
}

const RES_LABELS = { screen: 'Match this screen', native: 'Host native', 2160: '2160p', 1440: '1440p', 1080: '1080p', 720: '720p' };
fillSelect(ui.resolution, settingsStore.RESOLUTIONS, (v) => RES_LABELS[v]);
fillSelect(ui.bitrate, ['auto', ...settingsStore.BITRATES_KBPS], (v) => (v === 'auto' ? 'Auto' : `${v / 1000} Mb/s`));
fillSelect(ui.fps, settingsStore.FRAME_RATES, (v) => String(v));
fillSelect(ui.quality, settingsStore.QUALITIES, (v) => v[0].toUpperCase() + v.slice(1));
fillSelect(ui.keyframeScale, settingsStore.KEYFRAME_SCALES, (v) => (v === 1 ? '1x (fastest)' : v === 4 ? '4x (sharpest)' : `${v}x`));
fillSelect(ui.audioKbps, settingsStore.AUDIO_KBPS, (v) => (v === 'host' ? 'Host setting' : `${v} kb/s`));
fillSelect(ui.audioFrame, settingsStore.AUDIO_FRAMES, (v) => (v === 'host' ? 'Host setting' : v === '10' ? '10 ms (lowest delay)' : '20 ms (better sound)'));

// Shows the settings; a bitrate auto-tune picked that isn't in the list gets its own entry.
function showSettings() {
  if (![...ui.bitrate.options].some((o) => o.value === String(settings.bitrate))) {
    const o = document.createElement('option');
    o.value = String(settings.bitrate);
    o.textContent = `${Number(settings.bitrate) / 1000} Mb/s`;
    ui.bitrate.append(o);
  }
  ui.resolution.value = settings.resolution;
  ui.bitrate.value = String(settings.bitrate);
  ui.fps.value = String(settings.fps);
  ui.quality.value = settings.quality;
  ui.keyframeScale.value = String(settings.keyframeScale);
  ui.recoverDrops.checked = !!settings.recoverDrops;
  ui.matchHost.checked = !!settings.matchHost;
  ui.mic.checked = !!settings.mic;
  ui.audioKbps.value = settings.audioKbps;
  ui.audioFrame.value = settings.audioFrame;
}
showSettings();

for (const [el, key] of [[ui.resolution, 'resolution'], [ui.bitrate, 'bitrate'], [ui.fps, 'fps'], [ui.quality, 'quality'], [ui.keyframeScale, 'keyframeScale']]) {
  el.addEventListener('change', () => {
    settings[key] = key === 'fps' || key === 'keyframeScale' ? Number(el.value) : el.value;
    settingsStore.save(settings);
    applySettings();
  });
}

ui.recoverDrops.addEventListener('change', () => {
  settings.recoverDrops = ui.recoverDrops.checked;
  settingsStore.save(settings);
  applySettings();
});

ui.matchHost.addEventListener('change', () => {
  settings.matchHost = ui.matchHost.checked;
  settingsStore.save(settings);
  if (!host) return;
  if (settings.matchHost) {
    host.matched = false;
    applySettings();
  } else if (host.matched) {
    host.matched = false;
    session?.sendControl({ type: 'restoreHostResolution' });
  }
});

// Starts or stops sending the mic. The host plays it into its virtual cable (see docs/host-setup.md).
let micGen = 0;
async function setMic(on) {
  const s = session;
  const gen = ++micGen;
  micTrack?.stop();
  micTrack = null;
  if (!s) return;
  let track = null;
  if (on) {
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      });
      track = stream.getAudioTracks()[0];
    } catch (err) {
      if (gen !== micGen || s !== session) return;
      setStatus(`Mic unavailable: ${err.message}`, ui.status.dataset.state);
      on = false;
      settings.mic = false;
      ui.mic.checked = false;
      settingsStore.save(settings);
    }
    // Disconnected, or toggled again, while the browser asked for permission.
    if (gen !== micGen || s !== session) {
      track?.stop();
      return;
    }
  }
  micTrack = track;
  await s.setMicTrack(track);
  s.sendControl({ type: 'setMicEnabled', enabled: on });
}

for (const [el, key] of [[ui.audioKbps, 'audioKbps'], [ui.audioFrame, 'audioFrame']]) {
  el.addEventListener('change', () => {
    settings[key] = el.value;
    settingsStore.save(settings);
    session?.sendControl(settingsStore.audioConfig(settings));
  });
}

ui.mic.addEventListener('change', () => {
  settings.mic = ui.mic.checked;
  settingsStore.save(settings);
  setMic(settings.mic);
});

// Health of the stream over `ms`, from the browser's stats and the host's videoStats.
async function measureWindow(ms) {
  const pc = session?.pc;
  if (!pc) throw new Cancelled();
  const a = snapshot(await pc.getStats(), performance.now());
  tuneWindow = [];
  await new Promise((r) => setTimeout(r, ms));
  const got = tuneWindow;
  tuneWindow = null;
  if (!session?.pc) throw new Cancelled();
  const b = snapshot(await session.pc.getStats(), performance.now());
  const s = compute(a, b);
  const d = (k) => (b.video?.[k] ?? 0) - (a.video?.[k] ?? 0);
  const received = d('packetsReceived');
  const clientLoss = received + s.lostDelta > 0 ? s.lostDelta / (received + s.lostDelta) : null;
  const hostLoss = got.map((h) => h.loss).filter((l) => l != null);
  const encode = got.map((h) => h.encodeMs).filter((e) => e > 0);
  const last = got.at(-1);
  return {
    freezes: d('freezeCount'),
    keyframeRequests: got.reduce((n, h) => n + (h.keyframeRequests ?? 0), 0),
    loss: Math.max(clientLoss ?? 0, ...hostLoss),
    bweKbps: last?.bweKbps ?? null,
    hostFps: got.length ? got.reduce((x, h) => x + (h.fps ?? 0), 0) / got.length : null,
    encodeMs: encode.length ? encode.reduce((x, y) => x + y, 0) / encode.length : null,
    decodeMs: s.decodeMs,
  };
}

async function autoTune() {
  if (tuning) {
    tuning.cancel();
    return;
  }
  if (!session?.pc || !host?.config) {
    setStatus('Connect first, then auto-tune', ui.status.dataset.state);
    return;
  }
  const base = currentConfig();
  tuning = new AutoTune({
    base,
    apply: (config) => {
      lastSent = config;
      session?.sendControl({ type: 'setVideoConfig', config });
    },
    measure: measureWindow,
    onProgress: (text) => setStatus(text, 'connecting'),
  });
  ui.autotune.textContent = 'Stop auto-tune';
  for (const el of [ui.resolution, ui.bitrate, ui.fps, ui.quality, ui.keyframeScale, ui.recoverDrops, ui.display, ui.matchHost]) el.disabled = true;
  try {
    const { config, summary } = await tuning.run();
    settings.bitrate = String(config.bitrateKbps);
    settings.quality = config.quality;
    settingsStore.save(settings);
    setStatus(summary, 'connected');
  } catch (err) {
    if (err instanceof Cancelled) setStatus('Auto-tune stopped', session ? 'connected' : 'idle');
    else setStatus(`Auto-tune failed: ${err.message}`, session ? 'connected' : 'idle');
  } finally {
    tuning = null;
    tuneWindow = null;
    ui.autotune.textContent = 'Auto-tune (test)';
    for (const el of [ui.resolution, ui.bitrate, ui.fps, ui.quality, ui.keyframeScale, ui.recoverDrops, ui.matchHost]) el.disabled = false;
    ui.display.disabled = ui.display.options.length < 2;
    showSettings();
    if (session) applySettings();
  }
}

ui.autotune.addEventListener('click', autoTune);

function selectDisplay(id) {
  if (!session) return;
  ui.display.value = id;
  zoom.reset(); // a different screen: start from the whole of it
  session.sendControl({ type: 'selectDisplay', id: Number(id) });
}

const fullscreenElement = () => document.fullscreenElement ?? document.webkitFullscreenElement ?? null;
const isTouch = () => document.body.classList.contains('touch');

async function toggleFullscreen() {
  if (document.body.classList.contains('fake-fs')) {
    setFakeFullscreen(false);
    return;
  }
  if (fullscreenElement()) {
    await (document.exitFullscreen ?? document.webkitExitFullscreen).call(document);
    return;
  }
  const request = ui.stage.requestFullscreen ?? ui.stage.webkitRequestFullscreen;
  try {
    await request.call(ui.stage, { navigationUI: 'hide' });
    // The old webkit call returns nothing and may quietly do nothing.
    if (!fullscreenElement()) await new Promise((r) => setTimeout(r, 300));
    if (!fullscreenElement()) throw new Error('not fullscreen');
  } catch {
    // iPhone Safari only fullscreens <video>, with its own controls and no input; fill the window instead.
    // On desktop a refusal just leaves things as they were.
    if (isTouch()) setFakeFullscreen(true);
    return;
  }
  // Keyboard Lock (Chromium, fullscreen only) lets Esc, Alt+Tab and the Windows key reach the host.
  // Hold Esc to leave fullscreen.
  try { await navigator.keyboard?.lock(); } catch { /* unsupported */ }
  // Phones: turn to the host display's shape (Android Chrome allows this only while fullscreen).
  if (isTouch()) {
    const d = host?.displays.find((x) => x.id === host.active);
    const portrait = d?.width && d?.height && d.height > d.width;
    screen.orientation?.lock?.(portrait ? 'portrait' : 'landscape').catch(() => {});
  }
}

function setFakeFullscreen(on) {
  document.body.classList.toggle('fake-fs', on);
  showFullscreenState();
}

function showFullscreenState() {
  const on = !!fullscreenElement() || document.body.classList.contains('fake-fs');
  ui.fullscreen.textContent = on ? 'Exit fullscreen' : 'Fullscreen';
  ui.touchFullscreen.textContent = on ? 'Exit' : 'Fullscreen';
}

for (const ev of ['fullscreenchange', 'webkitfullscreenchange']) {
  document.addEventListener(ev, () => {
    if (fullscreenElement()) {
      try { navigator.keyboard?.lock()?.catch(() => {}); } catch { /* unsupported */ }
    } else {
      navigator.keyboard?.unlock?.();
      try { screen.orientation?.unlock?.(); } catch { /* not locked */ }
    }
    showFullscreenState();
  });
}

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
    if (prevSnap) ui.stats.textContent = `${format(compute(prevSnap, snap), frameTimer.avgMs)}\n${formatHost(host?.stats)}`;
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
  cursor?.setMode(mode);
  sendCursorMode();
}

// The host sends pointer positions only for relative mode, where the browser draws the pointer.
function sendCursorMode() {
  session?.sendControl({ type: 'setCursorPositions', enabled: ui.mouseMode.value === 'relative' });
}

// In a window the browser keeps Esc to free the mouse and never passes it on; say how to send one.
// Only the page's fullscreen (not F11) can lock the keyboard and let Esc through.
let releasingMouse = false;
let escHintShown = false;
document.addEventListener('pointerlockchange', () => {
  if (document.pointerLockElement) return;
  const byUs = releasingMouse;
  releasingMouse = false;
  if (byUs || escHintShown || !input || ui.mouseMode.value !== 'relative' || fullscreenElement()) return;
  if (document.hidden || !document.hasFocus()) return;
  escHintShown = true;
  setStatus('Esc frees the mouse in a window. Use Fullscreen to send Esc, or Ctrl+Alt+Shift+E', ui.status.dataset.state);
});

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
    case 'KeyQ': releasingMouse = !!document.pointerLockElement; document.exitPointerLock(); return true;
    case 'KeyE': input?.tapKey(hidUsage('Escape')); return true;
  }
  return false;
}

ui.connect.addEventListener('click', () => (session && !session.closed ? disconnect() : connect()));
ui.display.addEventListener('change', () => selectDisplay(ui.display.value));
ui.mouseMode.addEventListener('change', () => setMouseMode(ui.mouseMode.value));
ui.fullscreen.addEventListener('click', toggleFullscreen);
ui.touchFullscreen.addEventListener('click', toggleFullscreen);
ui.statsToggle.addEventListener('click', toggleStats);
// Toolbar controls keep focus otherwise, and Space/Enter would re-trigger them.
for (const el of [ui.connect, ui.fullscreen, ui.statsToggle, ui.autotune]) el.addEventListener('mouseup', () => el.blur());

// Settings and Files dropdowns: a toggle opens one; a click or tap anywhere else, or Esc, closes it.
function setSettingsOpen(open) {
  if (open) setFilesOpen(false);
  document.body.classList.toggle('more-open', open);
  ui.moreToggle.setAttribute('aria-expanded', String(open));
}
function setFilesOpen(open) {
  if (open) setSettingsOpen(false);
  document.body.classList.toggle('files-open', open);
  ui.filesToggle.setAttribute('aria-expanded', String(open));
}
ui.moreToggle.addEventListener('click', () => setSettingsOpen(!document.body.classList.contains('more-open')));
ui.filesToggle.addEventListener('click', () => setFilesOpen(!document.body.classList.contains('files-open')));
document.addEventListener('pointerdown', (e) => {
  if (!ui.more.contains(e.target) && !ui.moreToggle.contains(e.target)) setSettingsOpen(false);
  if (!ui.files.contains(e.target) && !ui.filesToggle.contains(e.target)) setFilesOpen(false);
}, { capture: true });
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (document.body.classList.contains('more-open')) setSettingsOpen(false);
  if (document.body.classList.contains('files-open')) setFilesOpen(false);
});

// Files the host shares: the Files button shows how many; each row saves, cancels or says what happened.
const files = new Files({ sendControl: (msg) => session?.sendControl(msg) });
files.addEventListener('offer', (e) => {
  setStatus(`Host shared ${e.detail.name} (${formatSize(e.detail.size)}): see Files`, ui.status.dataset.state);
});
// Files this browser sends to the host, when it takes them: Send files… or a drop on the page.
const uploads = new Uploads({
  sendControl: (msg) => session?.sendControl(msg),
  sendFile: (buf) => session?.sendFile(buf) ?? false,
  fileBuffered: () => session?.fileBuffered() ?? null,
});
let filesQueued = false;
const queueFiles = () => {
  if (filesQueued) return;
  filesQueued = true;
  requestAnimationFrame(() => { filesQueued = false; showFiles(); });
};
files.addEventListener('change', queueFiles);
uploads.addEventListener('change', queueFiles);
// Rows are updated in place, so a click isn't lost to a row redrawn under the pointer.
const fileRows = new Map();
const uploadRows = new Map();
function showFiles() {
  const list = files.list();
  ui.filesToggle.hidden = list.length === 0 && !uploads.enabled;
  ui.filesToggle.textContent = list.length ? `Files (${list.length})` : 'Files';
  if (ui.filesToggle.hidden) setFilesOpen(false);
  ui.fromHost.hidden = list.length === 0;
  ui.toHost.hidden = !uploads.enabled;
  showRows(ui.fileList, fileRows, list, (f) => ({
    button: { offered: 'Save', receiving: 'Cancel', saved: 'Save again', failed: 'Save' }[f.state],
    progress: f.state === 'receiving' ? f.stored : null,
    failed: f.state === 'failed',
    info: fileInfo(f),
  }));
  showRows(ui.uploadList, uploadRows, uploads.list(), (u) => ({
    button: u.state === 'queued' || u.state === 'waiting' || u.state === 'sending' ? 'Cancel' : null,
    progress: u.state === 'sending' ? u.acked : null,
    failed: u.state === 'failed',
    info: uploadInfo(u),
  }));
}
function showRows(listEl, rows, items, view) {
  for (const [id, row] of rows) {
    if (!items.some((i) => i.id === id)) { row.li.remove(); rows.delete(id); }
  }
  for (const item of items) {
    let row = rows.get(item.id);
    if (!row) {
      const li = document.createElement('li');
      const name = Object.assign(document.createElement('span'), { className: 'name', textContent: item.name, title: item.name });
      const info = Object.assign(document.createElement('span'), { className: 'info' });
      const button = Object.assign(document.createElement('button'), { type: 'button' });
      button.dataset.id = String(item.id);
      const bar = document.createElement('progress');
      li.append(name, button, info, bar);
      listEl.append(li);
      row = { li, info, button, bar };
      rows.set(item.id, row);
    }
    const v = view(item);
    row.button.hidden = !v.button;
    row.button.textContent = v.button ?? '';
    row.bar.hidden = v.progress === null;
    row.bar.max = Math.max(item.size, 1);
    row.bar.value = v.progress ?? 0;
    row.info.classList.toggle('failed', v.failed);
    row.info.textContent = v.info;
  }
}
function rate(bytes, startedAt) {
  const secs = (performance.now() - startedAt) / 1000;
  return secs > 0.5 ? ` · ${formatSize(Math.round(bytes / secs))}/s` : '';
}
function fileInfo(f) {
  const size = formatSize(f.size);
  if (f.state === 'saved') return `Saved · ${size}`;
  if (f.state === 'failed') return f.error;
  if (f.state !== 'receiving') return size;
  return `${formatSize(f.stored)} of ${size}${rate(f.stored, f.startedAt)}`;
}
function uploadInfo(u) {
  const size = formatSize(u.size);
  if (u.state === 'saved') return `On the host as ${u.savedAs} · ${size}`;
  if (u.state === 'failed') return u.error;
  if (u.state === 'sending') return `${formatSize(u.acked)} of ${size}${rate(u.acked, u.startedAt)}`;
  return `${size} · waiting`;
}
ui.uploadPick.addEventListener('click', () => ui.uploadInput.click());
ui.uploadInput.addEventListener('change', () => {
  uploads.add(ui.uploadInput.files);
  ui.uploadInput.value = '';
});
ui.uploadList.addEventListener('click', (e) => {
  const button = e.target.closest('button[data-id]');
  if (button) uploads.cancel(Number(button.dataset.id));
});
// Dropping files anywhere on the page sends them, when the host takes them.
const dragsFiles = (e) => uploads.enabled && e.dataTransfer?.types.includes('Files');
document.addEventListener('dragover', (e) => {
  if (!dragsFiles(e)) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = 'copy';
  document.body.classList.add('drop-target');
});
document.addEventListener('dragleave', (e) => { if (!e.relatedTarget) document.body.classList.remove('drop-target'); });
document.addEventListener('drop', (e) => {
  document.body.classList.remove('drop-target');
  if (!dragsFiles(e)) return;
  e.preventDefault();
  uploads.add(e.dataTransfer.files);
  setFilesOpen(true);
});
ui.fileList.addEventListener('click', (e) => {
  const button = e.target.closest('button[data-id]');
  if (!button) return;
  const id = Number(button.dataset.id);
  if (files.files.get(id)?.state === 'receiving') files.cancel(id);
  else files.save(id);
});

// Touch screens: overlay buttons and on-screen keyboard.
const softKeys = new SoftKeyboard({
  textarea: ui.kbdInput,
  keys: ui.keys,
  send: (usage, down) => input?.rel(P.key(usage, down)),
});
const markTouch = () => document.body.classList.add('touch');
if (matchMedia('(pointer: coarse)').matches) markTouch();
document.addEventListener('pointerdown', (e) => { if (e.pointerType === 'touch') markTouch(); }, { capture: true });

// Overlay buttons don't take focus, so tapping them leaves the phone keyboard open.
for (const el of [$('touchbar'), ui.keys]) {
  for (const ev of ['pointerdown', 'mousedown']) el.addEventListener(ev, (e) => e.preventDefault());
}
// Opening the keyboard has to happen on click: phones show it only for a focus() inside a tap's
// user activation, which a touch pointerdown doesn't give. Whether it was open is read on pointerdown
// in case the tap moved focus anyway.
let kbdWasOpen = false;
ui.kbdToggle.addEventListener('pointerdown', () => { kbdWasOpen = softKeys.open; });
ui.kbdToggle.addEventListener('click', () => {
  if (kbdWasOpen || softKeys.open) ui.kbdInput.blur();
  else softKeys.show();
  kbdWasOpen = false;
});
ui.kbdInput.addEventListener('focus', () => ui.kbdToggle.setAttribute('aria-pressed', 'true'));
ui.kbdInput.addEventListener('blur', () => ui.kbdToggle.setAttribute('aria-pressed', 'false'));
ui.keysToggle.addEventListener('click', () => {
  ui.keys.hidden = !ui.keys.hidden;
  ui.keysToggle.setAttribute('aria-pressed', String(!ui.keys.hidden));
  if (ui.keys.hidden) softKeys.releaseSticky();
});

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
