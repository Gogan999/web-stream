import { Session } from './session.js';
import { defaultSignalUrl } from './signaling.js';
import { InputCapture } from './input.js';
import { hidUsage } from './keymap.js';
import { CursorView } from './cursor.js';
import { openPopout } from './popout.js';
import { snapshot, compute, assess, render, Alerts, FrameTimer } from './stats.js';
import * as pairing from './pairing.js';
import * as hosts from './hosts.js';
import * as settingsStore from './settings.js';
import { AutoTune, Cancelled } from './autotune.js';
import { SoftKeyboard } from './softkeys.js';
import { ClipboardSync } from './clipboard.js';
import * as P from './protocol.js';
import { Files, Uploads, formatSize } from './files.js';
import { applyCapabilities, parseCapabilities } from './capabilities.js';
import { DiagLog, diagEnabled } from './diag.js';
import { ZoomView } from './zoom.js';
import { StallWatchdog } from './watchdog.js';
import { ClientReporter } from './telemetry.js';
import { detectDecoders } from './decoders.js';

const $ = (id) => document.getElementById(id);
// First thing: the pairing link's secret leaves the address bar (and history) before anything else
// runs.
const pairingLink = pairing.takePairingLink();

const ui = {
  connect: $('connect'), connectBig: $('connect-big'), stageHost: $('stage-host'), status: $('status'), display: $('display'), displayQuick: $('display-quick'),
  mouseMode: $('mouse-mode'), mouseToggle: $('mouse-toggle'), mouseLabel: $('mouse-label'), micToggle: $('mic-toggle'), sas: $('sas'),
  priority: $('priority'), autotuneKeep: $('autotune-keep'), resetSettings: $('reset-settings'), version: $('version'),
  fullscreen: $('fullscreen'), statsToggle: $('stats-toggle'), statsLevel: $('stats-level'), stage: $('stage'), video: $('video'), stats: $('stats'),
  cursor: $('cursor'), video2: $('video2'), cursor2: $('cursor2'), display2: $('display2'), popout: $('popout'),
  unpair: $('unpair'), updateHost: $('update-host'), pair: $('pair'), deviceName: $('device-name'), pairStart: $('pair-start'),
  pairCode: $('pair-code'), pairConfirm: $('pair-confirm'), pairMsg: $('pair-msg'),
  resolution: $('resolution'), codec: $('codec'), bitrate: $('bitrate'), fps: $('fps'),
  keyframeScale: $('keyframe-scale'), recoverDrops: $('recover-drops'), minScale: $('min-scale'),
  matchHost: $('match-host'), autoconnect: $('autoconnect'), autotune: $('autotune'), mic: $('mic'),
  audioKbps: $('audio-kbps'), audioFrame: $('audio-frame'),
  moreToggle: $('more-toggle'), more: $('more'), kbdToggle: $('kbd-toggle'), keysToggle: $('keys-toggle'), keys: $('keys'),
  kbdInput: $('kbd-input'), touchFullscreen: $('touch-fullscreen'), view: $('view'), zoomReset: $('zoom-reset'), zoomPan: $('zoom-pan'),
  filesToggle: $('files-toggle'), files: $('files'), fileList: $('file-list'), fromHost: $('from-host'),
  toHost: $('to-host'), uploadPick: $('upload-pick'), uploadInput: $('upload-input'), uploadList: $('upload-list'),
  diagSave: $('diag-save'), clipboard: $('clipboard'), clipEnabled: $('clip-enabled'), touchPaste: $('touch-paste'),
  pcQuick: $('pc-quick'), pcList: $('pc-list'), pcAdd: $('pc-add'), pairOther: $('pair-other'),
  addPc: $('add-pc'), addAddress: $('add-address'), addDeviceName: $('add-device-name'), addStart: $('add-start'),
  addCode: $('add-code'), addConfirm: $('add-confirm'), addCancel: $('add-cancel'), addMsg: $('add-msg'),
  addTrust: $('add-trust'), addTrustHost: $('add-trust-host'), addTrustCa: $('add-trust-ca'), addTrustOpen: $('add-trust-open'),
};

const params = new URLSearchParams(location.search);
const signalUrl = defaultSignalUrl();
const diag = diagEnabled(params, globalThis.localStorage) ? new DiagLog() : null;
if (diag) {
  ui.diagSave.hidden = false;
  ui.diagSave.addEventListener('click', () => diag.save());
}

const zoom = new ZoomView({
  el: ui.view, stage: ui.stage,
  onChange: (on) => { ui.zoomReset.hidden = ui.zoomPan.hidden = !on; },
});
// Blocks Safari's whole-page pinch zoom.
for (const ev of ['gesturestart', 'gesturechange', 'gestureend']) {
  document.addEventListener(ev, (e) => e.preventDefault(), { passive: false });
}
ui.zoomReset.addEventListener('click', () => zoom.reset());
ui.zoomPan.addEventListener('click', () => {
  zoom.panMode = !zoom.panMode;
  ui.zoomPan.setAttribute('aria-pressed', String(zoom.panMode));
});

let session = null;
let soundBlocked = false;
let userClosed = true;
let wakeLock = null;
let input = null;
let savedPcs = hosts.loadHosts();
let localPaired = false;
let target = null;
let cursor = null;
let cursor2 = null;
let popout = null;
let statsTimer = 0;
const reporter = new ClientReporter({ getPc: () => session?.pc, send: (m) => session?.sendControl(m) });
let prevSnap = null;
const frameTimer = new FrameTimer(ui.video);
const alerts = new Alerts();

// Stream settings and what the host reported.
const settings = settingsStore.load();
let host = null;
let lastSent = null;
let tuning = null; // running AutoTune
let tuned = null;
let tuneWindow = null;
let micTrack = null;
let remoteUpdate = false;

function setStatus(text, state) {
  ui.status.textContent = text;
  ui.status.dataset.state = state;
}

// Connected (or connecting): the toolbar button disconnects, and the big one on the stage goes
// away.
function showConnected(on) {
  ui.connect.textContent = on ? 'Disconnect' : 'Connect';
  document.body.classList.toggle('connected', on);
}

