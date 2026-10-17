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

const ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/signal?role=host`);
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
      send({ type: 'iceServers', servers: [] });
      break;
    case 'offer':
      await answer(msg.sdp);
      break;
    case 'iceCandidate':
      if (pc) await pc.addIceCandidate({ candidate: msg.candidate, sdpMid: msg.sdpMid, sdpMLineIndex: msg.sdpMLineIndex }).catch(() => {});
      break;
    case 'dev-client-disconnected':
      pc?.close();
      pc = null;
      break;
  }
};

async function answer(sdp) {
  if (pc && pc.signalingState === 'stable' && pc.connectionState !== 'closed') {
    // ICE restart: same peer connection, new offer.
    write({ ev: 'reoffer' });
  } else {
    pc?.close();
    pc = new RTCPeerConnection();
    pc.onicecandidate = (e) => e.candidate && send({ type: 'iceCandidate', candidate: e.candidate.candidate, sdpMid: e.candidate.sdpMid, sdpMLineIndex: e.candidate.sdpMLineIndex });
    pc.onconnectionstatechange = () => write({ ev: 'pc', state: pc.connectionState });
    pc.ondatachannel = ({ channel }) => onChannel(channel);
  }
  await pc.setRemoteDescription({ type: 'offer', sdp });
  for (const t of pc.getTransceivers()) {
    const kind = t.receiver.track.kind;
    write({ ev: 'transceiver', kind, offered: t.direction });
    if (t.sender.track) continue;
    await t.sender.replaceTrack(kind === 'video' ? videoTrack : audioTrack);
    t.direction = kind === 'audio' ? 'sendrecv' : 'sendonly';
  }
  // Keep the "display" resolution fixed, as the real host's encoder does, so the client sees it change on a switch.
  for (const s of pc.getSenders()) {
    if (s.track?.kind !== 'video') continue;
    const p = s.getParameters();
    p.degradationPreference = 'maintain-resolution';
    await s.setParameters(p).catch(() => {});
  }
  await pc.setLocalDescription(await pc.createAnswer());
  send({ type: 'answer', sdp: pc.localDescription.sdp });
}

function onChannel(channel) {
  channel.binaryType = 'arraybuffer';
  write({ ev: 'channel', label: channel.label, ordered: channel.ordered, maxRetransmits: channel.maxRetransmits });
  channel.onmessage = (m) => {
    if (channel.label === 'control') {
      const c = JSON.parse(m.data);
      write({ ev: 'control', ...c });
      if (c.type === 'listDisplays') sendDisplays(channel);
      if (c.type === 'selectDisplay') {
        setDisplay(c.id);
        const { width, height } = active;
        channel.send(JSON.stringify({ type: 'displayChanged', id: active.id, config: { codec: 'h264', width, height, fps: 60, bitrateKbps: 20000 } }));
      }
    } else {
      write({ ev: 'input', ch: channel.label, ...decode(m.data) });
    }
  };
}

function sendDisplays(channel) {
  channel.send(JSON.stringify({
    type: 'displays',
    displays: DISPLAYS.map(({ color, ...d }) => d),
    active: active.id,
  }));
}
