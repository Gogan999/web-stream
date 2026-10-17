# Browser client

Plain HTML, CSS and ES modules. No framework, no build step: the host program serves this folder as-is. Chromium-based browsers are the target (Keyboard Lock and raw pointer lock are Chromium-only).

Wire protocol: [`docs/protocol.md`](../docs/protocol.md).

## Files

| File | What it does |
|---|---|
| `index.html`, `style.css` | Toolbar plus a full-size `<video>` and stats overlay |
| `js/main.js` | UI wiring, hotkeys, fullscreen + Keyboard Lock, stats polling |
| `js/session.js` | Signaling + `RTCPeerConnection`, data channels, low playout delay |
| `js/signaling.js` | WebSocket signaling client |
| `js/input.js` | Keyboard, mouse (pointer lock or absolute) and gamepad capture |
| `js/protocol.js` | Binary input message encoder/decoder |
| `js/keymap.js` | `KeyboardEvent.code` → USB HID usage |
| `js/gamepad.js` | Gamepad API standard mapping → XInput state |
| `js/stats.js` | Stats overlay numbers from `getStats()` and `requestVideoFrameCallback` |
| `dev/` | Dev server and a mock host page (canvas + tone over real WebRTC) |

## Using it

- Click **Connect**. Click the video to capture the mouse (relative mode); **Ctrl+Alt+Shift+Q** releases it.
- **Fullscreen** also turns on Keyboard Lock so Esc, Alt+Tab and the Windows key go to the host. Hold Esc to leave fullscreen.
- Hotkeys, all Ctrl+Alt+Shift +: **1–9** switch display, **S** stats, **F** fullscreen, **M** relative/absolute mouse, **Q** release mouse.
- URL options: `?signal=ws://host:port/signal` to point at another signaling endpoint (dev only; the CSP allows same-origin only), `?autoconnect`, `#token=...` for an auth token (kept in the fragment so it never hits server logs).

## Developing without a host

```
cd web
npm install
npm run dev            # http://localhost:8080
```

Open `http://localhost:8080/dev/mock-host.html` in one tab, then `http://localhost:8080/` in another and click Connect. The mock host streams an animated canvas and a quiet tone, offers two "displays" at different resolutions, and logs every input message it decodes.

## Tests

```
npm test               # unit tests (node --test)
npm run test:e2e       # client ↔ mock host over real WebRTC in headless Chromium (Playwright)
```

The headless Chromium bundled with Playwright has no H.264, so the e2e run negotiates VP8; real Chrome picks H.264.
