// Tells the host how this browser is coping, once a second, so the host can tell which end of the
// stream is the bottleneck (its encoder, the network, or this decoder) and adapt. Runs whenever
// the control channel is open, with or without the stats overlay.

import { snapshot, compute } from './stats.js';

export const INTERVAL_MS = 1000;

// The message for one interval, from stats.compute(). Pure, for testing.
export function clientStats(s, hidden) {
  const num = (v) => (v != null && Number.isFinite(v) ? v : null);
  return {
    type: 'clientStats',
    fps: num(s.fps) ?? 0,
    decodeMs: num(s.decodeMs),
    dropped: Math.max(0, s.droppedDelta | 0),
    freezes: Math.max(0, s.freezesDelta | 0),
    hidden: !!hidden,
  };
}

export class ClientReporter {
  constructor({ getPc, send, isHidden = () => document.hidden }) {
    this.getPc = getPc;
    this.send = send;
    this.isHidden = isHidden;
    this.timer = 0;
    this.prev = null;
  }

  start() {
    this.stop();
    this.timer = setInterval(() => this.tick(), INTERVAL_MS);
  }

  stop() {
    clearInterval(this.timer);
    this.timer = 0;
    this.prev = null;
  }

  async tick() {
    const pc = this.getPc();
    if (!pc) return;
    try {
      const snap = snapshot(await pc.getStats(), performance.now());
      const prev = this.prev;
      this.prev = snap;
      if (!prev || !snap.video) return;
      this.send(clientStats(compute(prev, snap), this.isHidden()));
    } catch { /* the connection is closing */ }
  }
}
