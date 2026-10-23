import test from 'node:test';
import assert from 'node:assert/strict';
import { snapshot, compute, format, formatHost, assess, render, Alerts } from '../../js/stats.js';

const report = (video, rtt = 0.012) => new Map([
  ['v', { type: 'inbound-rtp', kind: 'video', codecId: 'c', ...video }],
  ['c', { type: 'codec', mimeType: 'video/H264' }],
  ['p', { type: 'candidate-pair', nominated: true, state: 'succeeded', currentRoundTripTime: rtt, localCandidateId: 'l', remoteCandidateId: 'r' }],
  ['l', { type: 'local-candidate', candidateType: 'host' }],
  ['r', { type: 'remote-candidate', candidateType: 'relay' }],
]);

test('compute deltas over the interval', () => {
  const a = snapshot(report({ framesDecoded: 0, bytesReceived: 0, totalDecodeTime: 0, jitterBufferDelay: 0, jitterBufferEmittedCount: 0, framesDropped: 1, packetsLost: 0 }), 0);
  const b = snapshot(report({ framesDecoded: 30, bytesReceived: 625000, totalDecodeTime: 0.06, jitterBufferDelay: 0.15, jitterBufferEmittedCount: 30, framesDropped: 3, packetsLost: 2, frameWidth: 1920, frameHeight: 1080 }), 500);
  const s = compute(a, b);
  assert.equal(s.fps, 60);
  assert.equal(s.bitrateMbps, 10);
  assert.ok(Math.abs(s.decodeMs - 2) < 1e-9);
  assert.ok(Math.abs(s.jitterMs - 5) < 1e-9);
  assert.equal(s.droppedDelta, 2);
  assert.equal(s.lostDelta, 2);
  assert.equal(s.rttMs, 12);
  assert.equal(s.relayed, true);
  assert.equal(s.codec, 'H264');
  assert.match(format(s, 8), /latency est 14\.0 ms/);
});

test('resends and keyframe requests are counted per interval', () => {
  const a = snapshot(report({ nackCount: 4, retransmittedPacketsReceived: 10, pliCount: 1, firCount: 0 }), 0);
  const b = snapshot(report({ nackCount: 9, retransmittedPacketsReceived: 16, pliCount: 2, firCount: 1 }), 1000);
  const s = compute(a, b);
  assert.equal(s.nackDelta, 5);
  assert.equal(s.rtxDelta, 6);
  assert.equal(s.pliDelta, 2);
  assert.match(format(s, null), /resends asked \+5 got \+6  keyframes asked \+2/);
});

test('host line names why keyframes happened', () => {
  const line = formatHost({ keyframeRequests: 3, browserKeyframeRequests: 1, lateFrames: 2, lostFrames: 0, recoveredFrames: 2, sendDrops: 7 });
  assert.match(line, /host keyframes: browser 1  late 2  dropped 0  repaired 2  send drops 7/);
  assert.match(formatHost({ keyframeRequests: 1 }), /browser –/);
});

test('first sample has no rates', () => {
  const s = compute(null, snapshot(report({ framesDecoded: 5 }), 0));
  assert.equal(s.fps, null);
  assert.match(format(s, null), /fps –/);
});


const healthy = { width: 1920, height: 1080, fps: 60, bitrateMbps: 18, rttMs: 4, relayed: false, tcp: false, lossPct: 0, droppedDelta: 0, freezesDelta: 0, pliDelta: 0, decodeMs: 2, totals: { packets: 0, frames: 0, keyframes: 0, nacks: 0, plis: 0, freezeSec: 0 } };
const config = { codec: 'h264', width: 1920, height: 1080, fps: 60, bitrateKbps: 20000, quality: 'balanced' };
const hostOk = { encoderKbps: 20000, maxKbps: 20000, bweKbps: 40000, fps: 60, sentKbps: 19000, encodeMs: 3, keyframeRequests: 0, loss: 0, lateFrames: 0, lostFrames: 0, sendDrops: 0 };

