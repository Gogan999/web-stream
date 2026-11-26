// Touch gestures on the video, turned into mouse actions.

export const SLOP_PX = 10;
export const HOLD_MS = 450;
export const TWO_TAP_MS = 300;
// Trackpad: a finger landing this soon after a tap drags with the left button once it moves.
export const TAP_DRAG_MS = 350;
// How long after the first finger a second one still makes a two-finger gesture, however far the
// first has moved (see above).
export const SECOND_FINGER_MS = 120;
// Two fingers: scroll once their midpoint moves this far, pinch once their spread changes this
// much.
export const SCROLL_PX = 2 * SLOP_PX;
export const PINCH_PX = 30;
// Wheel units per CSS px of two-finger drag.
export const WHEEL_PER_PX = 2;

const LEFT = 0;
const RIGHT = 2;
const MIDDLE = 1;

export class TouchGestures {
  // Times are ms on one clock (performance.now() in input.js); move's `t` may be left out, which
  // counts as late enough for anything.
  constructor(out, mode = 'trackpad') {
    this.out = out;
    this.mode = mode; // 'trackpad' or 'direct'
    this.reset();
  }

  reset() {
    this.points = new Map(); // id -> { x, y, sx, sy }
    this.primary = null;
    this.state = 'idle';
    this.t0 = 0;
    this.two = null;
    this.spread = null;
    this.tapDrag = false;
    this.three = null;
    this.slid = false;
    this.lastTap ??= -Infinity;
    this.leftDown = false;
    this.wheelRest = { x: 0, y: 0 };
  }

  setMode(mode) {
    this.cancel();
    this.mode = mode;
  }

  down(id, x, y, t) {
    this.points.set(id, { x, y, sx: x, sy: y });
    const n = this.points.size;
    if (n === 1) {
      this.primary = id;
      this.state = 'pending';
      this.t0 = t;
      this.tapDrag = this.mode === 'trackpad' && t - this.lastTap < TAP_DRAG_MS;
      if (this.mode === 'direct') this.out.moveTo(x, y, true);
    } else if (n === 2 && (this.state === 'pending' || this.state === 'moving' ||
        (this.state === 'done' && this.two === 'pinch'))) {
      if (this.state !== 'done') this.two = null;
      this.state = 'two';
      this.t0 = t;
      this.slid = false;
      for (const p of this.points.values()) { p.sx = p.x; p.sy = p.y; }
      this.spread = this.measure();
      this.spread0 = this.spread;
    } else if (n >= 3) {
      this.releaseLeft();
      if (n === 3 && this.state === 'two' && !this.two) this.three = { t0: this.t0 };
      else this.three = null;
      this.state = 'done';
    }
  }

  move(id, x, y, t) {
    const p = this.points.get(id);
    if (!p) return;
    const dx = x - p.x;
    const dy = y - p.y;
    p.x = x;
    p.y = y;
    if (this.three && Math.hypot(x - p.sx, y - p.sy) >= SLOP_PX) this.three = null;
    if (this.state !== 'two' && id !== this.primary) return;
    switch (this.state) {
      case 'pending':
        if (!this.slid && Math.hypot(x - p.sx, y - p.sy) < SLOP_PX) return;
        this.slid = true;
        if (this.presses && t - this.t0 < SECOND_FINGER_MS) return;
        this.startMove(p);
        return;
      case 'moving':
      case 'holding':
        this.out.move(dx, dy);
        return;
      case 'dragging':
        this.out.moveTo(x, y, false);
        return;
      case 'two': {
        const now = this.measure();
        if (!this.two) {
          const s = this.spread0;
          const spread = Math.abs(now.d - s.d);
          const pan = Math.hypot(now.x - s.x, now.y - s.y);
          if (this.out.pans?.() && Math.hypot(x - p.sx, y - p.sy) >= SLOP_PX) this.two = 'pinch';
          else if (spread >= PINCH_PX && spread > pan) this.two = 'pinch';
          else if (pan >= SCROLL_PX && pan > spread) this.two = 'scroll';
          else return;
          if (this.two === 'scroll') {
            this.scroll(now.x - s.x, now.y - s.y);
            return;
          }
        }
        if (this.two === 'scroll') {
          this.scroll(dx / 2, dy / 2);
          return;
        }
        const last = this.spread;
        this.spread = now;
        if (last.d > 0 && now.d > 0) this.out.pinch?.(now.d / last.d, now.x, now.y, now.x - last.x, now.y - last.y);
      }
    }
  }

