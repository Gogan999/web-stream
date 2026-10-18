// Stand-in for the real host agent so the client can be developed and tested without it.
// Streams an animated canvas plus a tone, answers the client's offer, sends a display list,
// switches "displays" on request and logs decoded input.

import { decode } from '../js/protocol.js';

const DISPLAYS = [
  { id: 0, name: 'Mock 720p', x: 0, y: 0, width: 1280, height: 720, refreshMhz: 60000, primary: true, color: '#246' },
  { id: 1, name: 'Mock 1080p', x: 1280, y: 0, width: 1920, height: 1080, refreshMhz: 59940, primary: false, color: '#642' },
];

const canvas = document.getElementById('screen');
const ctx = canvas.getContext('2d');
const logEl = document.getElementById('log');
const log = (window.mockLog = []);
let active = DISPLAYS[0];
let frame = 0;

function write(entry) {
  log.push(entry);
  logEl.textContent = (JSON.stringify(entry) + '\n' + logEl.textContent).slice(0, 20000);
}

function draw() {
  frame++;
  ctx.fillStyle = active.color;
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = '#fff';
  ctx.font = '48px monospace';
  ctx.fillText(`${active.name}  frame ${frame}`, 40, 80);
  ctx.fillText(new Date().toISOString().slice(11, 23), 40, 150);
  const x = (frame * 8) % canvas.width;
  ctx.fillRect(x, canvas.height - 100, 60, 60);
  requestAnimationFrame(draw);
}
requestAnimationFrame(draw);

function setDisplay(id) {
  const d = DISPLAYS.find((x) => x.id === id);
  if (!d) return;
  active = d;
  canvas.width = d.width;
  canvas.height = d.height;
}

const videoTrack = canvas.captureStream(60).getVideoTracks()[0];
const audioCtx = new AudioContext();
const osc = audioCtx.createOscillator();
const gain = audioCtx.createGain();
gain.gain.value = 0.02;
const dest = audioCtx.createMediaStreamDestination();
osc.connect(gain).connect(dest);
osc.start();
const audioTrack = dest.stream.getAudioTracks()[0];

const ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?role=host`);
let pc = null;
const send = (m) => ws.send(JSON.stringify(m));
ws.onopen = () => write({ ev: 'signaling-open' });

ws.onmessage = async (e) => {
  const msg = JSON.parse(e.data);
  switch (msg.type) {
    case 'dev-pairing-code':
      window.mockPairingCode = msg.code;
      write({ ev: 'pairing-code', code: msg.code });
      break;
    case 'dev-client-connected':
      write({ ev: 'client-connected' });
      send({ type: 'welcome', ice_servers: [] });
      await offer();
      break;
    case 'restart':
      write({ ev: 'restart' });
      await offer();
      break;
    case 'answer':
      write({ ev: 'answer' });
      await pc.setRemoteDescription({ type: 'answer', sdp: msg.sdp });
      break;
    case 'candidate':
      if (pc) await pc.addIceCandidate({ candidate: msg.candidate, sdpMid: msg.sdpMid, sdpMLineIndex: msg.sdpMLineIndex }).catch(() => {});
      break;
    case 'dev-client-disconnected':
      pc?.close();
      pc = null;
      break;
  }
};

// Mirrors the real host: host offers video sendonly + audio sendrecv and opens the channels.
async function offer() {
  pc?.close();
  pc = new RTCPeerConnection();
  pc.onconnectionstatechange = () => write({ ev: 'pc', state: pc.connectionState });
  pc.addTransceiver(videoTrack, { direction: 'sendonly' });
  pc.addTransceiver(audioTrack, { direction: 'sendrecv' });
  window.__hostPc = pc;
  // Keep the "display" resolution fixed, as the real host's encoder does, so the client sees it change on a switch.
  for (const s of pc.getSenders()) {
    if (s.track?.kind !== 'video') continue;
    const p = s.getParameters();
    p.degradationPreference = 'maintain-resolution';
    await s.setParameters(p).catch(() => {});
  }
  const channels = [
    pc.createDataChannel('control'),
    pc.createDataChannel('input'),
    pc.createDataChannel('input-motion', { ordered: false, maxRetransmits: 0 }),
    pc.createDataChannel('file'),
  ];
  for (const channel of channels) {
    channel.binaryType = 'arraybuffer';
    if (channel.label === 'control') window.__control = channel;
    channel.onopen = () => write({ ev: 'channel', label: channel.label, ordered: channel.ordered, maxRetransmits: channel.maxRetransmits });
    channel.onmessage = (m) => {
      if (channel.label === 'control') {
        const c = JSON.parse(m.data);
        write({ ev: 'control', ...c });
        // Like the real host: the stream keeps its configured size whatever the display (the canvas
        // resizes here only so the client sees a switch), and settings come back in displayChanged.
        if (c.type === 'listDisplays') {
          sendDisplays(channel);
          sendConfig(channel);
        }
        if (c.type === 'selectDisplay') {
          setDisplay(c.id);
          sendConfig(channel);
        }
        if (c.type === 'setVideoConfig') {
          config = { ...config, ...c.config };
          sendConfig(channel);
        }
        if (c.type === 'setHostResolution') {
          channel.send(JSON.stringify({ type: 'error', message: "the host doesn't allow changing its resolution; start it with --allow-resolution-change" }));
        }
      } else {
        write({ ev: 'input', ch: channel.label, ...decode(m.data) });
      }
    };
  }
  await pc.setLocalDescription(await pc.createOffer());
  // Like the real host: all candidates go in the offer, no trickle. Host candidates come
  // within milliseconds; don't wait for slow interfaces to finish.
  await new Promise((r) => {
    pc.addEventListener('icegatheringstatechange', () => pc.iceGatheringState === 'complete' && r());
    setTimeout(r, 500);
  });
  send({ type: 'offer', sdp: pc.localDescription.sdp });
}

// Mic audio packets received from the client so far.
window.mockMicPackets = async () => {
  let n = 0;
  for (const s of (await window.__hostPc?.getStats())?.values() ?? []) {
    if (s.type === 'inbound-rtp' && s.kind === 'audio') n += s.packetsReceived;
  }
  return n;
};

let config = { codec: 'h264', width: 1920, height: 1080, fps: 60, bitrateKbps: 20000, quality: 'balanced' };

function sendConfig(channel) {
  channel.send(JSON.stringify({ type: 'displayChanged', id: active.id, config }));
}

// A rough videoStats once a second, so the client's host line and auto-tune have something to read.
setInterval(() => {
  const ch = window.__control;
  if (ch?.readyState !== 'open') return;
  ch.send(JSON.stringify({
    type: 'videoStats', encoderKbps: config.bitrateKbps, maxKbps: config.bitrateKbps, bweKbps: config.bitrateKbps,
    fps: 60, sentKbps: config.bitrateKbps, encodeMs: 2, keyframeRequests: 0, loss: 0,
  }));
}, 1000);

function sendDisplays(channel) {
  channel.send(JSON.stringify({
    type: 'displays',
    displays: DISPLAYS.map(({ color, ...d }) => d),
    active: active.id,
  }));
}