// Hooks for the host's capabilities message (see capabilities.js).
function setSasVisible(visible) {
  ui.sas.hidden = !visible;
  $('keys-sas').hidden = !visible;
}
// Shown on the not-connected screen and in the title.
function setHostName(name) {
  ui.stageHost.textContent = name ?? '';
  ui.stageHost.hidden = !name;
  document.title = name ? `${name} - web-stream` : 'web-stream';
  if (name && target) rememberName(target, name);
}
// The "page vX, host vY" line in Settings > Advanced.
function setHostVersion(version) {
  ui.version.textContent = version ? `Host version ${version}` : '';
  ui.version.hidden = !version;
}

async function connect(next = target) {
  if (!next) {
    showAddPc();
    return;
  }
  disconnect();
  userClosed = false;
  target = next;
  hosts.setLastHost(target.id);
  renderPcs();
  const remote = isRemote(target);
  session = new Session({
    signalUrl: remote ? hosts.signalUrlFor(target.origin) : signalUrl,
    token: remote ? target.token : null,
    video: ui.video, video2: ui.video2,
  });
  session.addEventListener('status', (e) => setStatus(e.detail.text, e.detail.state));
  session.addEventListener('muted', () => { soundBlocked = true; });
  session.addEventListener('control', (e) => onControl(e.detail));
  session.addEventListener('file-chunk', (e) => files.onChunk(e.detail));
  session.addEventListener('control-open', () => {
    keepAwake();
    files.clear();
    uploads.clear();
    if (diag && session.pc) diag.start(session.pc, ui.video);
    session.sendControl({ type: 'listDisplays' });
    if (host) {
      host.decodersSent = null;
      reportDecoders();
    }
    reporter.start();
    sendCursorMode();
    session.sendControl({ type: 'setMicEnabled', enabled: settings.mic });
    session.sendControl(settingsStore.audioConfig(settings));
  });
  session.addEventListener('closed', () => {
    releaseWakeLock();
    reporter.stop();
    setRemoteUpdate(false);
    clip.reset();
    setSasVisible(false);
    showConnected(false);
    ui.display.disabled = true;
    syncQuickDisplay();
    tuning?.cancel();
    files.clear();
    uploads.clear();
  });
  clearTuned();
  host = {
    displays: [], active: null, second: null, config: null, autoCodec: false, stats: null, matched: false, awaitingMatch: false,
    decoderKey: null, decoderList: null, decodersSent: null,
  };
  lastSent = null;
  session.addEventListener('status', (e) => { if (/unpaired/.test(e.detail.text)) recheck(target); });
  input = new InputCapture({
    session, video: ui.video, video2: ui.video2, stage: ui.stage, onHotkey,
    keyTarget: ui.kbdInput, onKeySent: (usage, down) => softKeys.keySent(usage, down),
    onReleaseAll: () => softKeys.forget(),
    displaySize: () => host?.displays.find((d) => d.id === host.active) ?? null,
    displaySize2: () => displayById(host?.second),
    zoom, onBeforePaste: () => clip.beforePaste(),
  });
  input.setMode(ui.mouseMode.value);
  input.start();
  cursor = new CursorView({
    video: ui.video, overlay: ui.cursor, displaySize: () => host?.displays.find((d) => d.id === host.active) ?? null,
    onPlace: (x, y) => zoom.follow(x, y),
  });
  cursor.setMode(ui.mouseMode.value);
  makeCursor2(ui.video2, ui.cursor2);
  showConnected(true);
  if (!ui.stats.hidden) startStats();
  if (settings.mic) setMic(true);
  try {
    await session.start();
  } catch (err) {
    session.fail(err.message);
    recheck(target);
  }
}

function disconnect() {
  userClosed = true;
  reporter.stop();
  diag?.stop();
  if (input) input.stop();
  if (session) session.close();
  showDual(false);
  cursor?.stop();
  cursor2?.stop();
  zoom.reset();
  micTrack?.stop();
  micTrack = null;
  input = null;
  cursor = null;
  cursor2 = null;
  session = null;
  stopStats();
  tuning?.cancel();
  files.clear();
  uploads.clear();
  setRemoteUpdate(false);
  setSasVisible(false);
  host = null;
  clearTuned();
  ui.display.replaceChildren();
  syncQuickDisplay();
  showConnected(false);
  setStatus('Not connected', 'idle');
}

