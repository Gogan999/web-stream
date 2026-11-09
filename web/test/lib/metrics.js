// Turns what probe.js collected in the page into figures a test can assert on.

// The p-th percentile (0..100) of `values`, or null for none.
export function percentile(values, p) {
  if (!values.length) return null;
  const s = [...values].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.max(0, Math.ceil((p / 100) * s.length) - 1))];
}

// A presentation gap at least this long counts as a freeze.
export const FREEZE_MS = 200;

// Figures for the frames presented, and beeps heard, between `from` and `to` (Unix ms).
export function summarize({ frames, beeps = [] }, from = -Infinity, to = Infinity) {
  const inWindow = frames.filter((f) => f.shown >= from && f.shown < to);
  const good = inWindow.filter((f) => !f.bad);
  // Frames the encoder made that were never shown: gaps in the frame index. It restarts at 0 with
  // every new encoder (codec or size change), which is not a loss.
  let lost = 0;
  let repeats = 0;
  const unique = [];
  for (const f of good) {
    const prev = unique.at(-1);
    if (prev && prev.frame === f.frame && prev.display === f.display) {
      repeats++;
      continue;
    }
    if (prev && f.frame > prev.frame) {
      // Frames the browser presented but this probe's callback never saw (the page was busy) are
      // not lost: the browser's own presented-frame counter says how many that was.
      const unseen = f.presented != null && prev.presented != null ? Math.max(0, f.presented - prev.presented - 1) : 0;
      lost += Math.max(0, f.frame - prev.frame - 1 - unseen);
    }
    unique.push(f);
  }
  const freezes = [];
  for (let i = 1; i < unique.length; i++) {
    const gap = unique[i].shown - unique[i - 1].shown;
    if (gap >= FREEZE_MS) freezes.push(Math.round(gap));
  }
  const span = unique.length > 1 ? unique.at(-1).shown - unique[0].shown : 0;
  const latency = good.map((f) => f.latency);
  const network = good.map((f) => f.network).filter((v) => v != null);
  const heard = beeps.filter((b) => b >= from && b < to);
  // A beep starts at a whole Unix second: how far past it the beep was heard is its latency.
  const audioLatency = heard.map((b) => ((b % 1000) + 1000) % 1000);
  const round = (v) => (v == null ? null : Math.round(v * 10) / 10);
  const videoP50 = percentile(latency, 50);
  const audioP50 = percentile(audioLatency, 50);
  return {
    frames: unique.length,
    fps: round(span > 0 ? ((unique.length - 1) * 1000) / span : 0),
    lost,
    lostPct: round(unique.length + lost ? (100 * lost) / (unique.length + lost) : 0),
    repeats,
    unreadable: inWindow.length - good.length,
    latencyMs: { p50: round(videoP50), p95: round(percentile(latency, 95)), max: round(percentile(latency, 100)) },
    networkMs: { p50: round(percentile(network, 50)), p95: round(percentile(network, 95)) },
    freezes: freezes.length,
    longestFreezeMs: freezes.length ? Math.max(...freezes) : 0,
    displays: [...new Set(unique.map((f) => f.display))],
    size: unique.length ? `${unique.at(-1).width}x${unique.at(-1).height}` : null,
    beeps: heard.length,
    audioLatencyMs: { p50: round(audioP50), p95: round(percentile(audioLatency, 95)) },
    // Positive: sound comes after the picture.
    avOffsetMs: videoP50 != null && audioP50 != null ? round(audioP50 - videoP50) : null,
  };
}
