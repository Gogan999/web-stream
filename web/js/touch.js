// Touch gestures on the video, turned into mouse actions. Pure state machine (times and points
// come in as arguments) so it can be tested; input.js feeds it pointer events.
//
// Trackpad (relative mouse mode): drag moves the cursor, tap clicks, hold then drag drags with the
// left button, two-finger tap right-clicks, two-finger drag scrolls.
// Direct (absolute mouse mode): the cursor goes where the finger is; tap clicks there, drag drags
// with the left button, hold right-clicks, two-finger tap right-clicks, two-finger drag scrolls.

export const SLOP_PX = 10; // movement below this still counts as a tap or hold
export const HOLD_MS = 450;
export const TWO_TAP_MS = 300;
// Wheel units per CSS px of two-finger drag (120 is one notch).
export const WHEEL_PER_PX = 2;

const LEFT = 0;
const RIGHT = 2;

export class TouchGestures {
  // out: { move(dx, dy), moveTo(x, y), button(b, down), wheel(dx, dy), haptic() }
  constructor(out, mode = 'trackpad') {
    this.out = out;
    this.mode = mode; // 'trackpad' or 'direct'
    this.reset();
  }

  reset() {
    this.points = new Map(); // id -> { x, y, sx, sy }
    this.primary = null; // first finger down
    this.state = 'idle'; // idle, pending, moving, holding, dragging, two, done
    this.t0 = 0;
    this.scrolling = false;
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
      if (this.mode === 'direct') this.out.moveTo(x, y);
    } else if (n === 2 && (this.state === 'pending' || this.state === 'moving')) {
      this.state = 'two';
      this.t0 = t;
      this.scrolling = false;
      for (const p of this.points.values()) { p.sx = p.x; p.sy = p.y; }
    } else if (n >= 3) {
      this.releaseLeft();
      this.state = 'done';
    }
  }

  move(id, x, y) {
    const p = this.points.get(id);
    if (!p) return;
    const dx = x - p.x;
    const dy = y - p.y;
    p.x = x;
    p.y = y;
    // A second finger that lands mid-drag is ignored rather than moving the cursor too.
    if (this.state !== 'two' && id !== this.primary) return;
    switch (this.state) {
      case 'pending':
        if (Math.hypot(x - p.sx, y - p.sy) < SLOP_PX) return;
        if (this.mode === 'trackpad') {
          this.state = 'moving';
          this.out.move(x - p.sx, y - p.sy);
        } else {
          this.state = 'dragging';
          this.pressLeft();
          this.out.moveTo(x, y);
        }
        return;
      case 'moving':
      case 'holding':
        this.out.move(dx, dy);
        return;
      case 'dragging':
        this.out.moveTo(x, y);
        return;
      case 'two': {
        if (!this.scrolling) {
          if (Math.hypot(x - p.sx, y - p.sy) < SLOP_PX) return;
          this.scrolling = true;
        }
        // Each finger moves half the scroll; content follows the fingers like a touch screen.
        this.scroll(dx / 2, dy / 2);
      }
    }
  }

  up(id, t) {
    if (!this.points.delete(id)) return;
    switch (this.state) {
      case 'pending':
        this.click(LEFT);
        break;
      case 'holding':
      case 'dragging':
        this.releaseLeft();
        break;
      case 'two':
        if (!this.scrolling && t - this.t0 < TWO_TAP_MS) this.click(RIGHT);
        break;
    }
    this.state = this.points.size ? 'done' : 'idle';
  }

  // Called while a finger is down (input.js runs a timer) so a hold fires without the finger moving.
  tick(t) {
    if (this.state !== 'pending' || t - this.t0 < HOLD_MS) return;
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
