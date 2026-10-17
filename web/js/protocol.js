// Binary input messages sent on the "input" and "input-motion" data channels.
// Little-endian, first byte is the message type. See docs/protocol.md.

export const MSG = Object.freeze({
  MOUSE_REL: 0x01,
  MOUSE_ABS: 0x02,
  MOUSE_BUTTON: 0x03,
  WHEEL: 0x04,
  KEY: 0x05,
  GAMEPAD: 0x06,
  GAMEPAD_DISCONNECT: 0x07,
  RELEASE_ALL: 0x08,
});

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

// x, y in 0..1 over the active display.
export function mouseAbs(x, y) {
  const b = new ArrayBuffer(5);
  const v = new DataView(b);
  v.setUint8(0, MSG.MOUSE_ABS);
  v.setUint16(1, clampU16(x * 65535), true);
  v.setUint16(3, clampU16(y * 65535), true);
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
export function gamepad(index, s) {
  const b = new ArrayBuffer(14);
  const v = new DataView(b);
  v.setUint8(0, MSG.GAMEPAD);
  v.setUint8(1, index);
  v.setUint16(2, s.buttons & 0xffff, true);
  v.setInt16(4, clampI16(s.lx), true);
  v.setInt16(6, clampI16(s.ly), true);
  v.setInt16(8, clampI16(s.rx), true);
  v.setInt16(10, clampI16(s.ry), true);
  v.setUint8(12, clampU8(s.lt));
  v.setUint8(13, clampU8(s.rt));
  return b;
}

export function gamepadDisconnect(index) {
  return new Uint8Array([MSG.GAMEPAD_DISCONNECT, index]).buffer;
}

export function releaseAll() {
  return new Uint8Array([MSG.RELEASE_ALL]).buffer;
}

// Decoder, used by tests and the dev mock host.
export function decode(buf) {
  const v = new DataView(buf instanceof ArrayBuffer ? buf : buf.buffer, buf.byteOffset ?? 0, buf.byteLength);
  const t = v.getUint8(0);
  switch (t) {
    case MSG.MOUSE_REL:
      return { type: 'mouse-rel', dx: v.getInt16(1, true), dy: v.getInt16(3, true) };
    case MSG.MOUSE_ABS:
      return { type: 'mouse-abs', x: v.getUint16(1, true), y: v.getUint16(3, true) };
    case MSG.MOUSE_BUTTON:
      return { type: 'mouse-button', button: v.getUint8(1), down: v.getUint8(2) === 1 };
    case MSG.WHEEL:
      return { type: 'wheel', dx: v.getInt16(1, true), dy: v.getInt16(3, true) };
    case MSG.KEY:
      return { type: 'key', usage: v.getUint16(1, true), down: v.getUint8(3) === 1 };
    case MSG.GAMEPAD:
      return {
        type: 'gamepad', index: v.getUint8(1), buttons: v.getUint16(2, true),
        lx: v.getInt16(4, true), ly: v.getInt16(6, true),
        rx: v.getInt16(8, true), ry: v.getInt16(10, true),
        lt: v.getUint8(12), rt: v.getUint8(13),
      };
    case MSG.GAMEPAD_DISCONNECT:
      return { type: 'gamepad-disconnect', index: v.getUint8(1) };
    case MSG.RELEASE_ALL:
      return { type: 'release-all' };
    default:
      return { type: 'unknown', code: t };
  }
}
