// Auto-tune (test): steps the stream through bitrates, then encoder qualities, and keeps the best
// that streams cleanly. Run it while something is moving on screen: a still desktop needs few bits,
// so the network is never really loaded, and the result says so.
//
// The policy lives here; main.js supplies apply(config), measure(ms) and wait(ms).

export const LADDER_KBPS = Object.freeze([10000, 20000, 30000, 50000, 80000, 120000, 150000]);
export const FLOOR_KBPS = 5000;
// Keep this much below the highest bitrate that passed, for the network's bad moments.
export const HEADROOM = 0.85;
const SETTLE_MS = 3000;
const MEASURE_MS = 4000;

export class Cancelled extends Error {
  constructor() { super('auto-tune cancelled'); }
}

// Why a measurement window is not good enough, or null if it is.
// m: { freezes, keyframeRequests, loss, bweKbps, hostFps, decodeMs, encodeMs } over the window
// (any may be null).
export function problem(m, config) {
  const frameMs = 1000 / config.fps;
  if (m.freezes > 0) return 'the picture froze';
  if (m.keyframeRequests > 1) return 'frames were dropped on the way';
  if (m.loss != null && m.loss > 0.02) return `${(m.loss * 100).toFixed(1)}% packet loss`;
  // The estimate only climbs as far as what is sent, and a still picture sends little, so it says
  // something only while the picture moves.
  if (moving(m, config) && m.bweKbps != null && m.bweKbps < config.bitrateKbps * 0.85) {
    return `the network only carries about ${(m.bweKbps / 1000).toFixed(0)} Mb/s`;
  }
  if (m.encodeMs != null && m.encodeMs > frameMs * 0.6) return `host encode too slow (${m.encodeMs.toFixed(1)} ms)`;
  if (m.decodeMs != null && m.decodeMs > frameMs * 0.6) return `browser decode too slow (${m.decodeMs.toFixed(1)} ms)`;
  return null;
}

// Whether the picture was moving: the host encodes at the full frame rate only then (a still
// desktop is re-sent about 10 times a second).
export function moving(m, config) {
  return m.hostFps != null && m.hostFps >= config.fps * 0.8;
}

const mbps = (kbps) => `${(kbps / 1000).toFixed(kbps % 1000 ? 1 : 0)} Mb/s`;

export class AutoTune {
  constructor({ base, apply, measure, wait, onProgress = () => {} }) {
    this.base = base;
    this.apply = apply;
    this.measure = measure;
    this.wait = wait ?? ((ms) => new Promise((r) => setTimeout(r, ms)));
    this.onProgress = onProgress;
    this.cancelled = false;
    this.log = [];
  }

  cancel() { this.cancelled = true; }

  async trial(config, label) {
    this.onProgress(`Auto-tune: trying ${label}…`);
    this.apply(config);
    await this.wait(SETTLE_MS);
    if (this.cancelled) throw new Cancelled();
    const m = await this.measure(MEASURE_MS);
    if (this.cancelled) throw new Cancelled();
    const why = problem(m, config);
    this.log.push({ label, ok: !why, why, metrics: m, moving: moving(m, config) });
    return !why;
  }

  // Resolves to { config, summary }. Rejects with Cancelled if cancel() was called.
  async run() {
    // Bitrate first, at the fastest encoder setting so encode time doesn't cloud the network test.
    let best = null;
    for (const kbps of LADDER_KBPS) {
      if (!(await this.trial({ ...this.base, bitrateKbps: kbps, quality: 'speed' }, mbps(kbps)))) break;
      best = kbps;
    }
    const bitrateKbps = best == null
      ? FLOOR_KBPS
      : Math.max(FLOOR_KBPS, Math.round((best * HEADROOM) / 500) * 500);

    // Then the slowest encoder setting that still keeps up, at that bitrate.
    let quality = 'speed';
    for (const q of ['clarity', 'balanced']) {
      if (await this.trial({ ...this.base, bitrateKbps, quality: q }, `${q} at ${mbps(bitrateKbps)}`)) {
        quality = q;
        break;
      }
    }
    const config = { ...this.base, bitrateKbps, quality };
    this.apply(config);
    const failed = this.log.find((t) => !t.ok);
    const still = !this.log.some((t) => t.moving);
    const summary = `Auto-tune: ${mbps(bitrateKbps)}, ${quality}` +
      (failed ? ` (${failed.label} failed: ${failed.why})` : '') +
      (still ? ' (the picture was mostly still, so the network was not really tested)' : '');
    return { config, summary };
  }
}
