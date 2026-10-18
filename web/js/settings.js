// Stream settings chosen in the browser: bitrate, resolution, frame rate and encoder quality,
// turned into the host's VideoConfig. Pure functions so they can be tested; main.js owns the UI.

export const QUALITIES = Object.freeze(['speed', 'balanced', 'clarity']);
export const BITRATES_KBPS = Object.freeze([5000, 10000, 15000, 20000, 30000, 50000, 80000, 120000, 150000]);
export const FRAME_RATES = Object.freeze([30, 60, 90, 120, 144]);
// 'screen' fits this screen, 'native' is the host display's own size; the rest are fixed heights in 16:9.
export const RESOLUTIONS = Object.freeze(['screen', 'native', '2160', '1440', '1080', '720']);

export const DEFAULTS = Object.freeze({
  bitrate: 'auto', resolution: 'screen', fps: 60, quality: 'balanced', matchHost: false,
});

const STORAGE_KEY = 'ws-stream-settings';
const even = (v) => Math.max(2, Math.floor(v / 2) * 2);

// Bitrate for "Auto": 20 Mb/s at 1080p60, growing a little slower than the pixel rate.
export function autoKbps(width, height, fps) {
  const ratio = (width * height * fps) / (1920 * 1080 * 60);
  const kbps = 20000 * Math.pow(ratio, 0.9);
  return Math.round(Math.min(150000, Math.max(5000, kbps)) / 500) * 500;
}

// Largest even size with the display's shape that fits in the box, never above the display's own size
// (upscaling on the host only costs bits).
export function fitToDisplay(boxW, boxH, dispW, dispH) {
  if (!(dispW > 0 && dispH > 0)) return { width: even(boxW), height: even(boxH) };
  const scale = Math.min(boxW / dispW, boxH / dispH, 1);
  return { width: even(dispW * scale), height: even(dispH * scale) };
}

// The stream size for a resolution setting. `screen` is this screen's size in device pixels.
export function streamSize(resolution, screen, display) {
  const dw = display?.width ?? 1920;
  const dh = display?.height ?? 1080;
  if (resolution === 'native') return { width: even(dw), height: even(dh) };
  if (resolution === 'screen') return fitToDisplay(screen.width, screen.height, dw, dh);
  const h = Number(resolution);
  return fitToDisplay(Math.round((h * 16) / 9), h, dw, dh);
}

// The VideoConfig to ask the host for. `codec` is kept from what the host streams now.
export function videoConfig(settings, { screen, display, codec = 'h264' }) {
  const { width, height } = streamSize(settings.resolution, screen, display);
  const fps = Number(settings.fps) || DEFAULTS.fps;
  const bitrateKbps = settings.bitrate === 'auto' ? autoKbps(width, height, fps) : Number(settings.bitrate);
  const quality = QUALITIES.includes(settings.quality) ? settings.quality : DEFAULTS.quality;
  return { codec, width, height, fps, bitrateKbps, quality };
}

export function sameConfig(a, b) {
  return !!a && !!b && ['codec', 'width', 'height', 'fps', 'bitrateKbps', 'quality']
    .every((k) => a[k] === b[k] || (k === 'quality' && (a[k] ?? 'balanced') === (b[k] ?? 'balanced')));
}

// This screen in device pixels (what a fullscreen stream is shown at).
export function screenPixels(win = globalThis) {
  const dpr = win.devicePixelRatio || 1;
  return { width: Math.round(win.screen.width * dpr), height: Math.round(win.screen.height * dpr) };
}

// Storage can be missing or throw (private windows, blocked site data); settings then last for the page only.
export function load(storage = globalThis.localStorage) {
  try {
    const saved = JSON.parse(storage?.getItem(STORAGE_KEY) ?? 'null');
    return { ...DEFAULTS, ...(saved && typeof saved === 'object' ? saved : {}) };
  } catch {
    return { ...DEFAULTS };
  }
}

export function save(settings, storage = globalThis.localStorage) {
  try { storage?.setItem(STORAGE_KEY, JSON.stringify(settings)); } catch { /* not persisted */ }
}
