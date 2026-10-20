// Captures keyboard, mouse and gamepads and sends them to the host.

import * as P from './protocol.js';
import { hidUsage } from './keymap.js';
import { toXInput, sameState } from './gamepad.js';
import { CHANNELS } from './session.js';
import { TouchGestures, HOLD_MS } from './touch.js';

const GAMEPAD_KEEPALIVE_MS = 100;
const MAX_PADS = 4;
// Chrome reads gamepads every 4 ms; polling only once per animation frame would leave a press waiting
// up to a display frame (16 ms at 60 Hz) before it is even seen.
export const GAMEPAD_POLL_MS = 4;
// Mouse motion goes out at most this often. Raw pointer updates arrive as fast as the mouse reports
// (up to 8 kHz); faster than this they are summed, so a gaming mouse can't flood the channel.
export const MOTION_GAP_MS = 2;

// Chrome reports ~100 px per wheel notch; Windows expects 120 per notch.
export function wheelToDelta(e) {
  const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 : 1.2;
  // DOM: +deltaY scrolls down. Windows: +delta scrolls up (away from user).
  return { dx: Math.round(e.deltaX * scale), dy: Math.round(-e.deltaY * scale) };
}

// Picture pixels per CSS pixel of the letterboxed video, so a trackpad drag across the picture moves
// the host cursor across the display.
export function pictureScale(rect, videoW, videoH) {
  if (!videoW || !videoH || !rect.width || !rect.height) return 1;
  return Math.max(videoW / rect.width, videoH / rect.height);
}

// Map a client point to 0..1 over the actual picture inside a letterboxed (object-fit: contain) video.
// With `clamp`, points in the black bars snap to the nearest edge instead of being dropped.
export function videoPoint(clientX, clientY, rect, videoW, videoH, clamp = false) {
  if (!videoW || !videoH) return null;
  const scale = Math.min(rect.width / videoW, rect.height / videoH);
  const w = videoW * scale;
  const h = videoH * scale;
  const left = rect.left + (rect.width - w) / 2;
  const top = rect.top + (rect.height - h) / 2;
  const x = (clientX - left) / w;
  const y = (clientY - top) / h;
  if (clamp) return { x: Math.min(1, Math.max(0, x)), y: Math.min(1, Math.max(0, y)) };
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}

export class InputCapture {
  constructor({ session, video, stage, onHotkey, keyTarget = null, onKeySent = null, onReleaseAll = null, displaySize = null, zoom = null }) {
    this.session = session;
    this.video = video;
    this.stage = stage;
    this.onHotkey = onHotkey;
    this.keyTarget = keyTarget; // the on-screen keyboard's textarea (softkeys.js)
    this.onKeySent = onKeySent;
    this.onReleaseAll = onReleaseAll; // softkeys: forget sticky modifiers the host just let go
    this.displaySize = displaySize; // () => { width, height } of the host display, for trackpad scaling
    this.zoom = zoom; // ZoomView: pinches zoom and pan the stream in the page
    this.touchActive = false; // the stream was touched last, so keys from a paired keyboard go to it
    this.touchRest = { x: 0, y: 0 }; // sub-pixel trackpad motion not yet sent
    this.holdTimer = 0;
    this.gestures = new TouchGestures({
      move: (dx, dy) => this.touchMove(dx, dy),
      moveTo: (x, y, reliable) => this.touchMoveTo(x, y, reliable),
      button: (b, down) => {
        this.rel(P.mouseButton(b, down));
        if (!down) this.onKeySent?.(0, false); // a click ends a sticky Ctrl/Alt/Shift/Win
      },
      wheel: (dx, dy) => {
        const b = this.touchBatch;
        if (b) {
          b.wheel = { dx: (b.wheel?.dx ?? 0) + dx, dy: (b.wheel?.dy ?? 0) + dy };
          return;
        }
        this.rel(P.wheel(dx, dy));
        this.onKeySent?.(0, false);
      },
      haptic: () => navigator.vibrate?.(15),
      pinch: (ratio, x, y, dx, dy) => this.zoom?.pinch(ratio, x, y, dx, dy),
      zoomed: () => !!this.zoom?.zoomed,
    });
    this.mode = 'relative'; // 'relative' (pointer lock) or 'absolute'
    this.heldKeys = new Set();
    this.pads = new Array(MAX_PADS).fill(null); // last sent state per slot
    this.motion = { dx: 0, dy: 0, abs: null }; // not yet sent because input-motion was backed up
    this.padSentAt = new Array(MAX_PADS).fill(0);
    this.abort = new AbortController();
    this.rafId = 0;
    this.padTimer = 0;
    this.motionTimer = 0;
    this.motionSentAt = -Infinity;
  }

