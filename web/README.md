# Browser client

Plain HTML, CSS and ES modules. No framework, no build step: the host program serves this folder as-is. Chromium-based browsers are the target (Keyboard Lock and raw pointer lock are Chromium-only).

Pairing and signaling: `docs/signaling.md` (transport). Data channel messages: [`docs/protocol.md`](../docs/protocol.md).

## Files

| File | What it does |
|---|---|
| `index.html`, `style.css` | Toolbar with a Settings dropdown, plus a full-size `<video>`, stats overlay and touch overlay |
| `manifest.webmanifest` | Lets a phone add the client to its home screen, opening fullscreen and landscape |
| `js/main.js` | UI wiring, hotkeys, fullscreen + Keyboard Lock, stats polling |
| `js/session.js` | Signaling + `RTCPeerConnection`, data channels, low playout delay |
| `js/signaling.js` | WebSocket signaling client |
| `js/pairing.js` | Pairing API client (`/api/session`, `/api/pair/*`) |
| `js/input.js` | Keyboard, mouse (pointer lock or absolute), touch and gamepad capture |
| `js/touch.js` | Touch gestures → mouse actions (trackpad and direct modes) and pinch zoom |
| `js/zoom.js` | Pinch zoom and pan of the stream in the page |
| `js/cursor.js` | The host pointer: CSS cursor in absolute mode, an overlay at the host position in relative mode |
| `js/softkeys.js` | On-screen keyboard: typed text → key presses, extra keys row |
| `js/protocol.js` | Binary input message encoder/decoder |
| `js/keymap.js` | `KeyboardEvent.code` → USB HID usage |
| `js/gamepad.js` | Gamepad API standard mapping → XInput state |
| `js/files.js` | Files the host shares (saving to disk or to downloads) and files sent to the host: acks, cancel |
| `js/stats.js` | Stats overlay numbers from `getStats()`, `requestVideoFrameCallback` and the host's `videoStats` |
| `js/settings.js` | Resolution, bitrate, fps and quality settings → the host's `VideoConfig`; kept in `localStorage` |
| `js/autotune.js` | Auto-tune (test): bitrate ladder, then encoder quality, keeping the best that streams cleanly |
| `dev/` | Dev server (pairing API + `/ws` relay) and a mock host page (canvas + tone over real WebRTC) |

## Using it

- First visit: request a pairing code and type the 6-digit code the host shows. After that the browser stays paired (cookie) until **Unpair**.
- Click **Connect**. Click the video to capture the mouse (relative mode); **Ctrl+Alt+Shift+Q** releases it.
- **Fullscreen** also turns on Keyboard Lock so Esc, Alt+Tab and the Windows key go to the host. Hold Esc to leave fullscreen.
- Hotkeys, all Ctrl+Alt+Shift +: **1–9** switch display, **S** stats, **F** fullscreen, **M** relative/absolute mouse, **Q** release mouse, **E** send Esc to the host.
- Esc: in a window the browser keeps Esc to free the mouse, so it never reaches the host. In fullscreen (the Fullscreen button or Ctrl+Alt+Shift+F, not F11) the keyboard is locked: Esc goes to the host and holding Esc leaves fullscreen. In a window, use Ctrl+Alt+Shift+E.
- The toolbar holds **Connect**, the status, **Fullscreen** and **Settings ▾**. Settings opens one dropdown, grouped into Display, Video, Input, Audio and Other (stats, unpair, hotkeys); a click outside it or Esc closes it.
- Stream settings (in the dropdown), remembered in this browser:
  - **Resolution**: *Match this screen* (default) streams at this screen's size in device pixels, fitted to the host display's shape and never larger than it. *Host native* is the display's own size; the others are fixed heights.
  - **Bitrate**: *Auto* picks from resolution and fps (20 Mb/s at 1080p60). It is a ceiling: the host follows the network's estimate below it.
  - **FPS**, and **Quality**: *Speed* (fastest encode, softest), *Balanced*, *Clarity* (slowest encode the frame rate allows, more room for fast motion).
  - **Match host resolution** switches the host display to the largest resolution that fits this screen. Off by default; the host must also be started with `--allow-resolution-change`, and it is undone on disconnect.
  - **Audio bitrate** and **Frame**: game audio quality. *Host setting* keeps what the host was started with (128 kb/s, 10 ms unless changed). 20 ms frames sound better at the same bitrate and arrive 10 ms later. Changes apply straight away.
  - **Mic** sends this browser's microphone to the host, which plays it into its virtual cable (see `docs/host-setup.md`). Off by default; the browser asks for mic permission the first time. Turning it on or off doesn't reconnect.
  - **Auto-tune (test)** tries bitrates from 10 to 150 Mb/s, then Clarity and Balanced, about 7 s each, and keeps the best that streams without freezes, drops, loss, or slow encode/decode, minus 15% headroom. Run it while something moves on screen. Click again to stop.
