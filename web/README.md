# Browser client

Plain HTML, CSS and ES modules. No framework, no build step: the host program serves this folder as-is. Chromium-based browsers are the target (Keyboard Lock and raw pointer lock are Chromium-only).

Pairing and signaling: [`docs/dev/signaling.md`](../docs/dev/signaling.md). Data channel messages: [`docs/dev/protocol.md`](../docs/dev/protocol.md).

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

How to use the client (controls, hotkeys, settings, phones and tablets) is in
[Using the browser client](../docs/guide/browser-client.md).

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
WS_HOST_BIN=../target/debug/examples/test-host npm run test:host   # file transfer against the real host (cargo build -p ws-host --example test-host)
```

The headless Chromium bundled with Playwright has no H.264, so the e2e run negotiates VP8; real Chrome picks H.264.
