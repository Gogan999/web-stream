// On-screen keyboard for touch devices.

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
  for (const [ch, plain] of [['\u2018', "'"], ['\u2019', "'"], ['\u201c', '"'], ['\u201d', '"'], ['\u00a0', ' ']]) m.set(ch, m.get(plain));
  return m;
})();

export function charKey(ch) {
  return CHARS.get(ch) ?? null;
}

// What changed between two textarea values, assuming edits happen at the end: how many characters
// were deleted, then what was typed.
export function diffText(before, after) {
  let p = 0;
  const max = Math.min(before.length, after.length);
  while (p < max && before[p] === after[p]) p++;
  return { backspaces: before.length - p, text: after.slice(p) };
}

// Kept in the textarea so Backspace always has something to delete.
export const SENTINEL = '\u200b'.repeat(16);

// Key presses for a diff.
export function typeDiff({ backspaces, text }, send, shiftHeld = false, sendText = null) {
  for (let i = 0; i < backspaces; i++) {
    send(HID.Backspace, true);
    send(HID.Backspace, false);
  }
  let run = '';
  const flush = () => {
    if (run) sendText?.(run);
    run = '';
  };
  for (const ch of text) {
    const k = charKey(ch);
    if (!k) {
      if (sendText && !/[\p{Cc}\u200b-\u200d\ufeff]/u.test(ch)) run += ch;
      continue;
    }
    flush();
    const shift = k.shift && !shiftHeld;
    if (shift) send(SHIFT, true);
    send(k.usage, true);
    send(k.usage, false);
    if (shift) send(SHIFT, false);
  }
  flush();
}

const MODIFIERS = new Set([HID.ControlLeft, HID.AltLeft, HID.ShiftLeft, HID.MetaLeft]);

// The hidden textarea plus the row of extra keys.
export class SoftKeyboard {
  // sendText(text) types Unicode text on the host; onPaste(text) gets text pasted into the field,
  // which is then not typed here: the owner decides.
  constructor({ textarea, keys, send, sendText = null, onPaste = null }) {
    this.ta = textarea;
    this.keys = keys;
    this.send = send;
    this.sendText = sendText;
    this.onPaste = onPaste;
    this.sticky = new Set();
    this.last = SENTINEL;
    this.abort = new AbortController();
    const opt = { signal: this.abort.signal };
    this.ta.value = SENTINEL;
    this.ta.addEventListener('input', (e) => this.onInput(e), opt);
    this.ta.addEventListener('compositionend', () => this.onInput({ isComposing: false }), opt);
    this.ta.addEventListener('blur', () => this.releaseSticky(), opt);
    this.ta.addEventListener('paste', (e) => {
      const text = e.clipboardData?.getData('text/plain');
      if (!text || !this.onPaste) return;
      e.preventDefault();
      this.onPaste(text);
    }, opt);
    this.ta.addEventListener('select', () => this.caretToEnd(), opt);
    this.keys?.addEventListener('pointerdown', (e) => {
      const b = e.target.closest('button[data-key], button[data-chord]');
      if (!b) return;
      e.preventDefault();
      if (b.dataset.chord) this.chord(b.dataset.chord);
      else this.press(b);
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
      typeDiff(d, this.send, this.sticky.has(SHIFT), this.sendText);
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

  // A shortcut such as 'ctrl+c': modifiers down, the key pressed, modifiers up.
  chord(spec) {
    const names = spec.split('+');
    const code = names.pop();
    const mods = names.map((m) => HID[{ ctrl: 'ControlLeft', alt: 'AltLeft', shift: 'ShiftLeft', win: 'MetaLeft' }[m]]);
    const key = HID['Key' + code.toUpperCase()] ?? HID[code];
    if (!key || mods.some((m) => !m)) return;
    const held = mods.filter((m) => this.sticky.has(m));
    for (const m of mods) if (!held.includes(m)) this.send(m, true);
    this.send(key, true);
    this.send(key, false);
    for (const m of mods) if (!held.includes(m)) this.send(m, false);
    this.releaseSticky();
  }

  // A key sent some other way also ends a sticky press.
  keySent(usage, down) {
    if (!down && !MODIFIERS.has(usage) && this.sticky.size) this.releaseSticky();
  }

  // The host already let go of everything (RELEASE_ALL): just show the keys as up.
  forget() {
    this.sticky.clear();
    for (const b of this.keys?.querySelectorAll('[aria-pressed="true"]') ?? []) b.setAttribute('aria-pressed', 'false');
  }

  // After one other key, sticky modifiers let go.
  releaseSticky() {
    for (const usage of this.sticky) this.send(usage, false);
    this.forget();
  }
}
