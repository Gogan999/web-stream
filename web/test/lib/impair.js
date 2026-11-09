// A UDP proxy between the browser and the host's media port that delays, jitters, drops and
// rate-limits packets, for testing the stream on a bad network without root or tc netem: works
// on any machine Node runs on, the same way every time (seeded). probe.js points the browser at
// it by rewriting the host's candidates.
//
//   const net = await startImpairment({ port: hostMediaPort });
//   net.set({ delayMs: 20, jitterMs: 5, loss: 0.02, rateKbps: 4000, queueMs: 200 });
//   net.set({ outage: true });   // every packet dropped, both ways
//   net.set({});                 // a clean link again
//
// Delay, jitter, loss and outage apply both ways; the rate limit only from the host to the browser
// (the stream's direction), as a bottleneck with a queue: a packet that would wait longer than
// queueMs behind it is dropped, like a router's buffer filling.
import dgram from 'node:dgram';

function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// The impairment itself, without sockets: when (or whether) a packet of `size` bytes arriving at
// `now` (ms) going `dir` ('down' to the browser, 'up' to the host) leaves.
// Returns { at } or { drop: 'loss' | 'queue' }.
export function createShaper(seed = 1) {
  const random = rng(seed);
  let profile = {};
  // Per direction: when the bottleneck is next free, and the last packet's send time (order kept).
  const lane = { down: { freeAt: 0, last: 0 }, up: { freeAt: 0, last: 0 } };
  return {
    set(p) {
      profile = { ...p };
    },
    decide(dir, size, now) {
      const p = profile;
      if (p.outage || (p.loss && random() < p.loss)) return { drop: 'loss' };
      const l = lane[dir];
      let at = now;
      if (dir === 'down' && p.rateKbps) {
        const start = Math.max(now, l.freeAt);
        if (start - now > (p.queueMs ?? 200)) return { drop: 'queue' };
        l.freeAt = start + (size * 8) / p.rateKbps;
        at = l.freeAt;
      }
      at += (p.delayMs || 0) + (p.jitterMs ? (random() * 2 - 1) * p.jitterMs : 0);
      at = Math.max(at, l.last);
      l.last = at;
      return { at };
    },
  };
}

export async function startImpairment({ port, host = '127.0.0.1', seed = 1 }) {
  const shaper = createShaper(seed);
  const stats = { down: { sent: 0, lost: 0, queueDrops: 0 }, up: { sent: 0, lost: 0, queueDrops: 0 } };
  const front = dgram.createSocket('udp4');
  await new Promise((r) => front.bind(0, '127.0.0.1', r));
  const clients = new Map(); // browser address -> socket towards the host
  const timers = new Set();

  function forward(dir, msg, send) {
    const st = stats[dir];
    const now = performance.now();
    const d = shaper.decide(dir, msg.length, now);
    if (d.drop) {
      st[d.drop === 'queue' ? 'queueDrops' : 'lost']++;
      return;
    }
    st.sent++;
    const wait = d.at - now;
    if (wait <= 0.5) return send(msg);
    const t = setTimeout(() => {
      timers.delete(t);
      send(msg);
    }, wait);
    timers.add(t);
  }

  front.on('message', (msg, from) => {
    const key = `${from.address}:${from.port}`;
    let back = clients.get(key);
    if (!back) {
      const socket = dgram.createSocket('udp4');
      back = { socket, ready: false, pending: [] };
      clients.set(key, back);
      socket.on('message', (m) => forward('down', m, (x) => front.send(x, from.port, from.address)));
      socket.on('error', () => {});
      socket.bind(0, '127.0.0.1', () => {
        back.ready = true;
        for (const m of back.pending.splice(0)) socket.send(m, port, host);
      });
    }
    const b = back;
    forward('up', msg, (x) => (b.ready ? b.socket.send(x, port, host) : b.pending.push(x)));
  });
  front.on('error', () => {});

  return {
    port: front.address().port,
    set: (p) => shaper.set(p),
    stats: () => structuredClone(stats),
    close() {
      for (const t of timers) clearTimeout(t);
      for (const c of clients.values()) c.socket.close();
      front.close();
    },
  };
}