  // Would leaving 'pending' by moving press a button?
  get presses() {
    return this.mode === 'direct' || this.tapDrag;
  }

  // The finger moved past the slop: from where it landed to where it is now.
  startMove(p) {
    this.slid = false;
    if (this.mode === 'trackpad') {
      this.state = this.tapDrag ? 'holding' : 'moving';
      this.out.move(p.x - p.sx, p.y - p.sy);
      if (this.tapDrag) this.pressLeft();
    } else {
      this.state = 'dragging';
      this.out.moveTo(p.sx, p.sy, true); // the drag starts where the finger landed
      this.pressLeft();
      this.out.moveTo(p.x, p.y, false);
    }
  }

  // Distance between the two fingers and their midpoint.
  measure() {
    const [a, b] = this.points.values();
    return { d: Math.hypot(a.x - b.x, a.y - b.y), x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  }

  up(id, t) {
    const p = this.points.get(id);
    if (!p) return;
    if (this.state === 'pending' && this.slid) this.startMove(p);
    this.points.delete(id);
    if (this.three && !this.points.size) {
      const tap = t - this.three.t0 < TWO_TAP_MS;
      this.three = null;
      this.state = 'idle';
      if (tap) this.click(MIDDLE);
      return;
    }
    switch (this.state) {
      case 'pending':
        this.click(LEFT);
        this.lastTap = this.tapDrag ? -Infinity : t;
        break;
      case 'holding':
      case 'dragging':
        this.releaseLeft();
        break;
      case 'two':
        if (!this.two && t - this.t0 < TWO_TAP_MS) this.click(RIGHT);
        break;
    }
    this.state = this.points.size ? 'done' : 'idle';
  }

  // Called while a finger is down so a waiting drag starts and a hold fires without the finger
  // moving.
  tick(t) {
    if (this.state !== 'pending') return;
    if (this.slid) {
      if (t - this.t0 >= SECOND_FINGER_MS) this.startMove(this.points.get(this.primary));
      return;
    }
    if (t - this.t0 < HOLD_MS) return;
    this.out.haptic?.();
    if (this.mode === 'trackpad') {
      this.state = 'holding';
      this.pressLeft();
    } else {
      this.state = 'done';
      this.click(RIGHT);
    }
  }

  // pointercancel, blur, mode change: let go of anything held.
  cancel() {
    this.releaseLeft();
    this.reset();
  }

  click(b) {
    this.out.button(b, true);
    this.out.button(b, false);
  }

  pressLeft() {
    if (this.leftDown) return;
    this.leftDown = true;
    this.out.button(LEFT, true);
  }

  releaseLeft() {
    if (!this.leftDown) return;
    this.leftDown = false;
    this.out.button(LEFT, false);
  }

  // Finger down (+dy) drags the page down, which is wheel up (+ on Windows); finger left (-dx)
  // reveals what is to the right, which is horizontal wheel right (+).
  scroll(dx, dy) {
    const r = this.wheelRest;
    r.x += -dx * WHEEL_PER_PX;
    r.y += dy * WHEEL_PER_PX;
    const wx = Math.trunc(r.x);
    const wy = Math.trunc(r.y);
    if (!wx && !wy) return;
    r.x -= wx;
    r.y -= wy;
    this.out.wheel(wx, wy);
  }
}
