// Measures the stream from inside the client page, for tests against the test host's synthetic
// source (test-host --synthetic). Load after stamp.js with addInitScript. It:
//  - reads the stamp in every frame the <video> presents (requestVideoFrameCallback), which gives
//    each frame's index, display and capture time, so latency, lost frames and freezes follow;
//  - listens to game audio for the beep the host plays at the top of every Unix second, whose
//    lateness is the audio latency;
//  - keeps the peer connections, for getStats;
//  - with window.__probeConfig.mediaPort / proxyPort set, points the browser at a proxy instead
//    of the host's media port (rewrites the offer's candidates), so a test can impair the network.
// Results collect in window.__probe; the Node side summarizes them (metrics.js).
(() => {
  const P = (window.__probe = { frames: [], beeps: [], pcs: [], errors: [], audioState: 'none' });
  const cfg = () => window.__probeConfig || {};

  // Peer connections, and the candidate rewrite.
  const Orig = window.RTCPeerConnection;
  window.RTCPeerConnection = class extends Orig {
    constructor(...args) {
      super(...args);
      P.pcs.push(this);
      this.addEventListener('track', (e) => {
        if (e.track.kind === 'audio') listenForBeeps(e.track);
      });
    }

    setRemoteDescription(desc) {
      const { mediaPort, proxyPort } = cfg();
      if (desc && desc.sdp && mediaPort && proxyPort) {
        desc = {
          type: desc.type,
          sdp: desc.sdp.replace(/^(a=candidate:\S+ \d+ udp \d+ \S+ )(\d+)( typ .*)$/gim, (m, a, port, b) =>
            Number(port) === mediaPort ? `${a}${proxyPort}${b}` : m),
        };
      }
      return super.setRemoteDescription(desc);
    }
  };

  // Video: one record per presented frame.
  let canvas, ctx;
  function onFrame(_now, md) {
    const video = document.getElementById('video');
    try {
      const vw = video.videoWidth;
      const cell = Math.floor(vw / wsStamp.COLS);
      if (cell >= 2) {
        if (!canvas) {
          canvas = new OffscreenCanvas(wsStamp.COLS, 2);
          ctx = canvas.getContext('2d', { willReadFrequently: true });
          ctx.imageSmoothingEnabled = false; // one source pixel per cell, taken from the cell's middle
        }
        ctx.drawImage(video, cell / 2 - 0.5, cell / 2 - 0.5, wsStamp.COLS * cell, 2 * cell, 0, 0, wsStamp.COLS, 2);
        const s = wsStamp.decodeRgba(ctx.getImageData(0, 0, wsStamp.COLS, 2).data);
        const shown = performance.timeOrigin + md.expectedDisplayTime;
        P.frames.push(s
          ? {
            frame: s.frame,
            display: s.display,
            shown,
            latency: wsStamp.age(s.time, shown),
            // Capture to arrival of the frame's last packet: network plus jitter buffer entry.
            network: md.receiveTime ? wsStamp.age(s.time, performance.timeOrigin + md.receiveTime) : null,
            presented: md.presentedFrames,
            width: vw,
            height: video.videoHeight,
          }
          : { bad: true, shown });
      }
    } catch (e) {
      P.errors.push(String(e));
    }
    video.requestVideoFrameCallback(onFrame);
  }
  function watchVideo() {
    const video = document.getElementById('video');
    if (!video) return setTimeout(watchVideo, 50);
    video.requestVideoFrameCallback(onFrame);
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', watchVideo);
  else watchVideo();

  // Audio: the time each beep starts playing, as Unix ms.
  // (A ScriptProcessor, not an AudioWorklet: the page's CSP blocks the blob: URL a worklet loads from.)
  async function listenForBeeps(track) {
    try {
      const ac = new AudioContext();
      const node = ac.createScriptProcessor(256, 1, 1);
      let quiet = 1e9; // samples of quiet before now
      node.onaudioprocess = (e) => {
        const ch = e.inputBuffer.getChannelData(0);
        let peak = 0;
        for (let i = 0; i < ch.length; i++) peak = Math.max(peak, Math.abs(ch[i]));
        // A beep onset: loud after at least 50 ms of quiet.
        if (peak > 0.08 && quiet >= 0.05 * ac.sampleRate) {
          const ts = ac.getOutputTimestamp();
          const heard = ts.performanceTime + (e.playbackTime - ts.contextTime) * 1000;
          P.beeps.push(performance.timeOrigin + heard);
        }
        quiet = peak < 0.02 ? quiet + ch.length : 0;
      };
      ac.createMediaStreamSource(new MediaStream([track])).connect(node);
      node.connect(ac.destination); // silent: the node writes nothing to its output
      await ac.resume();
      P.audioState = ac.state;
    } catch (e) {
      P.audioState = 'error';
      P.errors.push('audio: ' + e);
    }
  }

  // Browser figures for the inbound video and audio.
  P.stats = async () => {
    const pc = P.pcs.findLast((p) => p.connectionState === 'connected') || P.pcs.at(-1);
    if (!pc) return null;
    const out = {};
    (await pc.getStats()).forEach((r) => {
      if (r.type === 'inbound-rtp') {
        out[r.kind] = {
          packetsReceived: r.packetsReceived, packetsLost: r.packetsLost, bytesReceived: r.bytesReceived,
          framesDecoded: r.framesDecoded, framesDropped: r.framesDropped, keyFramesDecoded: r.keyFramesDecoded,
          freezeCount: r.freezeCount, totalFreezesDuration: r.totalFreezesDuration, pliCount: r.pliCount,
          nackCount: r.nackCount, jitterBufferDelay: r.jitterBufferDelay, jitterBufferEmittedCount: r.jitterBufferEmittedCount,
          concealedSamples: r.concealedSamples, totalSamplesReceived: r.totalSamplesReceived, audioLevel: r.audioLevel,
          decoderImplementation: r.decoderImplementation, frameWidth: r.frameWidth, frameHeight: r.frameHeight,
        };
      } else if (r.type === 'candidate-pair' && r.nominated && r.state === 'succeeded') {
        out.rttMs = r.currentRoundTripTime != null ? r.currentRoundTripTime * 1000 : null;
      }
    });
    return out;
  };
})();
