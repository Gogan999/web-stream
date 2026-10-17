// Captures keyboard, mouse and gamepads and sends them to the host.

import * as P from './protocol.js';
import { hidUsage } from './keymap.js';
import { toXInput, sameState } from './gamepad.js';
import { CHANNELS } from './session.js';

const GAMEPAD_KEEPALIVE_MS = 100;
const MAX_PADS = 4;

// Chrome reports ~100 px per wheel notch; Windows expects 120 per notch.
export function wheelToDelta(e) {
  const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 : 1.2;
  // DOM: +deltaY scrolls down. Windows: +delta scrolls up (away from user).
  return { dx: Math.round(e.deltaX * scale), dy: Math.round(-e.deltaY * scale) };
}

// Map a client point to 0..1 over the actual picture inside a letterboxed (object-fit: contain) video.
export function videoPoint(clientX, clientY, rect, videoW, videoH) {
  if (!videoW || !videoH) return null;
  const scale = Math.min(rect.width / videoW, rect.height / videoH);
  const w = videoW * scale;
  const h = videoH * scale;
  const left = rect.left + (rect.width - w) / 2;
  const top = rect.top + (rect.height - h) / 2;
  const x = (clientX - left) / w;
  const y = (clientY - top) / h;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}

export class InputCapture {
  constructor({ session, video, stage, onHotkey }) {
    this.session = session;
    this.video = video;
    this.stage = stage;
    this.onHotkey = onHotkey;
    this.mode = 'relative'; // 'relative' (pointer lock) or 'absolute'
    this.heldKeys = new Set();
    this.pads = new Array(MAX_PADS).fill(null); // last sent state per slot
    this.motion = { dx: 0, dy: 0, abs: null }; // not yet sent because input-motion was backed up
    this.padSentAt = new Array(MAX_PADS).fill(0);
    this.abort = new AbortController();
    this.rafId = 0;
  }

  get locked() {
    return document.pointerLockElement === this.video;
  }

  // Input reaches the host only while the user has clearly handed it to the stream.
  get active() {
    return this.locked || (this.mode === 'absolute' && this.stageFocused);
  }

  rel(buf) { this.session.sendInput(CHANNELS.INPUT, buf); }

  start() {
    const opt = { signal: this.abort.signal };
    const v = this.video;

    // Mouse events are taken at the document so they are seen once whether or not the
    // pointer is locked; ours() keeps toolbar clicks out of the stream.
    const ours = (e) => this.locked || e.target === v;
    document.addEventListener('mousedown', (e) => { if (ours(e)) this.onMouseDown(e); }, opt);
    document.addEventListener('mouseup', (e) => { if (ours(e)) this.onMouseUp(e); }, opt);
    document.addEventListener('mousemove', (e) => { if (ours(e)) this.onMouseMove(e); }, opt);
    document.addEventListener('wheel', (e) => { if (ours(e)) this.onWheel(e); }, { ...opt, passive: false });
    v.addEventListener('contextmenu', (e) => e.preventDefault(), opt);
    v.addEventListener('mouseenter', () => { this.stageFocused = true; }, opt);
    v.addEventListener('mouseleave', () => {
      if (this.mode === 'absolute') this.releaseAll();
      this.stageFocused = false;
    }, opt);

    window.addEventListener('keydown', (e) => this.onKey(e, true), { ...opt, capture: true });
    window.addEventListener('keyup', (e) => this.onKey(e, false), { ...opt, capture: true });
    window.addEventListener('blur', () => this.releaseAll(), opt);
    document.addEventListener('visibilitychange', () => { if (document.hidden) this.releaseAll(); }, opt);
    document.addEventListener('pointerlockchange', () => { if (!this.locked) this.releaseAll(); }, opt);

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
  }

  stop() {
    this.releaseAll();
    this.abort.abort();
    cancelAnimationFrame(this.rafId);
    if (this.locked) document.exitPointerLock();
  }

  setMode(mode) {
    this.mode = mode;
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
    this.flushMotion();
  }

  // Sends pending motion if the channel has room. When it is backed up, relative deltas
  // keep accumulating and absolute keeps only the latest point; the next flush (next
  // event or next animation frame) sends the sum, so no movement is lost or delayed in a queue.
  flushMotion() {
    const m = this.motion;
    if (m.abs && this.session.sendInput(CHANNELS.INPUT_RT, P.mouseAbs(m.abs.x, m.abs.y))) m.abs = null;
    while ((m.dx || m.dy) && this.session.canSend(CHANNELS.INPUT_RT)) {
      const dx = Math.max(-32768, Math.min(32767, m.dx));
      const dy = Math.max(-32768, Math.min(32767, m.dy));
      this.session.sendInput(CHANNELS.INPUT_RT, P.mouseRel(dx, dy));
      m.dx -= dx;
      m.dy -= dy;
    }
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
    const usage = hidUsage(e.code);
    if (!usage) return;
    e.preventDefault();
    e.stopPropagation();
    if (down) this.heldKeys.add(usage);
    else if (!this.heldKeys.delete(usage)) return; // key went down before we had focus
    this.rel(P.key(usage, down));
  }

  releaseAll() {
    this.heldKeys.clear();
    this.motion = { dx: 0, dy: 0, abs: null };
    this.rel(P.releaseAll());
  }

  pollGamepads() {
    if (!navigator.getGamepads) return;
    const now = performance.now();
    for (const pad of navigator.getGamepads()) {
      if (!pad || !pad.connected || pad.index >= MAX_PADS) continue;
      const i = pad.index;
      const s = toXInput(pad);
      if ((!sameState(s, this.pads[i]) || now - this.padSentAt[i] >= GAMEPAD_KEEPALIVE_MS) &&
          this.session.sendInput(CHANNELS.INPUT_RT, P.gamepad(i, s))) {
        this.pads[i] = s;
        this.padSentAt[i] = now;
      }
    }
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
