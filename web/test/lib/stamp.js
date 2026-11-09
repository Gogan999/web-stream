// The stamp the test host's synthetic source draws into the top of every frame
// (crates/capture/src/synthetic/stamp.rs): two rows of 32 square cells, each
// floor(width / 32) pixels, black = 0 and white = 1, most significant bit first.
//   row 0: capture time, Unix ms, low 32 bits
//   row 1: display (4 bits), frame index (20 bits), CRC-8 (poly 0x07) of the 7 bytes before it
// A plain script (no exports) so a page can load it with addInitScript; Node imports it
// for its side effect and uses globalThis.wsStamp.
(() => {
  const COLS = 32;

  function crc8(bytes) {
    let crc = 0;
    for (const b of bytes) {
      crc ^= b;
      for (let i = 0; i < 8; i++) crc = crc & 0x80 ? ((crc << 1) ^ 0x07) & 0xff : (crc << 1) & 0xff;
    }
    return crc;
  }

  const be = (v, n) => Array.from({ length: n }, (_, i) => (v >>> (8 * (n - 1 - i))) & 0xff);

  // The two rows' 32-bit words for a stamp, as the host draws them.
  function words({ time, display, frame }) {
    const t = time >>> 0;
    const id = (((display & 0xf) << 20) | (frame & 0xfffff)) >>> 0;
    const crc = crc8([...be(t, 4), ...be(id, 3)]);
    return [t, ((id << 8) | crc) >>> 0];
  }

  // Decodes 2 x 32 bits, each a 0..255 brightness, row by row. Returns
  // { time, display, frame } or null when the CRC fails (a torn or unstamped frame).
  function decode(levels) {
    const w = [0, 0];
    for (let r = 0; r < 2; r++) {
      let v = 0;
      for (let c = 0; c < COLS; c++) v = ((v << 1) | (levels[r * COLS + c] >= 128 ? 1 : 0)) >>> 0;
      w[r] = v;
    }
    const id = w[1] >>> 8;
    if (crc8([...be(w[0], 4), ...be(id, 3)]) !== (w[1] & 0xff)) return null;
    return { time: w[0], display: id >>> 20, frame: id & 0xfffff };
  }

  // Reads the stamp from RGBA pixels of a 32 x 2 image, one pixel per cell.
  function decodeRgba(data) {
    const levels = new Array(2 * COLS);
    for (let i = 0; i < 2 * COLS; i++) levels[i] = (data[i * 4] + data[i * 4 + 1] + data[i * 4 + 2]) / 3;
    return decode(levels);
  }

  // Milliseconds from a stamp's 32-bit time to `nowMs` (Unix ms), across the wrap.
  function age(time, nowMs) {
    const d = (Math.floor(nowMs) - time) >>> 0;
    return d > 0x7fffffff ? d - 0x100000000 : d;
  }

  globalThis.wsStamp = { COLS, crc8, words, decode, decodeRgba, age };
})();
