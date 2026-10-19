// One streaming session: signaling + RTCPeerConnection + data channels.
// The host is the offerer and opens the data channels. See docs/signaling.md.

import { Signaling } from './signaling.js';

export const CHANNELS = Object.freeze({
  INPUT_RT: 'input-motion',
  INPUT: 'input',
  CONTROL: 'control',
  FILE: 'file',
});

// Queued motion beyond this is stale; hold it back and send the sum later instead.
export const INPUT_RT_MAX_BUFFERED = 1024;

const BYE_TEXT = {
  replaced: 'Disconnected: another browser took over the session',
  host_shutdown: 'Disconnected: host shut down',
  hostShutdown: 'Disconnected: host shut down',
  revoked: 'Disconnected: this browser was unpaired',
};

// Play video frames as soon as they decode instead of smoothing with a jitter buffer. Audio keeps
// Chrome's adaptive buffer: forced to zero it constantly speeds up and stretches the sound to keep up,
// which is audible as warble.
export function minimizePlayout(receiver) {
  if (receiver.track?.kind !== 'video') return;
  try { receiver.jitterBufferTarget = 0; } catch { /* unsupported */ }
}

// The host sends stereo Opus, but Chrome decodes Opus as mono unless its own answer says stereo=1.
export function withStereoOpus(sdp) {
  const pts = [...sdp.matchAll(/^a=rtpmap:(\d+) opus\/48000\/2\r?$/gim)].map((m) => m[1]);
  let out = sdp;
  for (const pt of pts) {
    out = out.replace(new RegExp(`^(a=fmtp:${pt} )(.*?)(\r?)$`, 'm'), (line, head, params, cr) =>
      /(^|;)\s*stereo=/.test(params) ? line : `${head}${params};stereo=1;sprop-stereo=1${cr}`);
  }
  return out;
}

export class Session extends EventTarget {
  constructor({ signalUrl, video }) {
    super();
    this.signal = new Signaling(signalUrl);
    this.video = video;
    this.pc = null;
    this.iceServers = [];
    this.channels = {};
    this.closed = false;
    this.restarts = 0;
    this.micTrack = null; // sent on the audio transceiver while set
  }

  status(text, state) {
    this.dispatchEvent(new CustomEvent('status', { detail: { text, state } }));
  }

  async start() {
    this.status('Connecting to host…', 'connecting');
    this.signal.addEventListener('close', (e) => {
      if (this.closed) return;
      // Unauthenticated upgrades are refused before the socket opens; the page re-checks pairing.
      if (this.pc?.connectionState === 'connected') return; // media no longer needs signaling
      this.fail(e.detail?.code === 1006 && !this.pc ? 'Could not reach host (or not paired)' : 'Signaling closed');
    });
    this.signal.addEventListener('message', (e) => this.onSignal(e.detail).catch((err) => this.fail(err.message)));
    await this.signal.open();
  }

  async onSignal(msg) {
    switch (msg.type) {
      case 'welcome':
        this.iceServers = Array.isArray(msg.ice_servers) ? msg.ice_servers : [];
        break;
      case 'offer':
        await this.answer(msg.sdp);
        break;
      case 'error':
        this.status(`Host error: ${msg.message || msg.code}`, 'failed');
        break;
      case 'bye':
        this.fail(BYE_TEXT[msg.reason] || `Disconnected: ${msg.reason}`);
        break;
    }
  }

  // A restart brings a fresh offer; answer it on a fresh peer connection.
  async answer(sdp) {
    if (this.closed) return;
    this.closePeer();
    const pc = new RTCPeerConnection({ iceServers: this.iceServers, bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
    this.pc = pc;

    const stream = new MediaStream();
    pc.ontrack = (e) => {
      minimizePlayout(e.receiver);
      stream.addTrack(e.track);
      if (this.video.srcObject !== stream) this.video.srcObject = stream;
      this.video.play().catch(() => {});
    };
    pc.ondatachannel = ({ channel }) => this.onChannel(channel);
    pc.onicecandidate = (e) => {
      if (e.candidate) {
        this.signal.send({ type: 'candidate', candidate: e.candidate.candidate, sdpMid: e.candidate.sdpMid, sdpMLineIndex: e.candidate.sdpMLineIndex });
      }
    };
    pc.onconnectionstatechange = () => {
      if (pc !== this.pc) return;
      const s = pc.connectionState;
      if (s === 'connected') {
        this.restarts = 0;
        this.isRelayed().then((r) => this.status(r ? 'Connected (TCP fallback, degraded)' : 'Connected', 'connected'));
      } else if (s === 'failed') this.restart();
      else if (s === 'disconnected') this.status('Connection interrupted…', 'connecting');
    };

    await pc.setRemoteDescription({ type: 'offer', sdp });
    for (const t of pc.getTransceivers()) {
      minimizePlayout(t.receiver);
      // Audio is always answered sendrecv so the mic can come and go with replaceTrack, without a new
      // offer; with no track the browser sends nothing.
      if (t.receiver.track.kind === 'audio') {
        t.direction = 'sendrecv';
        await t.sender.replaceTrack(this.micTrack);
      }
    }
    const answer = await pc.createAnswer();
    await pc.setLocalDescription({ type: 'answer', sdp: withStereoOpus(answer.sdp) });
    this.signal.send({ type: 'answer', sdp: pc.localDescription.sdp });
  }

  // Sends `track` (or nothing, for null) as the mic, now and on any later peer connection.
  async setMicTrack(track) {
    this.micTrack = track;
    const t = this.pc?.getTransceivers().find((x) => x.receiver.track.kind === 'audio');
    if (t) await t.sender.replaceTrack(track).catch(() => {}); // a closed connection gets it on the next one
  }

  restart() {
    if (this.closed) return;
    if (this.restarts >= 3 || !this.signal.isOpen) {
      this.fail('Connection failed');
      return;
    }
    this.restarts++;
    this.status('Connection lost, retrying…', 'connecting');
    this.signal.send({ type: 'restart' });
  }

  onChannel(ch) {
    ch.binaryType = 'arraybuffer';
    this.channels[ch.label] = ch;
    if (ch.label === CHANNELS.CONTROL) {
      ch.onmessage = (e) => {
        if (typeof e.data !== 'string') return;
        let msg;
        try { msg = JSON.parse(e.data); } catch { return; }
        if (msg && typeof msg.type === 'string') this.dispatchEvent(new CustomEvent('control', { detail: msg }));
      };
      const opened = () => this.dispatchEvent(new Event('control-open'));
      if (ch.readyState === 'open') opened();
      else ch.onopen = opened;
    } else if (ch.label === CHANNELS.FILE) {
      ch.onmessage = (e) => {
        if (e.data instanceof ArrayBuffer) this.dispatchEvent(new CustomEvent('file-chunk', { detail: e.data }));
      };
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

  // Bytes waiting on the file channel, or null when it isn't open.
  fileBuffered() {
    const ch = this.channels[CHANNELS.FILE];
    return ch?.readyState === 'open' ? ch.bufferedAmount : null;
  }

  sendFile(buf) {
    const ch = this.channels[CHANNELS.FILE];
    if (ch?.readyState !== 'open') return false;
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

  closePeer() {
    if (this.pc) this.pc.close();
    this.pc = null;
    this.channels = {};
    if (this.video.srcObject) {
      for (const t of this.video.srcObject.getTracks()) t.stop();
      this.video.srcObject = null;
    }
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.signal.close();
    this.closePeer();
    this.dispatchEvent(new Event('closed'));
  }
}