  get locked() {
    return document.pointerLockElement === this.video;
  }

  // Input reaches the host only while the user has clearly handed it to the stream.
  get active() {
    return this.locked || (this.mode === 'absolute' && this.stageFocused) || this.touchActive ||
      (!!this.keyTarget && document.activeElement === this.keyTarget);
  }

  // Pending motion goes first, so a click, wheel or key lands where the pointer just moved.
  rel(buf) {
    this.flushMotion();
    this.session.sendInput(CHANNELS.INPUT, buf);
  }

  start() {
    const opt = { signal: this.abort.signal };
    const v = this.video;

    // Mouse events are taken at the document so they are seen once whether or not the
    // pointer is locked; ours() keeps toolbar clicks out of the stream.
    // Mouse events a touch screen synthesizes are skipped; touch goes through the pointer handlers below.
    const fromTouch = (e) => e.sourceCapabilities?.firesTouchEvents === true;
    const ours = (e) => (this.locked || e.target === v) && !fromTouch(e);
    document.addEventListener('mousedown', (e) => { if (ours(e)) this.onMouseDown(e); }, opt);
    document.addEventListener('mouseup', (e) => { if (ours(e)) this.onMouseUp(e); }, opt);
    // Chrome holds mousemove back to the next animation frame, up to a display frame late;
    // pointerrawupdate fires as soon as the mouse reports, so motion uses it where it exists.
    const raw = 'onpointerrawupdate' in window;
    if (raw) {
      document.addEventListener('pointerrawupdate', (e) => {
        if (e.pointerType !== 'touch' && ours(e)) this.onMouseMove(e);
      }, opt);
    }
    document.addEventListener('mousemove', (e) => { if (!raw && ours(e)) this.onMouseMove(e); }, opt);
    document.addEventListener('wheel', (e) => { if (ours(e)) this.onWheel(e); }, { ...opt, passive: false });
    v.addEventListener('contextmenu', (e) => e.preventDefault(), opt);
    v.addEventListener('mouseenter', (e) => { if (!fromTouch(e)) this.stageFocused = true; }, opt);
    v.addEventListener('mouseleave', (e) => {
      if (fromTouch(e)) return;
      if (this.mode === 'absolute') this.releaseAll();
      this.stageFocused = false;
    }, opt);

    const touch = (e) => e.pointerType === 'touch';
    v.addEventListener('pointerdown', (e) => { if (touch(e)) this.onTouchDown(e); }, opt);
    v.addEventListener('pointermove', (e) => {
      if (!touch(e)) return;
      // Coalesced events keep a fast swipe's full path; the sum is the same, but the hold/tap
      // slop check sees every step.
      const steps = e.getCoalescedEvents?.();
      this.batchTouch(() => {
        for (const c of steps?.length ? steps : [e]) this.gestures.move(e.pointerId, c.clientX, c.clientY);
      });
    }, opt);
    v.addEventListener('pointerup', (e) => { if (touch(e)) this.onTouchUp(e); }, opt);
    v.addEventListener('pointercancel', (e) => { if (touch(e)) this.gestures.cancel(); }, opt);
    // Touching anything outside the stream (toolbar, settings) hands keys back to the page.
    document.addEventListener('pointerdown', (e) => {
      if (!this.stage.contains(e.target) && e.target !== this.keyTarget) this.touchActive = false;
    }, { ...opt, capture: true });

    window.addEventListener('keydown', (e) => this.onKey(e, true), { ...opt, capture: true });
    window.addEventListener('keyup', (e) => this.onKey(e, false), { ...opt, capture: true });
    window.addEventListener('blur', () => this.releaseAll(), opt);
    document.addEventListener('visibilitychange', () => {
      if (document.hidden) this.releaseAll();
      else this.watchGamepads();
    }, opt);
    document.addEventListener('pointerlockchange', () => { if (!this.locked) this.releaseAll(); }, opt);

    window.addEventListener('gamepadconnected', () => this.watchGamepads(), opt);
    window.addEventListener('gamepaddisconnected', (e) => {
      const i = e.gamepad.index;
      if (i < MAX_PADS && this.pads[i]) {
        this.pads[i] = null;
        this.rel(P.gamepadDisconnect(i));
      }
    }, opt);
    this.session.addEventListener('control', (e) => this.onControl(e.detail), opt);
    const poll = () => { this.flushMotion(); this.pollGamepads(); this.rafId = requestAnimationFrame(poll); };
    this.rafId = requestAnimationFrame(poll);
    this.watchGamepads();
  }

