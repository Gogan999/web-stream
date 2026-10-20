// Touch gestures on the video, turned into mouse actions. Pure state machine (times and points
// come in as arguments) so it can be tested; input.js feeds it pointer events.
//
// Trackpad (relative mouse mode): drag moves the cursor, tap clicks, tap then touch again and drag
// (or hold then drag) drags with the left button, two-finger tap right-clicks, two-finger drag scrolls.
// Direct (absolute mouse mode): the cursor goes where the finger is; tap clicks there, drag drags
// with the left button, hold right-clicks, two-finger tap right-clicks, two-finger drag scrolls.
// Both: pinching zooms the view (the stream, not the host) and moving the pinch pans it; while
// zoomed in, any two-finger drag pans instead of scrolling.

export const SLOP_PX = 10; // movement below this still counts as a tap or hold
export const HOLD_MS = 450;
export const TWO_TAP_MS = 300;
// Trackpad: a finger landing this soon after a tap drags with the left button once it moves.
export const TAP_DRAG_MS = 350;
// Two fingers: scroll once their midpoint moves this far, pinch once their spread changes this
// much (whichever is larger at that point decides).
export const SCROLL_PX = 2 * SLOP_PX;
export const PINCH_PX = 30;
// Wheel units per CSS px of two-finger drag (120 is one notch).
export const WHEEL_PER_PX = 2;

const LEFT = 0;
const RIGHT = 2;

export class TouchGestures {
  // out: { move(dx, dy), moveTo(x, y, reliable), button(b, down), wheel(dx, dy), haptic(),
  //        pinch(ratio, midX, midY, panX, panY), zoomed() }
  // moveTo's `reliable` is true where a button event follows at that point (touch down, drag start).
  // pinch: the spread changed by `ratio` around the midpoint, which moved by (panX, panY).
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
    this.two = null; // two fingers: null (undecided), 'scroll' or 'pinch'
    this.spread = null; // two fingers: { d, x, y } distance and midpoint at the start and last sent
    this.tapDrag = false;
    this.lastTap ??= -Infinity; // kept across reset() so cancel doesn't forget it
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
    } else if (n === 2 && (this.state === 'pending' || this.state === 'moving')) {
      this.state = 'two';
      this.t0 = t;
      this.two = null;
      for (const p of this.points.values()) { p.sx = p.x; p.sy = p.y; }
      this.spread = this.measure();
      this.spread0 = this.spread;
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
          this.state = this.tapDrag ? 'holding' : 'moving';
          // Move before pressing: the press lands away from the tap, so the host doesn't take the
          // two as a double-click (which would maximize a window dragged by its title bar).
          this.out.move(x - p.sx, y - p.sy);
          if (this.tapDrag) this.pressLeft();
        } else {
          this.state = 'dragging';
          this.out.moveTo(p.sx, p.sy, true); // the drag starts where the finger landed
          this.pressLeft();
          this.out.moveTo(x, y, false);
        }
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
          if (this.out.zoomed?.() && Math.hypot(x - p.sx, y - p.sy) >= SLOP_PX) this.two = 'pinch';
          else if (spread >= PINCH_PX && spread > pan) this.two = 'pinch';
          else if (pan >= SCROLL_PX && pan > spread) this.two = 'scroll';
          else return;
          if (this.two === 'scroll') {
            this.scroll(now.x - s.x, now.y - s.y); // the movement so far
            return;
          }
        }
        if (this.two === 'scroll') {
          // Each finger moves half the scroll; content follows the fingers like a touch screen.
          this.scroll(dx / 2, dy / 2);
          return;
        }
        const last = this.spread;
        this.spread = now;
        if (last.d > 0 && now.d > 0) this.out.pinch?.(now.d / last.d, now.x, now.y, now.x - last.x, now.y - last.y);
      }
    }
  }

  // Distance between the two fingers and their midpoint.
  measure() {
    const [a, b] = this.points.values();
    return { d: Math.hypot(a.x - b.x, a.y - b.y), x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  }

  up(id, t) {
    if (!this.points.delete(id)) return;
    switch (this.state) {
      case 'pending':
        this.click(LEFT);
        // A tap that ended a tap-drag's second touch is a double-click; a third touch doesn't drag.
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