function onControl(msg) {
  // The pointer's shape goes to both videos; its position is told apart by where it falls.
  const pointer = [cursor?.onControl(msg), cursor2?.onControl(msg)];
  if (pointer.some(Boolean)) return;
  if (files.onControl(msg)) return;
  if (uploads.onControl(msg)) return;
  if (clip.onControl(msg)) return;
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
    syncQuickDisplay();
    fillSecond(msg.displays, msg.second ?? null);
    if (host) {
      host.displays = msg.displays;
      host.second = msg.second ?? null;
      showDual(host.second != null);
      if (msg.active != null) host.active = msg.active;
      host.awaitingMatch = false;
      applySettings();
    }
  } else if (msg.type === 'secondDisplay' && host) {
    host.second = msg.id ?? null;
    ui.display2.value = host.second == null ? 'off' : String(host.second);
    showDual(host.second != null);
    applySettings();
  } else if (msg.type === 'displayChanged' && msg.id != null) {
    ui.display.value = String(msg.id);
    syncQuickDisplay();
    if (host) {
      host.active = msg.id;
      if (msg.config) host.config = msg.config;
      host.autoCodec = msg.autoCodec === true;
      showCodec();
      applySettings();
    }
  } else if (msg.type === 'videoStats' && host) {
    host.stats = msg;
    diag?.add('host_stats', msg);
    if (tuneWindow) tuneWindow.push(msg);
  } else if (msg.type === 'bottleneck' && host) {
    host.bottleneck = msg;
    diag?.add('bottleneck', msg);
  } else if (msg.type === 'capabilities') {
    applyCapabilities(parseCapabilities(msg), { setSasVisible, setHostName, setHostVersion, setClipboardAvailable: (on) => clip.setAvailable(on) });
  } else if (msg.type === 'remoteUpdate') {
    setRemoteUpdate(msg.enabled === true);
  } else if (msg.type === 'updateStatus' && typeof msg.message === 'string') {
    setStatus(`Host update: ${msg.message}`, ui.status.dataset.state);
    setRemoteUpdate(true, msg.state === 'started');
  } else if (msg.type === 'micUnavailable') {
    setStatus(`Host: ${msg.message}`, ui.status.dataset.state);
    settings.mic = false;
    showMic();
    settingsStore.save(settings);
    setMic(false);
  } else if (msg.type === 'error' && typeof msg.message === 'string') {
    setStatus(`Host: ${msg.message}`, ui.status.dataset.state);
    if (remoteUpdate) setRemoteUpdate(true);
    if (host) {
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
  if (!session || !host) return;
  reportDecoders();
  if (!host.config || tuning) return;
  if (settings.matchHost && !host.matched) {
    host.matched = true;
    host.awaitingMatch = true;
    const { width, height } = settingsStore.screenPixels();
    session.sendControl({ type: 'setHostResolution', width, height });
  }
  if (host.awaitingMatch) return;
  sendConfig(currentConfig());
}

function currentConfig() {
  const display = host.displays.find((d) => d.id === host.active) ?? null;
  const config = settingsStore.videoConfig(settings, { screen: settingsStore.screenPixels(), display, codec: host.config.codec, rotate: isTouch() });
  return tuned ? { ...config, ...tuned } : config;
}

// The stream size and rate the settings ask for, to rate the browser's decoders at.
function decoderStream() {
  const display = host.displays.find((d) => d.id === host.active) ?? null;
  const c = settingsStore.videoConfig(settings, { screen: settingsStore.screenPixels(), display, rotate: isTouch() });
  return { width: c.width, height: c.height, fps: c.fps, bitrateKbps: c.bitrateKbps };
}

// Tells the host what this browser decodes, rated again whenever the stream size or rate changes.
function reportDecoders() {
  const stream = decoderStream();
  const key = `${stream.width}x${stream.height}@${stream.fps}`;
  if (key !== host.decoderKey) {
    const h = host;
    h.decoderKey = key;
    detectDecoders(stream).then((list) => {
      if (h !== host || h.decoderKey !== key) return;
      h.decoderList = list;
      sendDecoders();
    }, () => {});
  }
  sendDecoders();
}

// Sends the decoders and the Auto codec flag when they changed.
function sendDecoders() {
  if (!session || !host?.decoderList) return;
  const msg = { type: 'decoders', decoders: host.decoderList, auto: settings.codec == null };
  const json = JSON.stringify(msg);
  if (json === host.decodersSent) return;
  host.decodersSent = json;
  session.sendControl(msg);
}

function clearTuned() {
  tuned = null;
  ui.autotuneKeep.hidden = true;
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
fillSelect(ui.resolution, settingsStore.MENU_RESOLUTIONS, (v) => RES_LABELS[v]);
fillSelect(ui.bitrate, ['auto', ...settingsStore.BITRATES_KBPS], (v) => (v === 'auto' ? 'Auto' : `${v / 1000} Mb/s`));
fillSelect(ui.fps, settingsStore.FRAME_RATES, (v) => String(v));
const CODEC_LABELS = { h264: 'H.264', h265: 'H.265 (HEVC)', av1: 'AV1' };
const decodable = settingsStore.decodableCodecs();
fillSelect(ui.codec, ['auto', ...decodable], (v) => (v === 'auto' ? 'Auto' : CODEC_LABELS[v]));
if (settings.codec && !decodable.includes(settings.codec)) settings.codec = null;
fillSelect(ui.priority, settingsStore.PRIORITIES, (v) => v[0].toUpperCase() + v.slice(1));
fillSelect(ui.keyframeScale, settingsStore.KEYFRAME_SCALES, (v) => (v === 1 ? '1x (fastest)' : v === 4 ? '4x (sharpest)' : `${v}x`));
const MIN_SCALE_LABELS = { 0: 'On', 75: '75%', 50: '50%', 100: 'Off' };
fillSelect(ui.minScale, [0, 75, 50, 100], (v) => MIN_SCALE_LABELS[v]);
fillSelect(ui.audioKbps, settingsStore.AUDIO_KBPS, (v) => (v === 'host' ? 'Host setting' : `${v} kb/s`));
fillSelect(ui.statsLevel, settingsStore.STATS_LEVELS, (v) => v[0].toUpperCase() + v.slice(1));
fillSelect(ui.audioFrame, settingsStore.AUDIO_FRAMES, (v) => (v === 'host' ? 'Host setting' : v === '10' ? '10 ms (lowest delay)' : '20 ms (better sound)'));

function ensureOption(select, value, label) {
  if ([...select.options].some((o) => o.value === String(value))) return;
  const o = document.createElement('option');
  o.value = String(value);
  o.textContent = label();
  select.append(o);
}

// Shows the settings.
function showSettings() {
  ensureOption(ui.bitrate, settings.bitrate, () => `${Number(settings.bitrate) / 1000} Mb/s`);
  ensureOption(ui.resolution, settings.resolution, () => RES_LABELS[settings.resolution]);
  ui.resolution.value = settings.resolution;
  showCodec();
  ui.mouseMode.value = settings.mouseMode;
  ui.bitrate.value = String(settings.bitrate);
  ui.fps.value = String(settings.fps);
  ui.priority.value = settings.priority;
  ui.keyframeScale.value = String(settings.keyframeScale);
  ui.minScale.value = String(settings.minScale);
  ui.recoverDrops.checked = !!settings.recoverDrops;
  ui.matchHost.checked = !!settings.matchHost;
  ui.autoconnect.checked = settings.autoconnect !== false;
  showMic();
  showMouseLabel();
  ui.audioKbps.value = settings.audioKbps;
  ui.audioFrame.value = settings.audioFrame;
  ui.statsLevel.value = settings.statsLevel;
}
// The chosen codec, or Auto with what the host streams and whether it picked it for this browser.
function showCodec() {
  ui.codec.value = settings.codec ?? 'auto';
  const now = settings.codec == null ? host?.config?.codec : null;
  const picked = host?.autoCodec ? ', picked for this browser' : '';
  ui.codec.options[0].textContent = now ? `Auto (${CODEC_LABELS[now] ?? now}${picked})` : 'Auto';
}

// The Mic checkbox in Settings and the toolbar's Mic toggle show one setting.
function showMic() {
  ui.mic.checked = !!settings.mic;
  ui.micToggle.setAttribute('aria-pressed', String(!!settings.mic));
}
// Mouse button: Game / Desktop, or Trackpad / Direct on a touch screen.
function showMouseLabel() {
  const touch = document.body.classList.contains('touch');
  const relative = settings.mouseMode === 'relative';
  ui.mouseLabel.textContent = touch ? (relative ? 'Trackpad' : 'Direct') : (relative ? 'Game' : 'Desktop');
  ui.mouseToggle.title = touch
    ? 'Trackpad: drag moves the pointer. Direct: the pointer goes where you touch.'
    : 'Game: the mouse is captured and moves relatively. Desktop: the pointer follows yours. Ctrl+Alt+Shift+M';
}
// The toolbar's display switch lists what Settings > Display lists, and shows only for 2+ displays.
function syncQuickDisplay() {
  ui.displayQuick.replaceChildren(...[...ui.display.options].map((o) => o.cloneNode(true)));
  ui.displayQuick.value = ui.display.value;
  ui.displayQuick.disabled = ui.display.disabled;
  ui.displayQuick.hidden = ui.display.options.length < 2;
}
showSettings();

for (const [el, key] of [[ui.resolution, 'resolution'], [ui.codec, 'codec'], [ui.bitrate, 'bitrate'], [ui.fps, 'fps'], [ui.priority, 'priority'], [ui.keyframeScale, 'keyframeScale'], [ui.minScale, 'minScale']]) {
  el.addEventListener('change', () => {
    settings[key] = ['fps', 'keyframeScale', 'minScale'].includes(key) ? Number(el.value) : el.value;
    if (key === 'codec' && el.value === 'auto') settings.codec = null;
    if (key === 'priority' && settings.priority === 'source') Object.assign(settings, settingsStore.SOURCE_PRESET);
    clearTuned();
    settingsStore.save(settings);
    showSettings();
    applySettings();
  });
}

ui.recoverDrops.addEventListener('change', () => {
  settings.recoverDrops = ui.recoverDrops.checked;
  settingsStore.save(settings);
  applySettings();
});

ui.autoconnect.addEventListener('change', () => {
  settings.autoconnect = ui.autoconnect.checked;
  settingsStore.save(settings);
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

// Starts or stops sending the mic.
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
      showMic();
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

function changeMic(on) {
  settings.mic = on;
  showMic();
  settingsStore.save(settings);
  setMic(settings.mic);
}
ui.mic.addEventListener('change', () => changeMic(ui.mic.checked));
ui.micToggle.addEventListener('click', () => changeMic(!settings.mic));

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
  for (const el of [ui.resolution, ui.codec, ui.bitrate, ui.fps, ui.priority, ui.keyframeScale, ui.minScale, ui.recoverDrops, ui.display, ui.displayQuick, ui.matchHost]) el.disabled = true;
  try {
    const { config, summary } = await tuning.run();
    tuned = { bitrateKbps: config.bitrateKbps, quality: config.quality };
    ui.autotuneKeep.hidden = false;
    setStatus(`${summary} (this connection only; Settings > Advanced > Keep these)`, 'connected');
  } catch (err) {
    if (err instanceof Cancelled) setStatus('Auto-tune stopped', session ? 'connected' : 'idle');
    else setStatus(`Auto-tune failed: ${err.message}`, session ? 'connected' : 'idle');
  } finally {
    tuning = null;
    tuneWindow = null;
    ui.autotune.textContent = 'Auto-tune (test)';
    for (const el of [ui.resolution, ui.codec, ui.bitrate, ui.fps, ui.priority, ui.keyframeScale, ui.minScale, ui.recoverDrops, ui.matchHost]) el.disabled = false;
    ui.display.disabled = ui.display.options.length < 2;
    syncQuickDisplay();
    showSettings();
    if (session) applySettings();
  }
}

ui.autotune.addEventListener('click', autoTune);
ui.autotuneKeep.addEventListener('click', () => {
  if (!tuned) return;
  settings.bitrate = String(tuned.bitrateKbps);
  settings.priority = settingsStore.QUALITY_PRIORITY[tuned.quality] ?? settings.priority;
  settingsStore.save(settings);
  clearTuned();
  showSettings();
  setStatus('Saved auto-tune result as Max bitrate and Priority', ui.status.dataset.state);
});

ui.resetSettings.addEventListener('click', () => {
  if (!confirm('Put all settings on this page back to their defaults?')) return;
  const matched = settings.matchHost;
  const mic = settings.mic;
  Object.assign(settings, settingsStore.DEFAULTS);
  settingsStore.save(settings);
  clearTuned();
  showSettings();
  setMouseMode(settings.mouseMode);
  if (mic) setMic(false);
  if (matched) ui.matchHost.dispatchEvent(new Event('change'));
  session?.sendControl(settingsStore.audioConfig(settings));
  if (session) applySettings();
  setStatus('Settings reset to defaults', ui.status.dataset.state);
});

// The host pointer's view on the second monitor's video: in the page, or in its window.
function makeCursor2(video, overlay) {
  cursor2?.stop();
  cursor2 = new CursorView({
    video, overlay, displaySize: () => displayById(host?.second),
    shift: () => {
      const main = displayById(host?.active);
      const other = displayById(host?.second);
      return main && other ? { x: main.x - other.x, y: main.y - other.y } : { x: 0, y: 0 };
    },
  });
  cursor2.setMode(ui.mouseMode.value);
}

const displayById = (id) => (id == null ? null : host?.displays.find((d) => d.id === id) ?? null);

// The second monitor's choices: off, or any display.
function fillSecond(displays, second) {
  ui.display2.replaceChildren(...[{ id: 'off', label: 'Off' }, ...displays.map((d, i) => ({ id: String(d.id), label: `${i + 1}: ${d.name || d.id}` }))]
    .map(({ id, label }) => {
      const o = document.createElement('option');
      o.value = id;
      o.textContent = label;
      return o;
    }));
  ui.display2.value = second == null ? 'off' : String(second);
  ui.display2.disabled = displays.length < 2;
}

// Two monitors side by side, or one.
function showDual(on) {
  if (!on) popIn();
  document.body.classList.toggle('dual', on && !popout);
  ui.video2.hidden = !on || !!popout;
  ui.popout.hidden = !on;
  cursor?.schedule();
  cursor2?.schedule();
}

// Moves the second monitor's video to a window of its own, or back.
function popOut() {
  if (popout || !session || !input) return;
  const p = openPopout({ input, onClose: () => { if (popout === p) popIn(); } });
  if (!p) {
    setStatus('The browser blocked the pop-up window: allow pop-ups for this page', ui.status.dataset.state);
    return;
  }
  popout = p;
  p.video.srcObject = ui.video2.srcObject;
  ui.video2.srcObject = null;
  session.video2 = input.video2 = p.video;
  p.video.play().catch(() => {});
  makeCursor2(p.video, p.overlay);
  ui.popout.textContent = 'Pop in';
  showDual(true);
  p.window.focus();
}

function popIn() {
  const p = popout;
  if (!p) return;
  popout = null;
  ui.video2.srcObject = p.video.srcObject;
  p.video.srcObject = null;
  if (session) session.video2 = ui.video2;
  if (input) input.video2 = ui.video2;
  ui.video2.play().catch(() => {});
  if (cursor2) makeCursor2(ui.video2, ui.cursor2);
  ui.popout.textContent = 'Pop out';
  p.close();
  showDual(host?.second != null);
}

ui.popout.addEventListener('click', () => (popout ? popIn() : popOut()));

ui.display2.addEventListener('change', () => {
  if (!session) return;
  const id = ui.display2.value === 'off' ? null : Number(ui.display2.value);
  session.sendControl({ type: 'selectSecondDisplay', id });
});

function selectDisplay(id) {
  if (!session) return;
  ui.display.value = id;
  ui.displayQuick.value = id;
  zoom.reset();
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
    if (!fullscreenElement()) await new Promise((r) => setTimeout(r, 300));
    if (!fullscreenElement()) throw new Error('not fullscreen');
  } catch {
    if (isTouch()) setFakeFullscreen(true);
    return;
  }
  try { await navigator.keyboard?.lock(); } catch { /* unsupported */ }
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
      try { screen.orientation?.unlock?.(); } catch { }
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

// A stream that arrives but never decodes is rescued by asking the host for a keyframe, and the
// host restarts its video if that repeats.
const stallWatchdog = new StallWatchdog();
setInterval(async () => {
  const pc = session?.pc;
  if (!pc) return stallWatchdog.reset();
  const receiver = pc.getTransceivers().find((t) => t.receiver.track?.kind === 'video')?.receiver;
  if (!receiver) return stallWatchdog.reset();
  try {
    const video = [...(await receiver.getStats()).values()].find((s) => s.type === 'inbound-rtp');
    if (stallWatchdog.update(video, performance.now(), document.hidden)) session?.sendControl({ type: 'requestKeyframe' });
  } catch {
    // a closing connection
  }
}, 1000);

function startStats() {
  stopStats();
  frameTimer.start();
  ui.stats.textContent = 'waiting for stream…';
  statsTimer = setInterval(async () => {
    const pc = session?.pc;
    if (!pc) return;
    const snap = snapshot(await pc.getStats(), performance.now());
    if (prevSnap) showStats(compute(prevSnap, snap), snap.t);
    prevSnap = snap;
  }, 500);
}

function showStats(s, now) {
  const active = alerts.update(assess(s, host?.stats, host?.config, host?.bottleneck), now);
  ui.stats.replaceChildren(...render(settings.statsLevel, s, frameTimer.avgMs, host?.stats, host?.config, active).map(({ text, sev }) => {
    const line = document.createElement('span');
    if (sev) line.className = sev;
    line.textContent = `${text}\n`;
    return line;
  }));
}

function stopStats() {
  clearInterval(statsTimer);
  frameTimer.stop();
  alerts.clear();
  prevSnap = null;
}

function setStatsLevel(level) {
  settings.statsLevel = level;
  ui.statsLevel.value = level;
  settingsStore.save(settings);
}

function setMouseMode(mode) {
  ui.mouseMode.value = mode;
  settings.mouseMode = mode;
  showMouseLabel();
  settingsStore.save(settings);
  input?.setMode(mode);
  cursor?.setMode(mode);
  cursor2?.setMode(mode);
  sendCursorMode();
}

// The host sends pointer positions only for relative mode, where the browser draws the pointer.
function sendCursorMode() {
  session?.sendControl({ type: 'setCursorPositions', enabled: ui.mouseMode.value === 'relative' });
}

// Tells how to send Esc while windowed.
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

// Ctrl+Alt+Shift+<key>.
function onHotkey(code) {
  const digit = /^Digit([1-9])$/.exec(code);
  if (digit) {
    const opt = ui.display.options[Number(digit[1]) - 1];
    if (opt) selectDisplay(opt.value);
    return true;
  }
  switch (code) {
    case 'KeyS': toggleStats(); return true;
    case 'KeyL': {
      const levels = settingsStore.STATS_LEVELS;
      setStatsLevel(levels[(levels.indexOf(settings.statsLevel) + 1) % levels.length]);
      if (ui.stats.hidden) toggleStats();
      return true;
    }
    case 'KeyF': toggleFullscreen(); return true;
    case 'KeyM': setMouseMode(ui.mouseMode.value === 'relative' ? 'absolute' : 'relative'); return true;
    case 'KeyQ': releasingMouse = !!document.pointerLockElement; document.exitPointerLock(); return true;
    case 'KeyE': input?.tapKey(hidUsage('Escape')); return true;
  }
  return false;
}

ui.connect.addEventListener('click', () => (session && !session.closed ? disconnect() : connect()));
ui.connectBig.addEventListener('click', () => connect());
ui.display.addEventListener('change', () => selectDisplay(ui.display.value));
ui.displayQuick.addEventListener('change', () => selectDisplay(ui.displayQuick.value));
ui.mouseMode.addEventListener('change', () => setMouseMode(ui.mouseMode.value));
ui.mouseToggle.addEventListener('click', () => setMouseMode(ui.mouseMode.value === 'relative' ? 'absolute' : 'relative'));
ui.fullscreen.addEventListener('click', toggleFullscreen);
for (const el of [$('sas'), $('keys-sas')]) {
  el.addEventListener('click', () => session?.sendControl({ type: 'secureAttention' }));
}
ui.touchFullscreen.addEventListener('click', toggleFullscreen);
ui.statsToggle.addEventListener('click', toggleStats);
ui.statsLevel.addEventListener('change', () => setStatsLevel(ui.statsLevel.value));
for (const el of [ui.connect, ui.connectBig, ui.fullscreen, ui.sas, ui.mouseToggle, ui.micToggle, ui.statsToggle, ui.autotune, ui.clipboard]) el.addEventListener('mouseup', () => el.blur());

// Update host: only when the host allows it, and only after a confirm.
function setRemoteUpdate(enabled, running = false) {
  remoteUpdate = enabled;
  ui.updateHost.disabled = !enabled || running;
  ui.updateHost.textContent = running ? 'Updating host…' : 'Update host';
}
ui.updateHost.addEventListener('click', () => {
  if (!session || !remoteUpdate) return;
  const ok = confirm('Update the host to the latest main and restart it?\n\n'
    + 'It builds while the stream keeps running (several minutes), then restarts: '
    + 'the stream drops for a few seconds and reconnects to the new build. '
    + 'If the build fails, the host keeps running the old one.');
  if (!ok || !session) return;
  ui.updateHost.disabled = true;
  session.sendControl({ type: 'updateHost' });
});

// Settings and Files dropdowns: a toggle opens one; a click or tap anywhere else, or Esc, closes
// it.
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
// Whether Advanced is unfolded is remembered per browser.
const FOLDS_KEY = 'ws-settings-open';
try {
  const open = JSON.parse(localStorage.getItem(FOLDS_KEY) ?? 'null');
  if (open && typeof open === 'object') {
    for (const d of ui.more.querySelectorAll('details')) if (typeof open[d.id] === 'boolean') d.open = open[d.id];
  }
} catch { /* defaults from the page */ }
ui.more.addEventListener('toggle', () => {
  const open = Object.fromEntries([...ui.more.querySelectorAll('details')].map((d) => [d.id, d.open]));
  try { localStorage.setItem(FOLDS_KEY, JSON.stringify(open)); } catch { }
}, { capture: true });
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

// Files the host shares: the Files button shows how many; each row saves, cancels or says what
// happened.
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

// Clipboard text, both ways (clipboard.js).
const clip = new ClipboardSync({
  sendControl: (msg) => session?.sendControl(msg),
  notify: (text) => setStatus(text, ui.status.dataset.state),
  onChange: () => {
    ui.clipboard.hidden = !clip.usable;
    ui.touchPaste.hidden = !clip.usable;
    ui.clipboard.dataset.pending = String(clip.pending != null);
    ui.clipboard.textContent = clip.pending != null ? 'Copy from PC' : 'Clipboard';
    ui.clipEnabled.checked = clip.enabled;
    ui.clipEnabled.disabled = !clip.available;
  },
});
ui.clipEnabled.checked = clip.enabled;
ui.clipEnabled.disabled = true;
ui.clipEnabled.addEventListener('change', () => clip.setEnabled(ui.clipEnabled.checked));
ui.clipboard.addEventListener('click', () => clip.button());
window.addEventListener('focus', () => clip.onFocus());
ui.stage.addEventListener('focus', () => clip.onFocus());
ui.stage.addEventListener('pointerdown', () => clip.onGesture());
// Type text on the host as Unicode, whatever its keyboard layout.
function typeText(text) {
  for (const buf of P.text(text)) input?.rel(buf);
}
// Paste on a touch screen: the host gets this device's clipboard, then Ctrl+V.
async function touchPaste() {
  if (await clip.toHost()) {
    const ctrl = hidUsage('ControlLeft');
    const v = hidUsage('KeyV');
    for (const [usage, down] of [[ctrl, true], [v, true], [v, false], [ctrl, false]]) input?.rel(P.key(usage, down));
    return;
  }
  const text = await clip.readLocal();
  if (text) typeText(text);
  else setStatus('Allow clipboard access to paste', ui.status.dataset.state);
}
ui.touchPaste.addEventListener('click', touchPaste);

let filesQueued = false;
const queueFiles = () => {
  if (filesQueued) return;
  filesQueued = true;
  requestAnimationFrame(() => { filesQueued = false; showFiles(); });
};
files.addEventListener('change', queueFiles);
uploads.addEventListener('change', queueFiles);
// Rows are updated in place.
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
  sendText: typeText,
  onPaste: (text) => { clip.pushText(text); typeText(text); },
});
const markTouch = () => { document.body.classList.add('touch'); showMouseLabel(); };
if (matchMedia('(pointer: coarse)').matches) markTouch();
document.addEventListener('pointerdown', (e) => { if (e.pointerType === 'touch') markTouch(); }, { capture: true });

for (const el of [$('touchbar'), ui.keys]) {
  for (const ev of ['pointerdown', 'mousedown']) el.addEventListener(ev, (e) => e.preventDefault());
}
// Opens the keyboard on click, inside the tap's user activation.
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

// Mobile: sound after a tap, screen kept on, recovery after the app was in the background.
function unmute() {
  if (!soundBlocked) return;
  soundBlocked = false;
  for (const v of [ui.video, ui.video2]) {
    v.muted = false;
    if (v.srcObject && v.paused) v.play().catch(() => {});
  }
}
for (const ev of ['pointerup', 'touchend', 'keydown']) document.addEventListener(ev, unmute, { capture: true });

async function keepAwake() {
  if (document.hidden || wakeLock || !navigator.wakeLock) return;
  try {
    wakeLock = await navigator.wakeLock.request('screen');
    wakeLock.addEventListener('release', () => { wakeLock = null; });
  } catch { /* refused or unsupported */ }
}

function releaseWakeLock() {
  wakeLock?.release().catch(() => {});
  wakeLock = null;
}

function resumed() {
  if (document.hidden) return;
  if (session && !session.closed) {
    for (const v of [ui.video, ui.video2]) if (v.srcObject && v.paused) v.play().catch(() => {});
    keepAwake();
  } else if (!userClosed && target && ui.addPc.hidden && ui.pair.hidden) {
    connect();
  }
}
document.addEventListener('visibilitychange', resumed);
window.addEventListener('pageshow', (e) => { if (e.persisted) resumed(); });

// iOS keyboards cover the page instead of resizing it: fit the page to what is visible.
const vv = window.visualViewport;
if (vv) {
  const fit = () => {
    const covered = Math.abs(vv.scale - 1) < 0.01 && window.innerHeight - vv.height > 80;
    document.documentElement.style.setProperty('--app-height', covered ? `${Math.round(vv.height)}px` : '');
    if (covered && (window.scrollY || vv.offsetTop)) window.scrollTo(0, 0);
  };
  vv.addEventListener('resize', fit);
  vv.addEventListener('scroll', fit);
}

// Saved PCs and the page's own host.

const isRemote = (t) => !!t && t.id !== hosts.LOCAL;
const remoteOf = (t) => (isRemote(t) ? { origin: t.origin, token: t.token } : undefined);
const localTarget = () => ({ id: hosts.LOCAL, origin: location.origin, token: null, name: hosts.localName() || hosts.hostLabel(location.origin) });

function pickTarget(id) {
  if (id === hosts.LOCAL) return localPaired ? localTarget() : null;
  return savedPcs.find((h) => h.id === id && h.token) ?? null;
}

function targets() {
  return [...(localPaired ? [localTarget()] : []), ...savedPcs.filter((h) => h.token)];
}

function defaultTarget() {
  return pickTarget(hosts.lastHost()) ?? targets()[0] ?? null;
}

function storePcs(list) {
  savedPcs = list;
  hosts.saveHosts(savedPcs);
}

function rememberName(t, name) {
  if (t.id === hosts.LOCAL) hosts.setLocalName(name);
  else storePcs(hosts.renameHost(savedPcs, t.id, name));
  t.name = name;
  renderPcs();
}

function renderPcs() {
  const list = targets();
  if (target && !list.some((t) => t.id === target.id)) target = null;
  ui.pcList.replaceChildren(...list.map((t) => {
    const li = document.createElement('li');
    if (target?.id === t.id) li.setAttribute('aria-current', 'true');
    const go = document.createElement('button');
    go.type = 'button';
    go.className = 'pc-connect';
    const name = document.createElement('b');
    name.textContent = t.name;
    const where = document.createElement('small');
    where.textContent = hosts.hostLabel(t.origin);
    go.append(name, where);
    go.addEventListener('click', () => connect(t));
    li.append(go);
    if (isRemote(t)) {
      const del = document.createElement('button');
      del.type = 'button';
      del.className = 'pc-remove';
      del.textContent = '×';
      del.title = `Forget ${t.name}`;
      del.setAttribute('aria-label', `Forget ${t.name}`);
      del.addEventListener('click', () => forgetPc(t));
      li.append(del);
    }
    return li;
  }));
  if (!localPaired && savedPcs.length) {
    const li = document.createElement('li');
    const pair = document.createElement('button');
    pair.type = 'button';
    pair.className = 'pc-connect';
    pair.textContent = `Pair ${hosts.localName() || hosts.hostLabel(location.origin)}`;
    pair.addEventListener('click', () => setPairForm(true));
    li.append(pair);
    ui.pcList.append(li);
  }
  ui.pcQuick.replaceChildren(...list.map((t) => {
    const o = document.createElement('option');
    o.value = t.id;
    o.textContent = t.name;
    return o;
  }));
  ui.pcQuick.hidden = list.length < 2;
  if (target) ui.pcQuick.value = target.id;
  ui.connectBig.textContent = target && list.length > 1 ? `Connect to ${target.name}` : 'Connect';
  ui.unpair.hidden = !target;
}

async function forgetPc(t) {
  if (!confirm(`Forget ${t.name}? You will need a new code from it to connect again.`)) return;
  if (target?.id === t.id) disconnect();
  pairing.unpair(remoteOf(t)).catch(() => {});
  storePcs(hosts.removeHost(savedPcs, t.id));
  if (target?.id === t.id) target = defaultTarget();
  renderPcs();
}

// After a failed connection: tells an unreachable PC from one that no longer knows this device.
async function recheck(t) {
  if (!isRemote(t)) return checkPairing();
  try {
    const s = await pairing.getSession(remoteOf(t));
    if (s.paired) return;
    storePcs(hosts.forgetToken(savedPcs, t.id));
    renderPcs();
    showAddPc(t.origin, `${t.name} no longer knows this device. Pair it again.`);
  } catch {
    setStatus(`Can't reach ${t.name}`, 'failed');
    showTrust(t.origin);
  }
}

ui.pcQuick.addEventListener('change', () => {
  const t = pickTarget(ui.pcQuick.value);
  if (t && t.id !== target?.id) connect(t);
});

// Pairing this page's own host: the host sets an HttpOnly cookie once the code is confirmed.
let pairingId = null;

function setPairForm(on) {
  if (on) hideAddPc();
  document.body.classList.toggle('unpaired', on);
  ui.pair.hidden = !on;
  if (on) {
    setStatus('Not paired', 'idle');
    if (!ui.deviceName.value) ui.deviceName.value = pairing.defaultDeviceName();
  }
}

function showPairing(paired) {
  localPaired = paired;
  if (!paired && target?.id === hosts.LOCAL) {
    disconnect();
    target = null;
  }
  setPairForm(!paired && savedPcs.length === 0);
  if (!target) target = defaultTarget();
  renderPcs();
}

async function checkPairing() {
  try {
    const s = await pairing.getSession();
    showPairing(s.paired);
    return s.paired;
  } catch {
    setStatus('Host unreachable', 'failed');
    if (!target) target = defaultTarget();
    renderPcs();
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
    target = localTarget();
    showPairing(true);
    connect(localTarget());
  } catch (err) {
    ui.pairMsg.textContent = err.message;
  }
});

