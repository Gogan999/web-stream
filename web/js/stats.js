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
    // ICE-TCP: the host's TCP fallback when UDP can't get through.
    snap.tcp = local?.protocol === 'tcp' || remote?.protocol === 'tcp';
    snap.route = `${local?.candidateType ?? '?'}/${local?.protocol ?? '?'} -> ${remote?.candidateType ?? '?'}/${remote?.protocol ?? '?'}`;
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
    tcp: !!cur.tcp,
    route: cur.route ?? null,
    codec: cur.codec?.mimeType?.split('/')[1] ?? null,
    width: v?.frameWidth ?? null,
    height: v?.frameHeight ?? null,
    fps: null, bitrateMbps: null, decodeMs: null, jitterMs: null,
    dropped: v?.framesDropped ?? 0, droppedDelta: 0,
    lost: v?.packetsLost ?? 0, lostDelta: 0,
    lossPct: null,
    freezes: v?.freezeCount ?? 0, freezesDelta: 0,
    // Repairs: resends asked for and received, and keyframes asked for when
    // a resend didn't come in time.
    nackDelta: 0, rtxDelta: 0, pliDelta: 0,
    decoder: v?.decoderImplementation ?? null,
    // Running totals, for the debug level.
    totals: {
      packets: v?.packetsReceived ?? 0, frames: v?.framesDecoded ?? 0, keyframes: v?.keyFramesDecoded ?? 0,
      nacks: v?.nackCount ?? 0, plis: (v?.pliCount ?? 0) + (v?.firCount ?? 0), freezeSec: v?.totalFreezesDuration ?? 0,
    },
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
  const expected = out.lostDelta + d(v, pv, 'packetsReceived');
  if (expected > 0) out.lossPct = (Math.max(0, out.lostDelta) / expected) * 100;
  out.freezesDelta = d(v, pv, 'freezeCount');
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
    // Why keyframes happen: the browser lost a packet no resend repaired, the host's send queue fell
    // behind, frames were dropped another way, or the host's socket refused packets. Late and dropped
    // frames the encoder could skip over ("repaired") cost no keyframe.
    `host keyframes: browser ${h.browserKeyframeRequests ?? '–'}  late ${h.lateFrames ?? '–'}  dropped ${h.lostFrames ?? '–'}  repaired ${h.recoveredFrames ?? '–'}  send drops ${h.sendDrops ?? '–'}`,
  ].join('\n');
}

// Reporting levels for the overlay. Essential: is the stream healthy, and if not, why, in plain words.
// Detailed: every browser and host figure. Debug: adds running totals, the ICE route and raw host stats.
export const LEVELS = Object.freeze(['essential', 'detailed', 'debug']);

// What is wrong with the stream right now, worst first. `s` is from compute(); `host` the host's last
// videoStats; `config` the VideoConfig the host runs. Each has a key (to hold it on screen), a
// severity ('warn' or 'bad') and the text.
export function assess(s, host, config) {
  const out = [];
  const add = (key, sev, text) => out.push({ key, sev, text });
  const mb = (k) => (k / 1000).toFixed(1);
  if (s.width && s.height && config?.width && config?.height && s.width * s.height < 0.9 * config.width * config.height) {
    add('resolution', 'warn', `Reduced resolution ${s.width}x${s.height} (asked ${config.width}x${config.height}): host scaled down for the network`);
  }
  if (host?.encoderKbps != null && host.maxKbps > 0 && host.encoderKbps < 0.6 * host.maxKbps) {
    const bwe = host.bweKbps != null ? `, network carries ~${mb(host.bweKbps)}` : '';
    add('bitrate', host.encoderKbps < 0.25 * host.maxKbps ? 'bad' : 'warn',
      `Reduced bitrate ${mb(host.encoderKbps)} of ${mb(host.maxKbps)} Mb/s${bwe}`);
  }
  if (s.rttMs != null && s.rttMs >= 80) add('rtt', s.rttMs >= 150 ? 'bad' : 'warn', `Slow connection: round trip ${s.rttMs.toFixed(0)} ms`);
  const loss = s.lossPct ?? (host?.loss != null ? host.loss * 100 : null);
  if (loss != null && loss >= 1) add('loss', loss >= 5 ? 'bad' : 'warn', `Lossy connection: ${loss.toFixed(1)}% packets lost`);
  if (s.tcp) add('tcp', 'warn', 'TCP fallback in use: a lost packet stalls the stream until resent');
  if (s.relayed) add('relay', 'warn', 'Relayed through TURN: extra hop adds delay');
  if (s.freezesDelta > 0) add('freeze', 'bad', 'Picture froze');
  if (s.droppedDelta > 0) add('dropped', 'warn', `Browser dropped ${s.droppedDelta} frame${s.droppedDelta > 1 ? 's' : ''}`);
  const hostDropped = (host?.lateFrames ?? 0) + (host?.lostFrames ?? 0);
  if (hostDropped > 0) add('host-dropped', 'warn', `Host dropped ${hostDropped} frame${hostDropped > 1 ? 's' : ''} (network fell behind)`);
  if (s.pliDelta > 0 || host?.keyframeRequests > 0) add('keyframe', 'warn', 'Keyframe needed to repair the picture (brief stutter)');
  if (host?.sendDrops > 0) add('send-drops', 'warn', `Host socket full: ${host.sendDrops} packets refused`);
  // A still screen sends few frames, so a low rate only counts when the host sends more than is shown.
  if (s.fps != null && host?.fps > 10 && s.fps < 0.8 * host.fps) {
    add('fps', 'warn', `Showing ${s.fps.toFixed(0)} of ${host.fps.toFixed(0)} fps the host sends`);
  }
  const budget = config?.fps > 0 ? 1000 / config.fps : null;
  if (budget && s.decodeMs != null && s.decodeMs > budget) add('decode', 'warn', `Slow decode: ${s.decodeMs.toFixed(1)} ms per frame (budget ${budget.toFixed(1)})`);
  if (budget && host?.encodeMs > budget) add('encode', 'warn', `Slow host encode: ${host.encodeMs.toFixed(1)} ms per frame (budget ${budget.toFixed(1)})`);
  return out.sort((a, b) => (a.sev === b.sev ? 0 : a.sev === 'bad' ? -1 : 1));
}