- **Files (n)** appears next to Settings when the host shares files. Each row has **Save**, which asks where to save and writes the file as it arrives (on phones the file goes to downloads, up to 512 MiB), and **Cancel** while it runs. Nothing downloads until you click Save. The list empties when the session ends.
- When the host lets the browser send it files, the Files panel also has **Send files…**, and files dropped on the page go too. They're sent one at a time; each row says the name the host saved it under.
- The stats overlay's last two lines are the host's side: encoder bitrate / ceiling, bandwidth estimate, encode time and keyframe requests.
- URL option: `?autoconnect` connects on load when paired.

## Phones and tablets

Chrome on Android is the target; iPhone and iPad Safari work with the limits below.

- The Settings dropdown spans a narrow screen and its controls are finger-sized. Buttons over the stream (top right) stay there in fullscreen: **Fit** (while zoomed), **Keyboard**, **Keys** and **Fullscreen**.
- **Mouse: Relative** works as a trackpad: drag moves the cursor, tap clicks, two-finger tap right-clicks, two-finger drag scrolls. To drag (move a window, select text), tap and then touch again straight away and drag, like a laptop trackpad; holding still for half a second then dragging also works.
- **Mouse: Absolute** is direct touch: the cursor goes where the finger is; tap clicks there, drag drags, hold right-clicks, two-finger tap right-clicks, two-finger drag scrolls.
- **Pinch** zooms into the stream (on the phone only; the host's screen doesn't change) and moving the pinch pans around. While zoomed in, a two-finger drag pans instead of scrolling, and in Relative mode the view follows the pointer. **Fit** (top right, shown while zoomed) zooms back out; pinching back in does too. Trackpad motion gets finer as you zoom in.
- **Keyboard** opens the phone's keyboard. What it types is sent as key presses on a US layout (the host's layout decides what they produce); characters a US keyboard can't type are skipped. Autocorrect is off, but some keyboards still send a word only when it's finished.
- **Keys** shows Esc, Tab, Ctrl, Alt, Shift, Win, arrows, Del, Backspace and Enter. Ctrl, Alt, Shift and Win stay down until the next other key (tap again to let go), so Ctrl+Esc is Ctrl then Esc.
- **Fullscreen** on Android also turns the phone to the host display's shape. iPhone Safari can't fullscreen a page, so the stream fills the window instead; adding the page to the home screen gets closest to fullscreen there. *Match this screen* sizes the stream for the phone turned sideways.
- If the browser blocks sound until the page is touched (iOS), the first touch on the stream starts it.
- Not yet: an on-screen gamepad. Bluetooth controllers work through the Gamepad API as on desktop.

## Developing without a host

```
cd web
npm install
npm run dev            # http://localhost:8080
```

Open `http://localhost:8080/dev/mock-host.html` in one tab, then `http://localhost:8080/` in another. Pair with the code shown on the mock host page (also printed in the terminal), then Connect. The mock host streams an animated canvas and a quiet tone, offers two "displays" at different resolutions, and logs every input message it decodes.

## Tests

```
npm test               # unit tests (node --test)
npm run test:e2e       # client ↔ mock host over real WebRTC in headless Chromium (Playwright)
WS_HOST_BIN=../target/debug/ws-host npm run test:host   # file transfer against the real host (cargo build -p ws-host)
```

The headless Chromium bundled with Playwright has no H.264, so the e2e run negotiates VP8; real Chrome picks H.264.