ui.unpair.addEventListener('click', async () => {
  if (isRemote(target)) {
    forgetPc(target);
    return;
  }
  if (!confirm('Unpair this browser? You will need a new code from the host to connect again.')) return;
  disconnect();
  await pairing.unpair().catch(() => {});
  showPairing(false);
});

ui.pairOther.addEventListener('click', () => showAddPc());

async function pairFromLink(link) {
  try {
    await pairing.pairWithLink(link, ui.deviceName.value.trim() || pairing.defaultDeviceName());
    target = localTarget();
    showPairing(true);
    return true;
  } catch (err) {
    if (!(err instanceof TypeError)) ui.pairMsg.textContent = err.message;
    return false;
  }
}

// Adding another PC: pairs with it from this page and keeps its device token.
let adding = null;

function showAddPc(address = '', message = '') {
  setPairForm(false);
  document.body.classList.add('adding');
  ui.addPc.hidden = false;
  ui.addAddress.value = address ? hosts.hostLabel(address) : ui.addAddress.value;
  if (!ui.addDeviceName.value) ui.addDeviceName.value = pairing.defaultDeviceName();
  ui.addMsg.textContent = message;
  ui.addTrust.hidden = true;
  resetAddCode();
  ui.addAddress.focus();
}

function hideAddPc() {
  document.body.classList.remove('adding');
  ui.addPc.hidden = true;
  adding = null;
}

