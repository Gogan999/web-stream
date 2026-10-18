// Gamepad API "standard" mapping -> XInput state.

export const XI = Object.freeze({
  DPAD_UP: 0x0001, DPAD_DOWN: 0x0002, DPAD_LEFT: 0x0004, DPAD_RIGHT: 0x0008,
  START: 0x0010, BACK: 0x0020, LEFT_THUMB: 0x0040, RIGHT_THUMB: 0x0080,
  LEFT_SHOULDER: 0x0100, RIGHT_SHOULDER: 0x0200, GUIDE: 0x0400,
  A: 0x1000, B: 0x2000, X: 0x4000, Y: 0x8000,
});

// Index = standard-mapping button index.
const BUTTON_BITS = [
  XI.A, XI.B, XI.X, XI.Y, XI.LEFT_SHOULDER, XI.RIGHT_SHOULDER,
  0, 0, // 6, 7 are the analog triggers
  XI.BACK, XI.START, XI.LEFT_THUMB, XI.RIGHT_THUMB,
  XI.DPAD_UP, XI.DPAD_DOWN, XI.DPAD_LEFT, XI.DPAD_RIGHT, XI.GUIDE,
];

const axis = (v) => Math.round(Math.max(-1, Math.min(1, v || 0)) * 32767);
const trigger = (b) => Math.round(Math.max(0, Math.min(1, b ? b.value : 0)) * 255);

export function toXInput(pad) {
  let buttons = 0;
  for (let i = 0; i < BUTTON_BITS.length && i < pad.buttons.length; i++) {
    if (BUTTON_BITS[i] && pad.buttons[i].pressed) buttons |= BUTTON_BITS[i];
  }
  const a = pad.axes;
  return {
    buttons,
    lx: axis(a[0]), ly: -axis(a[1]),
    rx: axis(a[2]), ry: -axis(a[3]),
    lt: trigger(pad.buttons[6]), rt: trigger(pad.buttons[7]),
  };
}

export function sameState(a, b) {
  return !!a && !!b && a.buttons === b.buttons && a.lx === b.lx && a.ly === b.ly &&
    a.rx === b.rx && a.ry === b.ry && a.lt === b.lt && a.rt === b.rt;
}
