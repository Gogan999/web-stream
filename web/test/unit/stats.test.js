import test from 'node:test';
import assert from 'node:assert/strict';
import { snapshot, compute, format } from '../../js/stats.js';

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

test('first sample has no rates', () => {
  const s = compute(null, snapshot(report({ framesDecoded: 5 }), 0));
  assert.equal(s.fps, null);
  assert.match(format(s, null), /fps –/);
});