function resetAddCode() {
  adding = null;
  ui.addCode.value = '';
  ui.addCode.disabled = true;
  ui.addConfirm.disabled = true;
  syncAddStart();
}

function syncAddStart() {
  ui.addStart.textContent = hosts.parseTarget(ui.addAddress.value)?.link ? 'Pair with link' : 'Request code';
}

function showTrust(origin) {
  ui.addTrustHost.textContent = hosts.hostLabel(origin);
  ui.addTrustCa.href = `${origin}/ca.crt`;
  ui.addTrustOpen.href = `${origin}/`;
  if (!ui.addPc.hidden) ui.addTrust.hidden = false;
  else setStatus(`Can't reach ${hosts.hostLabel(origin)}: is it on, and does this device trust its certificate?`, 'failed');
}

function addFailed(err, origin) {
  if (err instanceof TypeError) {
    ui.addMsg.textContent = '';
    showTrust(origin);
  } else {
    ui.addMsg.textContent = err.message;
  }
}

async function finishAdd(origin, token) {
  const [list, entry] = hosts.upsertHost(savedPcs, { origin, token });
  storePcs(list);
  hideAddPc();
  if (!localPaired && !savedPcs.some((h) => h.id !== entry.id)) setPairForm(false);
  await connect(entry);
}

