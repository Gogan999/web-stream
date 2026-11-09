import test from 'node:test';
import assert from 'node:assert/strict';
import { percentile, summarize } from '../lib/metrics.js';

const frame = (frame, shown, latency = 40, display = 0) => ({ frame, display, shown, latency, network: latency / 2, width: 1280, height: 720 });

test('percentile', () => {
  assert.equal(percentile([], 50), null);
  assert.equal(percentile([5, 1, 3], 50), 3);
  assert.equal(percentile([1, 2, 3, 4], 100), 4);
});

test('counts lost frames, repeats, freezes and a restart', () => {
  const frames = [
    frame(0, 1000), frame(1, 1016), frame(1, 1020), frame(4, 1066), // 2 lost, 1 repeat
    frame(5, 1400), // a 334 ms freeze
    frame(0, 1416, 40, 1), frame(1, 1433, 40, 1), // a new encoder: no loss
    { bad: true, shown: 1440 },
  ];
  const s = summarize({ frames });
  assert.equal(s.frames, 6);
  assert.equal(s.lost, 2);
  assert.equal(s.repeats, 1);
  assert.equal(s.unreadable, 1);
  assert.equal(s.freezes, 1);
  assert.equal(s.longestFreezeMs, 334);
  assert.deepEqual(s.displays, [0, 1]);
});

test('audio latency from beeps at whole seconds, and A/V offset', () => {
  const frames = [frame(0, 5000, 50), frame(1, 5016, 50)];
  const beeps = [1_700_000_000_120, 1_700_000_001_130, 1_700_000_002_125];
  const s = summarize({ frames, beeps });
  assert.equal(s.beeps, 3);
  assert.equal(s.audioLatencyMs.p50, 125);
  assert.equal(s.avOffsetMs, 75);
});

test('window', () => {
  const frames = [frame(0, 100), frame(1, 200), frame(2, 300)];
  assert.equal(summarize({ frames }, 150, 250).frames, 1);
});

test('frames the probe missed but the browser presented are not lost', () => {
  const f = (frame, shown, presented) => ({ ...frame_(frame, shown), presented });
  const frame_ = (frame, shown) => ({ frame, display: 0, shown, latency: 40, network: 20, width: 1280, height: 720 });
  // Frame 2 was presented (counter 7 -> 9 across two) but never seen; frame 4 never was.
  const s = summarize({ frames: [f(0, 0, 5), f(1, 16, 6), f(3, 50, 8), f(5, 83, 9)] });
  assert.equal(s.lost, 1);
});