// Keeps each warning on screen for `holdMs` after it was last seen, so a one-off drop can still be read.
export class Alerts {
  constructor(holdMs = 4000) {
    this.holdMs = holdMs;
    this.seen = new Map();
  }

  update(list, now) {
    for (const a of list) this.seen.set(a.key, { ...a, at: now });
    for (const [k, a] of this.seen) if (now - a.at > this.holdMs) this.seen.delete(k);
    return [...this.seen.values()].sort((a, b) => (a.sev === b.sev ? 0 : a.sev === 'bad' ? -1 : 1));
  }

  clear() {
    this.seen.clear();
  }
}

function route(s) {
  if (s.relayed) return 'TURN relay';
  return s.tcp ? 'TCP fallback' : 'direct UDP';
}

// The overlay's lines for a level, as { text, sev } (sev only on warnings). `alerts` from Alerts.update.
export function render(level, s, recvToDisplayMs, host, config, alerts) {
  const lines = alerts.length ? alerts.map((a) => ({ text: `! ${a.text}`, sev: a.sev })) : [{ text: 'OK', sev: 'ok' }];
  const est = s.rttMs != null && recvToDisplayMs != null ? s.rttMs / 2 + recvToDisplayMs : null;
  if (level === 'essential') {
    const want = config?.fps ? ` / ${config.fps}` : '';
    lines.push(
      { text: `${s.width ?? '–'}x${s.height ?? '–'}  fps ${f(s.fps)}${want}  bitrate ${f(s.bitrateMbps, 1)} Mb/s` },
      { text: `rtt ${f(s.rttMs)} ms  ${route(s)}  latency est ${f(est, 0)} ms` },
    );
    return lines;
  }
  for (const text of `${format(s, recvToDisplayMs)}\n${formatHost(host)}`.split('\n')) lines.push({ text });
  lines.push({ text: `connection ${route(s)}  loss ${s.lossPct == null ? '–' : s.lossPct.toFixed(1) + '%'}` });
  if (level === 'debug') {
    const t = s.totals;
    lines.push(
      { text: `route ${s.route ?? '–'}` },
      { text: `asked ${config ? `${config.width}x${config.height}@${config.fps} ${config.codec} ${(config.bitrateKbps / 1000).toFixed(1)} Mb/s ${config.quality ?? ''}` : '–'}` },
      { text: `totals packets ${t.packets}  frames ${t.frames}  keyframes ${t.keyframes}  nacks ${t.nacks}  plis ${t.plis}  frozen ${t.freezeSec.toFixed(1)} s` },
      { text: `host raw ${host ? JSON.stringify({ ...host, type: undefined }) : '–'}` },
    );
  }
  return lines;
}