ui.addAddress.addEventListener('input', () => {
  ui.addTrust.hidden = true;
  resetAddCode();
});

ui.addStart.addEventListener('click', async () => {
  ui.addMsg.textContent = '';
  ui.addTrust.hidden = true;
  const t = hosts.parseTarget(ui.addAddress.value);
  if (!t) {
    ui.addMsg.textContent = 'That is not an address. Type it as shown in the PC\'s web-stream window, like 192.168.1.20:8443.';
    return;
  }
  const name = ui.addDeviceName.value.trim() || pairing.defaultDeviceName();
  if (t.origin === location.origin) {
    hideAddPc();
    if (t.link) await pairFromLink(t.link).then((ok) => ok && connect(localTarget()));
    else setPairForm(true);
    return;
  }
  try {
    if (t.link) {
      const token = await pairing.pairWithLink(t.link, name, { origin: t.origin });
      await finishAdd(t.origin, token);
      return;
    }
    const r = await pairing.startPairing(name, { origin: t.origin });
    adding = { origin: t.origin, pairingId: r.pairingId };
    ui.addCode.disabled = false;
    ui.addConfirm.disabled = false;
    ui.addCode.focus();
    ui.addMsg.textContent = `Enter the code shown on ${hosts.hostLabel(t.origin)} (valid ${r.expiresIn ?? 120} s).`;
  } catch (err) {
    addFailed(err, t.origin);
  }
});

ui.addPc.addEventListener('submit', async (e) => {
  e.preventDefault();
  if (!adding) {
    ui.addStart.click();
    return;
  }
  const { origin, pairingId: id } = adding;
  try {
    const token = await pairing.confirmPairing(id, ui.addCode.value.trim(), { origin });
    await finishAdd(origin, token);
  } catch (err) {
    addFailed(err, origin);
  }
});

ui.addCancel.addEventListener('click', () => {
  hideAddPc();
  if (!localPaired && !savedPcs.length) checkPairing();
});

ui.pcAdd.addEventListener('click', () => showAddPc());

if ('serviceWorker' in navigator && window.isSecureContext) {
  navigator.serviceWorker.register('sw.js').catch(() => {});
}
navigator.storage?.persist?.().catch(() => {});

target = pickTarget(hosts.lastHost());
checkPairing().then(async (paired) => {
  if (!paired && pairingLink === '') {
    ui.pairMsg.textContent = 'That pairing link was damaged. Scan the QR code on the host again.';
  }
  if (!paired && pairingLink) paired = await pairFromLink(pairingLink);
  if (!target) target = defaultTarget();
  renderPcs();
  if (pairingLink && paired) connect(localTarget());
  else if (target && settings.autoconnect) connect();
});
