// Zoom on touch screens: the stream is scaled and panned in the page, the host is untouched.
// A CSS transform on the element holding the video and the pointer overlay, so the input mapping
// (getBoundingClientRect) and the overlay follow it without knowing about it.

export const MAX_ZOOM = 6;
// Trackpad: the view pans to keep the host pointer at least this far (CSS px) inside the edges.
export const FOLLOW_MARGIN = 48;

// The view after a pinch: scale `s` and offset (x, y) of the content's top-left, in stage px.
// The pinch scales by `ratio` around (cx, cy) and moves by (dx, dy); the content always covers
// the stage (w x h), so it can't be zoomed out past fitting or panned off.
export function pinchView({ s, x, y }, ratio, cx, cy, dx, dy, w, h) {
  const ns = Math.min(MAX_ZOOM, Math.max(1, s * ratio));
  const k = ns / s;
  return clampView({ s: ns, x: cx - (cx - x) * k + dx, y: cy - (cy - y) * k + dy }, w, h);
}

export function clampView({ s, x, y }, w, h) {
  return { s, x: Math.min(0, Math.max(w - w * s, x)), y: Math.min(0, Math.max(h - h * s, y)) };
}

// Pan just enough that content point (px, py), unscaled, sits `margin` inside the stage.
export function followView(v, px, py, w, h, margin = FOLLOW_MARGIN) {
  const m = Math.min(margin, w / 4, h / 4);
  const sx = px * v.s + v.x;
  const sy = py * v.s + v.y;
  let { x, y } = v;
  if (sx < m) x += m - sx;
  else if (sx > w - m) x -= sx - (w - m);
  if (sy < m) y += m - sy;
  else if (sy > h - m) y -= sy - (h - m);
  return clampView({ s: v.s, x, y }, w, h);
}

export class ZoomView {
  // el: the transformed element; it fills `stage`, which clips it.
  constructor({ el, stage, onChange }) {
    this.el = el;
    this.stage = stage;
    this.onChange = onChange; // (zoomed) when it zooms in from fit or back to it
    this.v = { s: 1, x: 0, y: 0 };
    // Rotation, a resized window or the phone keyboard opening: keep the zoom, stay covering.
    new ResizeObserver(() => this.set(clampView(this.v, stage.clientWidth, stage.clientHeight))).observe(stage);
  }

  get zoomed() {
    return this.v.s > 1;
  }

  // Client coordinates, as the touch gestures report them.
  pinch(ratio, clientX, clientY, dx, dy) {
    const r = this.stage.getBoundingClientRect();
    this.set(pinchView(this.v, ratio, clientX - r.left, clientY - r.top, dx, dy, r.width, r.height));
  }

  // (x, y) in the element's own, unscaled px.
  follow(x, y) {
    if (!this.zoomed) return;
    this.set(followView(this.v, x, y, this.stage.clientWidth, this.stage.clientHeight));
  }

  reset() {
    this.set({ s: 1, x: 0, y: 0 });
  }

  set(v) {
    const was = this.zoomed;
    this.v = v;
    this.el.style.transform = v.s === 1 ? '' : `translate(${v.x}px, ${v.y}px) scale(${v.s})`;
    if (was !== this.zoomed) this.onChange?.(this.zoomed);
  }
}