  stop() {
    this.releaseAll();
    this.abort.abort();
    cancelAnimationFrame(this.rafId);
    clearInterval(this.padTimer);
    clearTimeout(this.motionTimer);
    clearTimeout(this.holdTimer);
    if (this.locked) document.exitPointerLock();
  }

  setMode(mode) {
    this.mode = mode;
    this.gestures.setMode(mode === 'relative' ? 'trackpad' : 'direct');
    if (mode === 'absolute' && this.locked) document.exitPointerLock();
  }

  async lockPointer() {
    try {
      await this.video.requestPointerLock({ unadjustedMovement: true });
    } catch {
      // Raw input unsupported on this platform; fall back to OS-accelerated movement.
      try { await this.video.requestPointerLock(); } catch { /* user gesture missing */ }
    }
  }

  onMouseDown(e) {
    if (this.mode === 'relative' && !this.locked) {
      // First click only grabs the pointer; it is not forwarded.
      e.preventDefault();
      this.lockPointer();
      return;
    }
    if (!this.active) return;
    e.preventDefault();
    if (this.mode === 'absolute') this.onMouseMove(e);
    this.rel(P.mouseButton(e.button, true));
  }

  onMouseUp(e) {
    if (!this.active) return;
    e.preventDefault();
    this.rel(P.mouseButton(e.button, false));
  }

  onMouseMove(e) {
    if (this.locked) {
      this.motion.dx += e.movementX;
      this.motion.dy += e.movementY;
    } else if (this.mode === 'absolute') {
      const p = videoPoint(e.clientX, e.clientY, this.video.getBoundingClientRect(),
        this.video.videoWidth, this.video.videoHeight);
      if (p) this.motion.abs = p;
    }
    const wait = this.motionSentAt + MOTION_GAP_MS - performance.now();
    if (wait <= 0) this.flushMotion();
    else if (!this.motionTimer) {
      this.motionTimer = setTimeout(() => { this.motionTimer = 0; this.flushMotion(); }, wait);
    }
  }

  // Sends pending motion if the channel has room. When it is backed up, relative deltas
  // keep accumulating and absolute keeps only the latest point; the next flush (next
  // event or next animation frame) sends the sum, so no movement is lost or delayed in a queue.
  flushMotion() {
    const m = this.motion;
    if (!m.abs && !m.dx && !m.dy) return;
    this.motionSentAt = performance.now();
    if (m.abs && this.session.sendInput(CHANNELS.INPUT_RT, P.mouseAbs(m.abs.x, m.abs.y))) m.abs = null;
    while ((m.dx || m.dy) && this.session.canSend(CHANNELS.INPUT_RT)) {
      const dx = Math.max(-32768, Math.min(32767, m.dx));
      const dy = Math.max(-32768, Math.min(32767, m.dy));
      this.session.sendInput(CHANNELS.INPUT_RT, P.mouseRel(dx, dy));
      m.dx -= dx;
      m.dy -= dy;
    }
  }

  onTouchDown(e) {
    e.preventDefault(); // no synthesized mouse events, no focus change
    this.video.setPointerCapture?.(e.pointerId);
    this.touchActive = true;
    if (this.video.paused) this.video.play().catch(() => {}); // autoplay refused (iOS): a touch starts it
    this.gestures.down(e.pointerId, e.clientX, e.clientY, performance.now());
    clearTimeout(this.holdTimer);
    this.holdTimer = setTimeout(() => this.gestures.tick(performance.now()), HOLD_MS + 10);
  }

  onTouchUp(e) {
    this.gestures.up(e.pointerId, performance.now());
    if (!this.gestures.points.size) clearTimeout(this.holdTimer);
  }

  // Coalesced touch steps go out as one motion message and one wheel message per event.
  batchTouch(fn) {
    this.touchBatch = { wheel: null };
    try {
      fn();
    } finally {
      const w = this.touchBatch.wheel;
      this.touchBatch = null;
      if (w) this.gestures.out.wheel(w.dx, w.dy);
      this.flushMotion();
    }
  }

  touchMove(dx, dy) {
    // Relative motion is in host display pixels, so scale CSS px -> video px -> display px.
    const vw = this.video.videoWidth;
    const d = this.displaySize?.();
    const toDisplay = d?.width && vw ? d.width / vw : 1;
    const scale = pictureScale(this.video.getBoundingClientRect(), vw, this.video.videoHeight) * toDisplay;
    const r = this.touchRest;
    r.x += dx * scale;
    r.y += dy * scale;
    const ix = Math.trunc(r.x);
    const iy = Math.trunc(r.y);
    r.x -= ix;
    r.y -= iy;
    this.motion.dx += ix;
    this.motion.dy += iy;
    if (!this.touchBatch) this.flushMotion();
  }

