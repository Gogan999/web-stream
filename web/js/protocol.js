// Binary input messages sent on the "input" and "input-motion" data channels.
// Little-endian, first byte is the message type. See docs/dev/protocol.md.

export const MSG = Object.freeze({
  MOUSE_REL: 0x01,
  MOUSE_ABS: 0x02,
  MOUSE_BUTTON: 0x03,
  WHEEL: 0x04,
  KEY: 0x05,
  GAMEPAD: 0x06,
  GAMEPAD_DISCONNECT: 0x07,
  RELEASE_ALL: 0x08,
  GAMEPAD_BUTTONS: 0x09,
  MOUSE_ABS_ON: 0x0a, // MOUSE_ABS for a display other than the main one
  TEXT: 0x0b, // Unicode text to type on the host, UTF-8 up to TEXT_MAX bytes
});

// Most UTF-8 bytes the host takes in one text message.
export const TEXT_MAX = 4096;

const clampI16 = (v) => Math.max(-32768, Math.min(32767, Math.round(v)));
const clampU16 = (v) => Math.max(0, Math.min(65535, Math.round(v)));
const clampU8 = (v) => Math.max(0, Math.min(255, Math.round(v)));

export function mouseRel(dx, dy) {
  const b = new ArrayBuffer(5);
  const v = new DataView(b);
  v.setUint8(0, MSG.MOUSE_REL);
  v.setInt16(1, clampI16(dx), true);
  v.setInt16(3, clampI16(dy), true);
  return b;
}

// x, y in 0..1 over the active display. `seq` (optional, u16) numbers positions sent on the
// unordered channel so the host can ignore one overtaken by a newer one.
// `lane` says which video stream's display the point is over: 0 the main one, 1 the second monitor
// (which takes the longer MOUSE_ABS_ON form, with the lane after the type).
export function mouseAbs(x, y, seq, lane = 0) {
  const head = lane ? 2 : 1;
  const b = new ArrayBuffer(head + 4 + (seq === undefined ? 0 : 2));
  const v = new DataView(b);
  v.setUint8(0, lane ? MSG.MOUSE_ABS_ON : MSG.MOUSE_ABS);
  if (lane) v.setUint8(1, lane);
  v.setUint16(head, clampU16(x * 65535), true);
  v.setUint16(head + 2, clampU16(y * 65535), true);
  if (seq !== undefined) v.setUint16(head + 4, seq & 0xffff, true);
  return b;
}

// DOM MouseEvent.button: 0 left, 1 middle, 2 right, 3 back, 4 forward (same numbering on the wire).
export function mouseButton(button, down) {
  return new Uint8Array([MSG.MOUSE_BUTTON, button, down ? 1 : 0]).buffer;
}

// dx, dy in Windows WHEEL_DELTA units (120 per notch).
export function wheel(dx, dy) {
  const b = new ArrayBuffer(5);
  const v = new DataView(b);
  v.setUint8(0, MSG.WHEEL);
  v.setInt16(1, clampI16(dx), true);
  v.setInt16(3, clampI16(dy), true);
  return b;
}

export function key(hidUsage, down) {
  const b = new ArrayBuffer(4);
  const v = new DataView(b);
  v.setUint8(0, MSG.KEY);
  v.setUint16(1, hidUsage, true);
  v.setUint8(3, down ? 1 : 0);
  return b;
}

// state: { buttons (XInput bit flags), lx, ly, rx, ry (i16, +y up), lt, rt (u8) }
// `seq` (u16, per pad, shared with gamepadButtons) lets the host drop an overtaken snapshot.
export function gamepad(index, s, seq) {
  return padState(MSG.GAMEPAD, index, s, seq);
}

// The same snapshot for the reliable channel, sent when the buttons change: its buttons
// always apply on the host, so a press or release is never lost.
export function gamepadButtons(index, s, seq) {
  return padState(MSG.GAMEPAD_BUTTONS, index, s, seq);
}

