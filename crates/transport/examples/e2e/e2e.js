// Minimal client for the test_pattern example, driven by run.mjs.
const log = (m) => { document.getElementById('log').textContent += m + '\n'; };
const json = { 'Content-Type': 'application/json' };
const state = { channels: {}, echo: [], pc: null, ws: null, iceTransportPolicy: 'all' };
window.e2e = state;

window.startPair = async () => {
  const r = await fetch('/api/pair/start', { method: 'POST', headers: json, body: JSON.stringify({ device_name: 'e2e' }) });
  return (await r.json()).pairing_id;
};

window.confirmPair = async (pairing_id, code) => {
  const r = await fetch('/api/pair/confirm', { method: 'POST', headers: json, body: JSON.stringify({ pairing_id, code }) });
  return r.status;
};

// `tcpOnly` drops every UDP candidate from the offer to force ICE-TCP.
window.connect = (tcpOnly = false) => new Promise((resolve, reject) => {
  const ws = new WebSocket(`wss://${location.host}/ws`);
  state.ws = ws;
  ws.onerror = () => reject(new Error('ws error'));
  ws.onmessage = async (e) => {
    const msg = JSON.parse(e.data);
    log('<- ' + msg.type);
    if (msg.type === 'welcome') {
      const pc = new RTCPeerConnection({ iceServers: msg.ice_servers });
      state.pc = pc;
      pc.ontrack = (ev) => { if (ev.track.kind === 'video') document.getElementById('v').srcObject = new MediaStream([ev.track]); };
      pc.ondatachannel = (ev) => {
        const ch = ev.channel;
        ch.binaryType = 'arraybuffer';
        state.channels[ch.label] = ch;
        ch.onmessage = (m) => state.echo.push({ label: ch.label, data: typeof m.data === 'string' ? m.data : new TextDecoder().decode(m.data) });
      };
      pc.onicecandidate = (ev) => {
        if (ev.candidate) ws.send(JSON.stringify({ type: 'candidate', candidate: ev.candidate.candidate, sdpMid: ev.candidate.sdpMid, sdpMLineIndex: ev.candidate.sdpMLineIndex }));
      };
      resolve();
    } else if (msg.type === 'offer') {
      let sdp = msg.sdp;
      if (tcpOnly) sdp = sdp.split('\r\n').filter((l) => !(l.startsWith('a=candidate') && / udp /i.test(l))).join('\r\n');
      await state.pc.setRemoteDescription({ type: 'offer', sdp });
      for (const r of state.pc.getReceivers()) r.jitterBufferTarget = 0;
      await state.pc.setLocalDescription(await state.pc.createAnswer());
      ws.send(JSON.stringify({ type: 'answer', sdp: state.pc.localDescription.sdp }));
    } else if (msg.type === 'error' || msg.type === 'bye') {
      log(JSON.stringify(msg));
    }
  };
});

window.stats = async () => {
  const out = { offerExtPlayoutDelay: /playout-delay/.test(state.pc.remoteDescription?.sdp || '') };
  const report = await state.pc.getStats();
  let pairId;
  report.forEach((s) => {
    if (s.type === 'inbound-rtp' && s.kind === 'video') Object.assign(out, { framesDecoded: s.framesDecoded, frameWidth: s.frameWidth, codecId: s.codecId, packetsLost: s.packetsLost, jitterBufferDelay: s.jitterBufferDelay, jitterBufferEmittedCount: s.jitterBufferEmittedCount });
    if (s.type === 'transport') pairId = s.selectedCandidatePairId;
  });
  if (out.codecId) out.codec = report.get(out.codecId)?.mimeType;
  const pair = pairId && report.get(pairId);
  if (pair) out.localProtocol = report.get(pair.localCandidateId)?.protocol;
  out.channels = Object.fromEntries(Object.entries(state.channels).map(([k, c]) => [k, { ordered: c.ordered, maxRetransmits: c.maxRetransmits, state: c.readyState }]));
  out.echo = state.echo;
  return out;
};