  // `reliable`: where a click is about to land (touch down, drag start), the position goes on the
  // ordered reliable channel ahead of the button, so a lost or held motion packet can't misplace it.
  touchMoveTo(x, y, reliable) {
    const p = videoPoint(x, y, this.video.getBoundingClientRect(), this.video.videoWidth, this.video.videoHeight, true);
    if (!p) return;
    if (reliable) {
      this.motion.abs = null;
      this.rel(P.mouseAbs(p.x, p.y));
      return;
    }
    this.motion.abs = p;
    if (!this.touchBatch) this.flushMotion();
  }

  onWheel(e) {
    if (!this.active) return;
    e.preventDefault();
    const { dx, dy } = wheelToDelta(e);
    if (dx || dy) this.rel(P.wheel(dx, dy));
  }

  onKey(e, down) {
    // Client hotkeys: Ctrl+Alt+Shift+<key>. Never forwarded.
    if (down && e.ctrlKey && e.altKey && e.shiftKey && this.onHotkey && this.onHotkey(e.code)) {
      e.preventDefault();
      e.stopPropagation();
      return;
    }
    if (!this.active) return;
    // From the on-screen keyboard, characters are read from the text it types (softkeys.js);
    // only named keys with a real key code are taken here. 229 is "the IME has it".
    // Shortcuts (Ctrl/Alt/Win + key) from a hardware keyboard are still taken here.
    if (e.target === this.keyTarget &&
        (e.keyCode === 229 || (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey))) return;
    const usage = hidUsage(e.code);
    if (!usage) return;
    e.preventDefault();
    e.stopPropagation();
    if (down) this.heldKeys.add(usage);
    else if (!this.heldKeys.delete(usage)) return; // key went down before we had focus
    this.rel(P.key(usage, down));
    this.onKeySent?.(usage, down);
  }

  // A full press and release of one key, for keys the browser keeps for itself (Esc in a window).
  // Modifiers still held from the hotkey are let go around it, so the host sees the key alone.
  tapKey(usage) {
    const mods = [...this.heldKeys].filter((u) => u >= 0xe0 && u <= 0xe7);
    for (const u of mods) this.rel(P.key(u, false));
    this.rel(P.key(usage, true));
    this.rel(P.key(usage, false));
    for (const u of mods) this.rel(P.key(u, true));
  }

  releaseAll() {
    this.gestures.reset();
    clearTimeout(this.holdTimer);
    this.heldKeys.clear();
    this.onReleaseAll?.();
    this.motion = { dx: 0, dy: 0, abs: null };
    this.rel(P.releaseAll());
  }

  // Polls gamepads every GAMEPAD_POLL_MS while any is connected.
  watchGamepads() {
    if (this.padTimer || this.abort.signal.aborted) return;
    if (![...(navigator.getGamepads?.() ?? [])].some((p) => p?.connected)) return;
    this.padTimer = setInterval(() => {
      // A hidden tab gets no input; gamepadconnected restarts polling.
      if (document.hidden || !this.pollGamepads()) {
        clearInterval(this.padTimer);
        this.padTimer = 0;
      }
    }, GAMEPAD_POLL_MS);
  }

  // Sends what changed; returns whether any gamepad is connected.
  pollGamepads() {
    if (!navigator.getGamepads) return false;
    const now = performance.now();
    let any = false;
    for (const pad of navigator.getGamepads()) {
      if (!pad || !pad.connected || pad.index >= MAX_PADS) continue;
      any = true;
      const i = pad.index;
      const s = toXInput(pad);
      if ((!sameState(s, this.pads[i]) || now - this.padSentAt[i] >= GAMEPAD_KEEPALIVE_MS) &&
          this.session.sendInput(CHANNELS.INPUT_RT, P.gamepad(i, s))) {
        this.pads[i] = s;
        this.padSentAt[i] = now;
      }
    }
    return any;
  }

  onControl(msg) {
    if (msg.type !== 'rumble') return;
    const pad = navigator.getGamepads?.()[msg.index];
    const act = pad?.vibrationActuator;
    if (!act) return;
    const strong = Math.max(0, Math.min(1, +msg.strong || 0));
    const weak = Math.max(0, Math.min(1, +msg.weak || 0));
    if (!strong && !weak) act.reset?.();
    // Host sends a new value on every change and 0/0 to stop; the long duration just bridges gaps.
    else act.playEffect('dual-rumble', { duration: 5000, strongMagnitude: strong, weakMagnitude: weak }).catch(() => {});
  }
}