test('a healthy stream has no warnings and essential says OK', () => {
  assert.deepEqual(assess(healthy, hostOk, config), []);
  const lines = render('essential', healthy, 5, hostOk, config, []);
  assert.deepEqual(lines[0], { text: 'OK', sev: 'ok' });
  assert.match(lines.map((l) => l.text).join('\n'), /1920x1080  fps 60 \/ 60  bitrate 18\.0 Mb\/s\nrtt 4 ms  direct UDP/);
  assert.ok(!lines.some((l) => /host enc/.test(l.text)), 'essential leaves out host detail');
});

test('a lower bitrate target is no warning while the encoder sends well under it', () => {
  const quiet = { ...hostOk, encoderKbps: 2000, bweKbps: 2600, sentKbps: 900 };
  assert.ok(!assess(healthy, quiet, config).some((a) => a.key === 'bitrate'));
  assert.ok(assess(healthy, { ...quiet, sentKbps: 1900 }, config).some((a) => a.key === 'bitrate'));
});

test('essential warnings name reduced resolution, bitrate, slow and lossy links, TCP and drops', () => {
  const s = { ...healthy, width: 1280, height: 720, rttMs: 170, lossPct: 2.5, tcp: true, droppedDelta: 3 };
  const h = { ...hostOk, encoderKbps: 4000, bweKbps: 4500, sentKbps: 3900, lateFrames: 2 };
  const got = assess(s, h, config);
  const text = got.map((a) => a.text).join('\n');
  assert.match(text, /Reduced resolution 1280x720 \(asked 1920x1080\)/);
  assert.match(text, /Reduced bitrate 4\.0 of 20\.0 Mb\/s, network carries ~4\.5/);
  assert.match(text, /Slow connection: round trip 170 ms/);
  assert.match(text, /Lossy connection: 2\.5% packets lost/);
  assert.match(text, /TCP fallback in use/);
  assert.match(text, /Browser dropped 3 frames/);
  assert.match(text, /Host dropped 2 frames/);
  // Worst first.
  assert.equal(got[0].sev, 'bad');
  const lines = render('essential', s, 5, h, config, got);
  assert.equal(lines[0].text[0], '!');
  assert.match(lines.at(-1).text, /TCP fallback/);
});

test('a still screen with few frames is not a warning', () => {
  assert.deepEqual(assess({ ...healthy, fps: 2 }, { ...hostOk, fps: 2 }, config), []);
  assert.match(assess({ ...healthy, fps: 30 }, hostOk, config)[0].text, /Showing 30 of 60 fps/);
});

test('detailed adds browser and host figures, debug adds totals and raw host stats', () => {
  const detailed = render('detailed', healthy, 5, hostOk, config, []).map((l) => l.text).join('\n');
  assert.match(detailed, /host enc 20\.0\/20\.0 Mb\/s/);
  assert.match(detailed, /resends asked/);
  assert.doesNotMatch(detailed, /totals/);
  const debug = render('debug', { ...healthy, route: 'host/udp -> host/udp' }, 5, hostOk, config, []).map((l) => l.text).join('\n');
  assert.match(debug, /totals packets 0/);
  assert.match(debug, /route host\/udp -> host\/udp/);
  assert.match(debug, /asked 1920x1080@60 h264 20\.0 Mb\/s/);
  assert.match(debug, /host raw \{"encoderKbps":20000/);
});

test('alerts stay up for a while after the last time they were seen', () => {
  const a = new Alerts(1000);
  const w = { key: 'dropped', sev: 'warn', text: 'x' };
  assert.equal(a.update([w], 0).length, 1);
  assert.equal(a.update([], 900).length, 1);
  assert.equal(a.update([], 1100).length, 0);
});

test('snapshot spots ICE-TCP and loss is a share of packets', () => {
  const r = report({ packetsReceived: 0, packetsLost: 0 });
  r.set('l', { type: 'local-candidate', candidateType: 'host', protocol: 'tcp' });
  const a = snapshot(r, 0);
  const r2 = report({ packetsReceived: 98, packetsLost: 2 });
  r2.set('l', { type: 'local-candidate', candidateType: 'host', protocol: 'tcp' });
  const s = compute(a, snapshot(r2, 500));
  assert.equal(s.tcp, true);
  assert.equal(s.lossPct, 2);
});
