// Auto-tune (test): steps the stream through bitrates, then encoder qualities, and keeps the best
// that streams cleanly. Run it while something is moving on screen: a still desktop needs few bits,
// so the network is never really loaded, and the result says so.
//
// What counts as a problem is health.js, shared with the stats overlay.
// The policy lives here; main.js supplies apply(config), measure(ms) and wait(ms).

import { LIMITS, lossLevel, bandwidthShort, decodeSlow, encodeSlow, moving as isMoving } from './health.js';

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
// `encode` says whether encode time counts: not while stepping bitrate, since it doesn't depend on
// bitrate (a host whose encoder is slow at any bitrate would otherwise fail every step and end on the
// floor), only while picking the encoder quality.
export function problem(m, config, { encode = true } = {}) {
  if (m.freezes > 0) return 'the picture froze';
  if (m.keyframeRequests > LIMITS.keyframesTolerated) return 'frames were dropped on the way';
  if (m.loss != null && lossLevel(m.loss * 100)) return `${(m.loss * 100).toFixed(1)}% packet loss`;
  if (bandwidthShort(m.bweKbps, config.bitrateKbps, moving(m, config))) {
    return `the network only carries about ${(m.bweKbps / 1000).toFixed(0)} Mb/s`;
  }
  if (encode && encodeSlow(m.encodeMs, config.fps)) return `host encode too slow (${m.encodeMs.toFixed(1)} ms)`;
  if (decodeSlow(m.decodeMs, config.fps)) return `browser decode too slow (${m.decodeMs.toFixed(1)} ms)`;
  return null;
}

// Whether the picture was moving: the host encodes at the full frame rate only then.
export function moving(m, config) {
  return isMoving(m.hostFps, config.fps);
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

  async trial(config, label, opts) {
    this.onProgress(`Auto-tune: trying ${label}…`);
    this.apply(config);
    await this.wait(SETTLE_MS);
    if (this.cancelled) throw new Cancelled();
    const m = await this.measure(MEASURE_MS);
    if (this.cancelled) throw new Cancelled();
    const why = problem(m, config, opts);
    this.log.push({ label, ok: !why, why, metrics: m, moving: moving(m, config) });
    return !why;
  }

  // Resolves to { config, summary }. Rejects with Cancelled if cancel() was called.
  async run() {
    // Bitrate first, at the fastest encoder setting so encode time doesn't cloud the network test.
    let best = null;
    for (const kbps of LADDER_KBPS) {
      if (!(await this.trial({ ...this.base, bitrateKbps: kbps, quality: 'speed' }, mbps(kbps), { encode: false }))) break;
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
