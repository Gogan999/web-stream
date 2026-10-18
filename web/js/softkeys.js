// On-screen keyboard for touch devices. Phone keyboards mostly don't report real key codes
// (Android sends keyCode 229 for nearly everything), so typed text is read from a hidden textarea
// and turned back into key presses on a US layout. Named keys that do arrive as real key events
// (Enter, Backspace, arrows on some keyboards) are left to input.js.

import { HID } from './keymap.js';

const SHIFT = HID.ShiftLeft;

// Character -> { usage, shift } on a US layout, which is what the host's scan codes assume.
const CHARS = (() => {
  const m = new Map();
  for (let i = 0; i < 26; i++) {
    const usage = HID['Key' + String.fromCharCode(65 + i)];
    m.set(String.fromCharCode(97 + i), { usage, shift: false });
    m.set(String.fromCharCode(65 + i), { usage, shift: true });
  }
  const pairs = [
    ['1', '!', 'Digit1'], ['2', '@', 'Digit2'], ['3', '#', 'Digit3'], ['4', '$', 'Digit4'],
    ['5', '%', 'Digit5'], ['6', '^', 'Digit6'], ['7', '&', 'Digit7'], ['8', '*', 'Digit8'],
    ['9', '(', 'Digit9'], ['0', ')', 'Digit0'], ['-', '_', 'Minus'], ['=', '+', 'Equal'],
    ['[', '{', 'BracketLeft'], [']', '}', 'BracketRight'], ['\\', '|', 'Backslash'],
    [';', ':', 'Semicolon'], ["'", '"', 'Quote'], ['`', '~', 'Backquote'], [',', '<', 'Comma'],
    ['.', '>', 'Period'], ['/', '?', 'Slash'],
  ];
  for (const [plain, shifted, code] of pairs) {
    m.set(plain, { usage: HID[code], shift: false });
    m.set(shifted, { usage: HID[code], shift: true });
  }
  m.set(' ', { usage: HID.Space, shift: false });
  m.set('\n', { usage: HID.Enter, shift: false });
  m.set('\t', { usage: HID.Tab, shift: false });
  // Phone keyboards substitute these; send what the user meant.
  for (const [ch, plain] of [['‘', "'"], ['’', "'"], ['“', '"'], ['”', '"'], [' ', ' ']]) m.set(ch, m.get(plain));
  return m;
})();

export function charKey(ch) {
  return CHARS.get(ch) ?? null;
}

// What changed between two textarea values, assuming edits happen at the end (the caret is
// kept there): how many characters were deleted, then what was typed.
export function diffText(before, after) {
  let p = 0;
  const max = Math.min(before.length, after.length);
  while (p < max && before[p] === after[p]) p++;
  return { backspaces: before.length - p, text: after.slice(p) };
}

// Kept in the textarea so Backspace always has something to delete (keyboards send nothing on
// an empty field). Zero-width, so it shows nothing and isn't a word for autocorrect.
export const SENTINEL = '​'.repeat(16);

// Key presses for a diff. send(usage, down); `shiftHeld` is true when a sticky Shift is already down.
export function typeDiff({ backspaces, text }, send, shiftHeld = false) {
  for (let i = 0; i < backspaces; i++) {
    send(HID.Backspace, true);
    send(HID.Backspace, false);
  }
  for (const ch of text) {
    const k = charKey(ch);
    if (!k) continue; // nothing on a US keyboard types it
    const shift = k.shift && !shiftHeld;
    if (shift) send(SHIFT, true);
    send(k.usage, true);
    send(k.usage, false);
    if (shift) send(SHIFT, false);
  }
}

const MODIFIERS = new Set([HID.ControlLeft, HID.AltLeft, HID.ShiftLeft, HID.MetaLeft]);

// The hidden textarea plus the row of extra keys (Esc, Tab, Ctrl, Alt, Win, arrows...).
// Ctrl, Alt, Shift and Win are sticky: one tap holds them until the next other key.
export class SoftKeyboard {
  constructor({ textarea, keys, send }) {
    this.ta = textarea;
    this.keys = keys; // container of <button data-key="Escape"> etc.
    this.send = send;
    this.sticky = new Set();
    this.last = SENTINEL;
    this.abort = new AbortController();
    const opt = { signal: this.abort.signal };
    this.ta.value = SENTINEL;
    this.ta.addEventListener('input', (e) => this.onInput(e), opt);
    this.ta.addEventListener('compositionend', () => this.onInput({ isComposing: false }), opt);
    this.ta.addEventListener('blur', () => this.releaseSticky(), opt);
    // Keep the caret at the end, where diffText expects edits.
    this.ta.addEventListener('select', () => this.caretToEnd(), opt);
    this.keys?.addEventListener('pointerdown', (e) => {
      const b = e.target.closest('button[data-key]');
      if (!b) return;
      e.preventDefault(); // keeps focus (and the phone keyboard) where it is
      this.press(b);
    }, opt);
  }

  get open() {
    return document.activeElement === this.ta;
  }

  show() {
    this.ta.value = SENTINEL;
    this.last = SENTINEL;
    this.ta.focus({ preventScroll: true });
    this.caretToEnd();
  }

  stop() {
    this.releaseSticky();
    this.abort.abort();
  }

  caretToEnd() {
    const n = this.ta.value.length;
    if (this.ta.selectionStart !== n || this.ta.selectionEnd !== n) this.ta.setSelectionRange(n, n);
  }

  onInput(e) {
    const value = this.ta.value;
    const d = diffText(this.last, value);
    this.last = value;
    if (d.backspaces || d.text) {
      typeDiff(d, this.send, this.sticky.has(SHIFT));
      this.releaseSticky();
    }
    // Mid-composition the keyboard owns the text; reset only once it is committed.
    if (!e.isComposing && value !== SENTINEL) {
      this.ta.value = SENTINEL;
      this.last = SENTINEL;
    }
    this.caretToEnd();
  }

  press(button) {
    const usage = HID[button.dataset.key];
    if (!usage) return;
    if (MODIFIERS.has(usage)) {
      if (this.sticky.delete(usage)) {
        this.send(usage, false);
        button.setAttribute('aria-pressed', 'false');
      } else {
        this.sticky.add(usage);
        this.send(usage, true);
        button.setAttribute('aria-pressed', 'true');
      }
      return;
    }
    this.send(usage, true);
    this.send(usage, false);
    this.releaseSticky();
  }

  // A key sent some other way (input.js forwards named keys with real codes) also ends a sticky press.
  keySent(usage, down) {
    if (!down && !MODIFIERS.has(usage) && this.sticky.size) this.releaseSticky();
  }

  // After one other key, sticky modifiers let go.
  releaseSticky() {
    for (const usage of this.sticky) this.send(usage, false);
    this.sticky.clear();
    for (const b of this.keys?.querySelectorAll('[aria-pressed="true"]') ?? []) b.setAttribute('aria-pressed', 'false');
  }
}