function padState(type, index, s, seq) {
  const b = new ArrayBuffer(seq === undefined ? 14 : 16);
  const v = new DataView(b);
  v.setUint8(0, type);
  v.setUint8(1, index);
  v.setUint16(2, s.buttons & 0xffff, true);
  v.setInt16(4, clampI16(s.lx), true);
  v.setInt16(6, clampI16(s.ly), true);
  v.setInt16(8, clampI16(s.rx), true);
  v.setInt16(10, clampI16(s.ry), true);
  v.setUint8(12, clampU8(s.lt));
  v.setUint8(13, clampU8(s.rt));
  if (seq !== undefined) v.setUint16(14, seq & 0xffff, true);
  return b;
}

export function gamepadDisconnect(index) {
  return new Uint8Array([MSG.GAMEPAD_DISCONNECT, index]).buffer;
}

export function releaseAll() {
  return new Uint8Array([MSG.RELEASE_ALL]).buffer;
}

// Messages that type `text` on the host as Unicode characters, whatever its keyboard layout. The
// host takes at most TEXT_MAX bytes in one, so longer text is split between characters. Returns
// an array of buffers, in order (empty for empty text).
export function text(str) {
  const enc = new TextEncoder();
  const out = [];
  let chunk = '';
  let bytes = 0;
  const flush = () => {
    if (!chunk) return;
    const body = enc.encode(chunk);
    const b = new Uint8Array(1 + body.length);
    b[0] = MSG.TEXT;
    b.set(body, 1);
    out.push(b.buffer);
    chunk = '';
    bytes = 0;
  };
  for (const ch of String(str)) { // by code point, so a surrogate pair is never split
    const n = enc.encode(ch).length;
    if (bytes + n > TEXT_MAX) flush();
    chunk += ch;
    bytes += n;
  }
  flush();
  return out;
}

// Decoder, used by tests and the dev mock host.
export function decode(buf) {
  const v = new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer, buf.byteOffset ?? 0, buf.byteLength);
  const t = v.getUint8(0);
  switch (t) {
    case MSG.MOUSE_REL:
      return { type: 'mouse-rel', dx: v.getInt16(1, true), dy: v.getInt16(3, true) };
    case MSG.MOUSE_ABS:
      return {
        type: 'mouse-abs', x: v.getUint16(1, true), y: v.getUint16(3, true),
        ...(v.byteLength >= 7 ? { seq: v.getUint16(5, true) } : {}),
      };
    case MSG.MOUSE_ABS_ON:
      return {
        type: 'mouse-abs', lane: v.getUint8(1), x: v.getUint16(2, true), y: v.getUint16(4, true),
        ...(v.byteLength >= 8 ? { seq: v.getUint16(6, true) } : {}),
      };
    case MSG.MOUSE_BUTTON:
      return { type: 'mouse-button', button: v.getUint8(1), down: v.getUint8(2) === 1 };
    case MSG.WHEEL:
      return { type: 'wheel', dx: v.getInt16(1, true), dy: v.getInt16(3, true) };
    case MSG.KEY:
      return { type: 'key', usage: v.getUint16(1, true), down: v.getUint8(3) === 1 };
    case MSG.GAMEPAD:
    case MSG.GAMEPAD_BUTTONS:
      return {
        type: t === MSG.GAMEPAD ? 'gamepad' : 'gamepad-buttons',
        index: v.getUint8(1), buttons: v.getUint16(2, true),
        lx: v.getInt16(4, true), ly: v.getInt16(6, true),
        rx: v.getInt16(8, true), ry: v.getInt16(10, true),
        lt: v.getUint8(12), rt: v.getUint8(13),
        ...(v.byteLength >= 16 ? { seq: v.getUint16(14, true) } : {}),
      };
    case MSG.GAMEPAD_DISCONNECT:
      return { type: 'gamepad-disconnect', index: v.getUint8(1) };
    case MSG.RELEASE_ALL:
      return { type: 'release-all' };
    case MSG.TEXT:
      return { type: 'text', text: new TextDecoder().decode(new Uint8Array(v.buffer, v.byteOffset + 1, v.byteLength - 1)) };
    default:
      return { type: 'unknown', code: t };
  }
}
