// What "healthy" means for the stream, in one place. The stats overlay (stats.js) and auto-tune
// (autotune.js) both judge a stream by these limits, so a warning on screen and an auto-tune failure
// mean the same thing. Pure functions over plain numbers; any input may be null (unknown = fine).

export const LIMITS = Object.freeze({
  // Packet loss, percent of packets.
  lossWarnPct: 1,
  lossBadPct: 5,
  // Round trip, ms.
  rttWarnMs: 80,
  rttBadMs: 150,
  // Per-frame time as a share of the frame interval (1000 / fps): decode in the browser, and capture
  // to encoded on the host. Decode needs headroom for jitter; encode may use up to a whole interval.
  decodeShare: 0.6,
  encodeShare: 1,
  // The bandwidth estimate must reach this share of the target bitrate, while the picture moves.
  bandwidthShare: 0.85,
  // The host sends at this share of the asked frame rate or more only while the picture moves (a still
  // desktop is re-sent about 10 times a second).
  movingShare: 0.8,
  // The encoder's target fell below this share of its ceiling (severe: below the second one).
  reducedBitrateShare: 0.6,
  severeBitrateShare: 0.25,
  // Keyframe requests tolerated in one auto-tune window.
  keyframesTolerated: 1,
});

export const frameMs = (fps) => (fps > 0 ? 1000 / fps : null);

// null, 'warn' or 'bad'.
export function lossLevel(pct) {
  if (pct == null || pct < LIMITS.lossWarnPct) return null;
  return pct >= LIMITS.lossBadPct ? 'bad' : 'warn';
}

export function rttLevel(ms) {
  if (ms == null || ms < LIMITS.rttWarnMs) return null;
  return ms >= LIMITS.rttBadMs ? 'bad' : 'warn';
}

// The slowest decode or encode that is still healthy at this frame rate, in ms (null: unknown rate).
export function decodeLimitMs(fps) {
  const f = frameMs(fps);
  return f == null ? null : f * LIMITS.decodeShare;
}
export function encodeLimitMs(fps) {
  const f = frameMs(fps);
  return f == null ? null : f * LIMITS.encodeShare;
}
export const decodeSlow = (ms, fps) => ms != null && decodeLimitMs(fps) != null && ms > decodeLimitMs(fps);
export const encodeSlow = (ms, fps) => ms != null && encodeLimitMs(fps) != null && ms > encodeLimitMs(fps);

// Whether the picture is moving, from the host's actual frame rate.
export const moving = (hostFps, fps) => hostFps != null && hostFps >= fps * LIMITS.movingShare;

// The estimate only climbs as far as what is sent, so it says something only while the picture moves.
export const bandwidthShort = (bweKbps, targetKbps, isMoving) =>
  isMoving && bweKbps != null && bweKbps < targetKbps * LIMITS.bandwidthShare;

// The encoder's target sits well under its ceiling and the encoder uses what it is given. The target
// follows the bandwidth estimate, which sinks whenever little is sent (a still picture), so a lower
// target with the encoder well under it is not a limit. Returns null, 'warn' or 'bad'.
export function bitrateReduced(host) {
  if (host?.encoderKbps == null || !(host.maxKbps > 0)) return null;
  const encoderUsed = host.sentKbps == null || host.sentKbps >= 0.8 * host.encoderKbps;
  if (!encoderUsed || host.encoderKbps >= LIMITS.reducedBitrateShare * host.maxKbps) return null;
  return host.encoderKbps < LIMITS.severeBitrateShare * host.maxKbps ? 'bad' : 'warn';
}
