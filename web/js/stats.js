// Stream stats for the overlay, computed from successive RTCStatsReport snapshots
// plus per-frame timing from requestVideoFrameCallback.

// Pull the fields we need out of an RTCStatsReport (or any Map of stats objects).
export function snapshot(report, now) {
  const snap = { t: now, video: null, pair: null, codec: null, audio: null };
  for (const s of report.values()) {
    if (s.type === 'inbound-rtp' && s.kind === 'video') snap.video = s;
    else if (s.type === 'inbound-rtp' && s.kind === 'audio') snap.audio = s;
    else if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') snap.pair = s;
  }
  if (snap.video?.codecId) snap.codec = report.get(snap.video.codecId) ?? null;
  if (snap.pair) {
    const local = report.get(snap.pair.localCandidateId);
    const remote = report.get(snap.pair.remoteCandidateId);
    snap.relayed = local?.candidateType === 'relay' || remote?.candidateType === 'relay';
  }
  return snap;
}

const d = (a, b, k) => (a?.[k] ?? 0) - (b?.[k] ?? 0);

export function compute(prev, cur) {
  const v = cur.video;
  const pv = prev?.video;
  const dt = prev ? (cur.t - prev.t) / 1000 : 0;
  const out = {
    rttMs: cur.pair?.currentRoundTripTime != null ? cur.pair.currentRoundTripTime * 1000 : null,
    relayed: !!cur.relayed,
    codec: cur.codec?.mimeType?.split('/')[1] ?? null,
    width: v?.frameWidth ?? null,
    height: v?.frameHeight ?? null,
    fps: null, bitrateMbps: null, decodeMs: null, jitterMs: null,
    dropped: v?.framesDropped ?? 0, droppedDelta: 0,
    lost: v?.packetsLost ?? 0, lostDelta: 0,
    freezes: v?.freezeCount ?? 0,
    // Repairs: resends asked for and received, and keyframes asked for when
    // a resend didn't come in time.
    nackDelta: 0, rtxDelta: 0, pliDelta: 0,
    decoder: v?.decoderImplementation ?? null,
  };
  if (!v || !pv || dt <= 0) return out;
  const frames = d(v, pv, 'framesDecoded');
  out.fps = frames / dt;
  out.bitrateMbps = (d(v, pv, 'bytesReceived') * 8) / dt / 1e6;
  if (frames > 0) out.decodeMs = (d(v, pv, 'totalDecodeTime') / frames) * 1000;
  const emitted = d(v, pv, 'jitterBufferEmittedCount');
  if (emitted > 0) out.jitterMs = (d(v, pv, 'jitterBufferDelay') / emitted) * 1000;
  out.droppedDelta = d(v, pv, 'framesDropped');
  out.lostDelta = d(v, pv, 'packetsLost');
  out.nackDelta = d(v, pv, 'nackCount');
  out.rtxDelta = d(v, pv, 'retransmittedPacketsReceived');
  out.pliDelta = d(v, pv, 'pliCount') + d(v, pv, 'firCount');
  return out;
}

// Average time from the last packet of a frame arriving to that frame being shown.
export class FrameTimer {
  constructor(video) {
    this.video = video;
    this.samples = [];
    this.running = false;
  }

  start() {
    if (!this.video.requestVideoFrameCallback || this.running) return;
    this.running = true;
    const cb = (now, meta) => {
      if (!this.running) return;
      if (meta.receiveTime) {
        const ms = meta.expectedDisplayTime - meta.receiveTime;
        if (ms >= 0 && ms < 1000) this.samples.push(ms);
        if (this.samples.length > 120) this.samples.shift();
      }
      this.handle = this.video.requestVideoFrameCallback(cb);
    };
    this.handle = this.video.requestVideoFrameCallback(cb);
  }

  stop() {
    this.running = false;
    if (this.handle) this.video.cancelVideoFrameCallback(this.handle);
    this.samples = [];
  }

  get avgMs() {
    if (!this.samples.length) return null;
    return this.samples.reduce((a, b) => a + b, 0) / this.samples.length;
  }
}

const f = (n, digits = 0) => (n == null || !Number.isFinite(n) ? '–' : n.toFixed(digits));

export function format(s, recvToDisplayMs) {
  const est = s.rttMs != null && recvToDisplayMs != null ? s.rttMs / 2 + recvToDisplayMs : null;
  return [
    `${s.width ?? '–'}x${s.height ?? '–'} ${s.codec ?? ''} ${s.decoder ? `(${s.decoder})` : ''}`.trim(),
    `fps ${f(s.fps)}  bitrate ${f(s.bitrateMbps, 1)} Mb/s`,
    `rtt ${f(s.rttMs)} ms${s.relayed ? ' (TURN relay)' : ''}`,
    `decode ${f(s.decodeMs, 1)} ms  jitter buf ${f(s.jitterMs, 1)} ms`,
    `recv→display ${f(recvToDisplayMs, 1)} ms`,
    `latency est ${f(est, 1)} ms (rtt/2 + client; excludes host)`,
    `dropped ${s.dropped} (+${s.droppedDelta})  lost ${s.lost} (+${s.lostDelta})  freezes ${s.freezes}`,
    `resends asked +${s.nackDelta} got +${s.rtxDelta}  keyframes asked +${s.pliDelta}`,
  ].join('\n');
}

// The host's side, from its videoStats message.
export function formatHost(h) {
  if (!h) return 'host –';
  const mb = (k) => (k == null ? '–' : (k / 1000).toFixed(1));
  return [
    `host enc ${mb(h.encoderKbps)}/${mb(h.maxKbps)} Mb/s  bwe ${mb(h.bweKbps)}  sent ${mb(h.sentKbps)}`,
    `host fps ${f(h.fps)}  encode ${f(h.encodeMs, 1)} ms  keyframes ${h.keyframeRequests ?? '–'}  loss ${h.loss == null ? '–' : (h.loss * 100).toFixed(1) + '%'}`,
    // Why: the browser lost a packet no resend repaired, the host's send
    // queue fell behind, frames were dropped another way, or the host's
    // socket refused packets.
    `host keyframes: browser ${h.browserKeyframeRequests ?? '–'}  late ${h.lateFrames ?? '–'}  dropped ${h.lostFrames ?? '–'}  send drops ${h.sendDrops ?? '–'}`,
  ].join('\n');
}
