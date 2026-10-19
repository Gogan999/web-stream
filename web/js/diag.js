// Temporary diagnostic log for chasing freezes and unexpected keyframes. Off unless the page is
// opened with ?diag (or localStorage 'ws-diag' is '1'). Samples the video receiver's stats every
// 250 ms, notes every gap between shown frames over 50 ms, and keeps the host's videoStats, all
// with wall-clock times to line up with the host's WS_DIAG log. "Save diag log" downloads it.

const SAMPLE_MS = 250;
const FREEZE_MS = 50;
// About an hour of samples; the oldest go first.
const MAX_LINES = 50000;

// Cumulative inbound video counters worth logging; deltas are worked out when reading the log.
const VIDEO_FIELDS = [
  'framesReceived', 'framesDecoded', 'framesDropped', 'keyFramesDecoded', 'freezeCount',
  'totalFreezesDuration', 'pauseCount', 'packetsReceived', 'packetsLost', 'nackCount', 'pliCount',
  'firCount', 'retransmittedPacketsReceived', 'bytesReceived', 'totalDecodeTime',
  'jitterBufferDelay', 'jitterBufferEmittedCount', 'frameWidth', 'frameHeight',
];

export function diagEnabled(params, storage) {
  if (params.has('diag')) return params.get('diag') !== '0';
  try { return storage?.getItem('ws-diag') === '1'; } catch { return false; }
}

// The fields above from an inbound-rtp video stats object, plus the decoder (logged when it
// changes, since a fallback to software decode is itself a clue).
export function videoSample(s) {
  const out = {};
  for (const k of VIDEO_FIELDS) if (s[k] != null) out[k] = typeof s[k] === 'number' ? +s[k].toFixed(4) : s[k];
  return out;
}

export class DiagLog {
  constructor() {
    this.lines = [];
    this.timer = 0;
    this.decoder = null;
    this.lastFrame = null;
    this.frameHandle = 0;
  }

  add(kind, data) {
    this.lines.push(`${Date.now()} ${performance.now().toFixed(1)} ${kind} ${JSON.stringify(data)}`);
    if (this.lines.length > MAX_LINES) this.lines.splice(0, this.lines.length - MAX_LINES);
  }

  start(pc, video) {
    this.stop();
    this.add('start', { userAgent: navigator.userAgent });
    this.timer = setInterval(async () => {
      let report;
      try { report = await pc.getStats(); } catch { return; }
      for (const s of report.values()) {
        if (s.type === 'inbound-rtp' && s.kind === 'video') {
          this.add('video', videoSample(s));
          if (s.decoderImplementation && s.decoderImplementation !== this.decoder) {
            this.decoder = s.decoderImplementation;
            this.add('decoder', { decoder: this.decoder, powerEfficient: s.powerEfficientDecoder ?? null });
          }
        } else if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') {
          this.add('pair', { rttMs: s.currentRoundTripTime != null ? +(s.currentRoundTripTime * 1000).toFixed(1) : null, bytesReceived: s.bytesReceived });
        }
      }
    }, SAMPLE_MS);
    if (video.requestVideoFrameCallback) {
      const cb = (now, meta) => {
        if (this.lastFrame && now - this.lastFrame.now > FREEZE_MS) {
          this.add('frame_gap', {
            gapMs: +(now - this.lastFrame.now).toFixed(1),
            framesBetween: meta.presentedFrames - this.lastFrame.presented - 1,
            rtpTimestamp: meta.rtpTimestamp ?? null,
            recvToShowMs: meta.receiveTime ? +(meta.expectedDisplayTime - meta.receiveTime).toFixed(1) : null,
          });
        }
        this.lastFrame = { now, presented: meta.presentedFrames };
        this.frameHandle = video.requestVideoFrameCallback(cb);
      };
      this.frameHandle = video.requestVideoFrameCallback(cb);
      this.video = video;
    }
  }

  stop() {
    clearInterval(this.timer);
    this.timer = 0;
    if (this.frameHandle) this.video?.cancelVideoFrameCallback(this.frameHandle);
    this.frameHandle = 0;
    this.lastFrame = null;
    this.decoder = null;
  }

  save() {
    const blob = new Blob([this.lines.join('\n') + '\n'], { type: 'text/plain' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = `ws-diag-browser-${Math.floor(Date.now() / 1000)}.log`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }
}
