// One streaming session: signaling + RTCPeerConnection + data channels.
// The browser is the offerer. See docs/protocol.md.

import { Signaling } from './signaling.js';

export const CHANNELS = Object.freeze({
  INPUT_RT: 'input-rt',
  INPUT: 'input',
  CONTROL: 'control',
});

// Put H.264 first so the host's hardware encoder and every browser's hardware decoder line up.
export function preferCodec(codecs, mimeType) {
  const want = mimeType.toLowerCase();
  const first = codecs.filter((c) => c.mimeType.toLowerCase() === want);
  const rest = codecs.filter((c) => c.mimeType.toLowerCase() !== want);
  return [...first, ...rest];
}

// Play frames as soon as they decode instead of smoothing with a jitter buffer.
export function minimizePlayout(receiver) {
  try { receiver.jitterBufferTarget = 0; } catch { /* unsupported */ }
  try { receiver.playoutDelayHint = 0; } catch { /* unsupported */ }
}

export class Session extends EventTarget {
  constructor({ signalUrl, token, video }) {
    super();
    this.signal = new Signaling(signalUrl, token);
    this.video = video;
    this.pc = null;
    this.channels = {};
    this.pendingCandidates = [];
    this.closed = false;
  }

  status(text, state) {
    this.dispatchEvent(new CustomEvent('status', { detail: { text, state } }));
  }

  async start() {
    this.status('Connecting to host…', 'connecting');
    this.signal.addEventListener('close', () => {
      // Once media is flowing the WebSocket is not needed; only report it before then.
      if (!this.closed && this.pc?.connectionState !== 'connected') this.fail('Signaling closed');
    });
    this.signal.addEventListener('message', (e) => this.onSignal(e.detail).catch((err) => this.fail(err.message)));
    await this.signal.open();
    // The host may send "welcome" with ICE servers first; give it a moment, then offer regardless.
    this.welcomeTimer = setTimeout(() => this.createPeer([]), 1000);
  }

  async onSignal(msg) {
    switch (msg.type) {
      case 'welcome':
        clearTimeout(this.welcomeTimer);
        if (!this.pc) await this.createPeer(Array.isArray(msg.iceServers) ? msg.iceServers : []);
        break;
      case 'answer':
        await this.pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
        for (const c of this.pendingCandidates) await this.pc.addIceCandidate(c);
        this.pendingCandidates = [];
        break;
      case 'ice':
        if (!this.pc || !this.pc.remoteDescription) this.pendingCandidates.push(msg.candidate ?? null);
        else await this.pc.addIceCandidate(msg.candidate ?? null);
        break;
      case 'error':
        this.fail(msg.message || 'Host error');
        break;
    }
  }

  async createPeer(iceServers) {
    if (this.pc || this.closed) return;
    const pc = new RTCPeerConnection({ iceServers, bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
    this.pc = pc;

    const vt = pc.addTransceiver('video', { direction: 'recvonly' });
    const caps = RTCRtpReceiver.getCapabilities?.('video');
    if (caps && vt.setCodecPreferences) vt.setCodecPreferences(preferCodec(caps.codecs, 'video/H264'));
    pc.addTransceiver('audio', { direction: 'recvonly' });
    for (const t of pc.getTransceivers()) minimizePlayout(t.receiver);

    this.openChannel(CHANNELS.INPUT_RT, { ordered: false, maxRetransmits: 0 });
    this.openChannel(CHANNELS.INPUT, { ordered: true });
    this.openChannel(CHANNELS.CONTROL, { ordered: true });

    const stream = new MediaStream();
    pc.ontrack = (e) => {
      minimizePlayout(e.receiver);
      stream.addTrack(e.track);
      if (this.video.srcObject !== stream) this.video.srcObject = stream;
      this.video.play().catch(() => {});
    };
    pc.onicecandidate = (e) => this.signal.send({ type: 'ice', candidate: e.candidate ? e.candidate.toJSON() : null });
    pc.onconnectionstatechange = () => {
      const s = pc.connectionState;
      if (s === 'connected') {
        const relayed = this.isRelayed();
        relayed.then((r) => this.status(r ? 'Connected (relayed over TURN, degraded)' : 'Connected', 'connected'));
      } else if (s === 'failed') this.fail('Connection failed');
      else if (s === 'disconnected') this.status('Connection interrupted…', 'connecting');
    };

    await pc.setLocalDescription(await pc.createOffer());
    this.signal.send({ type: 'offer', sdp: pc.localDescription.sdp });
  }

  openChannel(label, opts) {
    const ch = this.pc.createDataChannel(label, opts);
    ch.binaryType = 'arraybuffer';
    this.channels[label] = ch;
    if (label === CHANNELS.CONTROL) {
      ch.onmessage = (e) => {
        if (typeof e.data !== 'string') return;
        let msg;
        try { msg = JSON.parse(e.data); } catch { return; }
        if (msg && typeof msg.type === 'string') this.dispatchEvent(new CustomEvent('control', { detail: msg }));
      };
      ch.onopen = () => this.dispatchEvent(new Event('control-open'));
    }
  }

  // Send a binary input message. Drops silently if the channel is not open or is backed up,
  // since stale input is worse than lost input.
  sendInput(label, buf) {
    const ch = this.channels[label];
    if (!ch || ch.readyState !== 'open') return false;
    if (label === CHANNELS.INPUT_RT && ch.bufferedAmount > 16 * 1024) return false;
    ch.send(buf);
    return true;
  }

  sendControl(msg) {
    const ch = this.channels[CHANNELS.CONTROL];
    if (ch && ch.readyState === 'open') ch.send(JSON.stringify(msg));
  }

  async isRelayed() {
    try {
      const stats = await this.pc.getStats();
      for (const s of stats.values()) {
        if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') {
          const local = stats.get(s.localCandidateId);
          const remote = stats.get(s.remoteCandidateId);
          return local?.candidateType === 'relay' || remote?.candidateType === 'relay';
        }
      }
    } catch { /* ignore */ }
    return false;
  }

  fail(text) {
    if (this.closed) return;
    this.close();
    this.status(text, 'failed');
  }

  close() {
    this.closed = true;
    clearTimeout(this.welcomeTimer);
    this.signal.close();
    if (this.pc) this.pc.close();
    this.pc = null;
    if (this.video.srcObject) {
      for (const t of this.video.srcObject.getTracks()) t.stop();
      this.video.srcObject = null;
    }
    this.dispatchEvent(new Event('closed'));
  }
}
