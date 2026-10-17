// One streaming session: signaling + RTCPeerConnection + data channels.
// The browser is the offerer and creates the data channels. Message names follow
// ws-proto's SignalMessage; see docs/protocol.md.

import { Signaling } from './signaling.js';

export const CHANNELS = Object.freeze({
  INPUT_RT: 'input-rt',
  INPUT: 'input',
  CONTROL: 'control',
  FILE: 'file',
});

// Queued motion beyond this is stale; hold it back and send the sum later instead.
export const INPUT_RT_MAX_BUFFERED = 1024;

const BYE_TEXT = {
  replaced: 'Disconnected: another browser took over the session',
  hostShutdown: 'Disconnected: host shut down',
  revoked: 'Disconnected: this browser was unpaired',
};

// Put H.264 first so the host's hardware encoder and the browser's hardware decoder line up.
export function preferCodec(codecs, mimeType) {
  const want = mimeType.toLowerCase();
  const first = codecs.filter((c) => c.mimeType.toLowerCase() === want);
  const rest = codecs.filter((c) => c.mimeType.toLowerCase() !== want);
  return [...first, ...rest];
}

// Play frames as soon as they decode instead of smoothing with a jitter buffer.
export function minimizePlayout(receiver) {
  try { receiver.jitterBufferTarget = 0; } catch { /* unsupported */ }
}

export class Session extends EventTarget {
  constructor({ signalUrl, video }) {
    super();
    this.signal = new Signaling(signalUrl);
    this.video = video;
    this.pc = null;
    this.channels = {};
    this.pendingCandidates = [];
    this.closed = false;
    this.restarts = 0;
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
    // The host sends iceServers first; if it doesn't arrive promptly, offer without them.
    this.iceTimer = setTimeout(() => this.createPeer([]), 1000);
  }

  async onSignal(msg) {
    switch (msg.type) {
      case 'iceServers':
        clearTimeout(this.iceTimer);
        if (!this.pc) await this.createPeer(Array.isArray(msg.servers) ? msg.servers : []);
        break;
      case 'answer':
        await this.pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
        for (const c of this.pendingCandidates) await this.pc.addIceCandidate(c);
        this.pendingCandidates = [];
        break;
      case 'iceCandidate': {
        const c = { candidate: msg.candidate, sdpMid: msg.sdpMid, sdpMLineIndex: msg.sdpMLineIndex };
        if (!this.pc?.remoteDescription) this.pendingCandidates.push(c);
        else await this.pc.addIceCandidate(c);
        break;
      }
      case 'error':
        this.status(`Host error: ${msg.message}`, 'failed');
        break;
      case 'bye':
        this.fail(BYE_TEXT[msg.reason] || `Disconnected: ${msg.reason}`);
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
    // sendrecv from the start so turning the mic on later is just replaceTrack, no renegotiation.
    this.audio = pc.addTransceiver('audio', { direction: 'sendrecv' });
    for (const t of pc.getTransceivers()) minimizePlayout(t.receiver);

    this.openChannel(CHANNELS.INPUT_RT, { ordered: false, maxRetransmits: 0 });
    this.openChannel(CHANNELS.INPUT, { ordered: true });
    this.openChannel(CHANNELS.CONTROL, { ordered: true });
    this.openChannel(CHANNELS.FILE, { ordered: true });

    const stream = new MediaStream();
    pc.ontrack = (e) => {
      minimizePlayout(e.receiver);
      stream.addTrack(e.track);
      if (this.video.srcObject !== stream) this.video.srcObject = stream;
      this.video.play().catch(() => {});
    };
    pc.onicecandidate = (e) => {
      if (e.candidate) {
        this.signal.send({ type: 'iceCandidate', candidate: e.candidate.candidate, sdpMid: e.candidate.sdpMid, sdpMLineIndex: e.candidate.sdpMLineIndex });
      }
    };
    pc.onconnectionstatechange = () => {
      const s = pc.connectionState;
      if (s === 'connected') {
        this.restarts = 0;
        this.isRelayed().then((r) => this.status(r ? 'Connected (relayed, degraded)' : 'Connected', 'connected'));
      } else if (s === 'failed') this.restartIce();
      else if (s === 'disconnected') this.status('Connection interrupted…', 'connecting');
    };

    await this.offer();
  }

  async offer(opts) {
    await this.pc.setLocalDescription(await this.pc.createOffer(opts));
    this.signal.send({ type: 'offer', sdp: this.pc.localDescription.sdp });
  }

  restartIce() {
    if (this.closed) return;
    if (this.restarts >= 3 || !this.signal.isOpen) {
      this.fail('Connection failed');
      return;
    }
    this.restarts++;
    this.status('Connection lost, retrying…', 'connecting');
    this.offer({ iceRestart: true }).catch((err) => this.fail(err.message));
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

  // True when a message on this channel would be sent now. The motion channel reports
  // false when its queue is backed up so the caller can coalesce instead.
  canSend(label) {
    const ch = this.channels[label];
    if (!ch || ch.readyState !== 'open') return false;
    return label !== CHANNELS.INPUT_RT || ch.bufferedAmount <= INPUT_RT_MAX_BUFFERED;
  }

  sendInput(label, buf) {
    if (!this.canSend(label)) return false;
    this.channels[label].send(buf);
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
          return [local, remote].some((c) => c?.candidateType === 'relay' || c?.protocol === 'tcp');
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
    if (this.closed) return;
    this.closed = true;
    clearTimeout(this.iceTimer);
    this.signal.close();
    if (this.pc) this.pc.close();
    this.pc = null;
    this.channels = {};
    if (this.video.srcObject) {
      for (const t of this.video.srcObject.getTracks()) t.stop();
      this.video.srcObject = null;
    }
    this.dispatchEvent(new Event('closed'));
  }
}
