// Stand-in for the real host agent so the client can be developed and tested without it.
// Streams an animated canvas plus a tone, answers the client's offer, sends a display list,
// switches "displays" on request and logs decoded input.

import { decode } from '../js/protocol.js';

const DISPLAYS = [
  { id: 0, name: 'Mock 720p', width: 1280, height: 720, refresh: 60, primary: true, color: '#246' },
  { id: 1, name: 'Mock 1080p', width: 1920, height: 1080, refresh: 60, primary: false, color: '#642' },
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
    case 'client-connected':
      send({ type: 'welcome', iceServers: [] });
      break;
    case 'hello':
      write({ ev: 'hello', token: msg.token });
      break;
    case 'offer':
      await answer(msg.sdp);
      break;
    case 'ice':
      if (pc) await pc.addIceCandidate(msg.candidate ?? null);
      break;
    case 'client-disconnected':
      pc?.close();
      pc = null;
      break;
  }
};

async function answer(sdp) {
  pc?.close();
  pc = new RTCPeerConnection();
  pc.onicecandidate = (e) => send({ type: 'ice', candidate: e.candidate ? e.candidate.toJSON() : null });
  pc.onconnectionstatechange = () => write({ ev: 'pc', state: pc.connectionState });
  pc.ondatachannel = ({ channel }) => {
    channel.binaryType = 'arraybuffer';
    write({ ev: 'channel', label: channel.label, ordered: channel.ordered, maxRetransmits: channel.maxRetransmits });
    channel.onmessage = (m) => {
      if (channel.label === 'control') {
        const c = JSON.parse(m.data);
        write({ ev: 'control', ...c });
        if (c.type === 'select-display') {
          setDisplay(c.id);
          sendDisplays(channel);
        }
      } else {
        write({ ev: 'input', ch: channel.label, ...decode(m.data) });
      }
    };
    if (channel.label === 'control') channel.onopen = () => sendDisplays(channel);
  };
  await pc.setRemoteDescription({ type: 'offer', sdp });
  for (const t of pc.getTransceivers()) {
    const kind = t.receiver.track.kind;
    await t.sender.replaceTrack(kind === 'video' ? videoTrack : audioTrack);
    t.direction = 'sendonly';
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

function sendDisplays(channel) {
  channel.send(JSON.stringify({
    type: 'displays',
    displays: DISPLAYS.map(({ color, ...d }) => d),
    active: active.id,
  }));
}
