# Using the browser client

The host serves the web page itself, so there is nothing to install on the
device you connect from. Chromium-based browsers (Chrome, Edge) on desktop
and Android are the main target; Safari on iPhone and iPad works with the
limits noted under [Phones and tablets](#phones-and-tablets).

## Connecting

- **First visit:** scan the host app's QR code (see
  [Getting started](getting-started.md)), or request a pairing code on the
  page and type the 6-digit code the host app shows. The browser then stays
  paired until you click **Unpair**.
- Click **Connect**. Add `?autoconnect` to the address to connect as soon
  as the page loads.

The toolbar holds **Connect**, the connection status, **Fullscreen** and
**Settings ▾**. Settings opens one dropdown, grouped into Display, Video,
Input, Audio and Other; click outside it or press Esc to close it.

## Mouse and keyboard

- Click the video to capture the mouse (relative mode).
  **Ctrl+Alt+Shift+Q** releases it.
- **Fullscreen** also turns on Keyboard Lock, so Esc, Alt+Tab and the
  Windows key go to the host. Hold Esc to leave fullscreen. Use the
  Fullscreen button or Ctrl+Alt+Shift+F, not F11, which doesn't lock the
  keyboard.
- In a window the browser keeps Esc for itself (to free the mouse), so it
  never reaches the host. Use **Ctrl+Alt+Shift+E** to send Esc.
- **Ctrl+Alt+Del** in the toolbar sends it to the host when the host runs as
  a [Windows service](windows-service.md#ctrlaltdel).

### Hotkeys

All hotkeys are **Ctrl+Alt+Shift** plus:

| Key | Action |
|---|---|
| 1–9 | Switch display |
| F | Fullscreen |
| M | Toggle relative and absolute mouse |
| Q | Release the mouse |
| E | Send Esc to the host |
| S | Show or hide stats |
| L | Change the stats level (and show stats) |

## Stream settings

Settings are remembered in this browser.

- **Resolution:** *Match this screen* (default) streams at this screen's
  size in device pixels, fitted to the host display's shape and never
  larger than it. *Host native* is the display's own size; the others are
  fixed heights.
- **Bitrate:** *Auto* picks from resolution and frame rate (20 Mb/s at
  1080p60). It is a ceiling: the host follows the network's measured
  bandwidth below it.
- **FPS** and **Quality:** *Speed* (fastest encode, softest picture),
  *Balanced*, or *Clarity* (slowest encode the frame rate allows, more
  detail in fast motion).
- **Codec:** overrides the host's codec for this browser. The host falls
  back to H.264 if either side can't handle the choice.
- **Match host resolution** switches the host display to the largest
  resolution that fits this screen, and is undone on disconnect. Off by
  default; the host must also allow it in its settings.
- **Audio bitrate** and **Frame:** game audio quality. *Host setting* keeps
  the host's own (128 kb/s and 10 ms unless changed). 20 ms frames sound
  better at the same bitrate and arrive 10 ms later. Changes apply
  immediately.
- **Mic** sends this browser's microphone to the host. Off by default; the
  browser asks for permission the first time, and turning it on or off
  doesn't reconnect. The host needs a virtual microphone driver (see
  [Microphone](audio-and-gamepads.md#microphone)).
- **Auto-tune (test)** tries bitrates from 10 to 150 Mb/s, then Clarity and
  Balanced, about 7 seconds each, and keeps the best that streams without
  freezes, drops, loss or slow encoding or decoding, minus 15% headroom.
  Run it while something moves on screen. Click again to stop.
- **Update host** (under Other) appears when the host allows updates from
  the browser. See [Building from source](../dev/building.md#updating-from-the-browser).

## Files

**Files (n)** appears next to Settings when the host shares files. Each row
has **Save**, which asks where to save and writes the file as it arrives
(on phones it goes to downloads, up to 512 MiB), and **Cancel** while it
runs. Nothing downloads until you click Save. The list empties when the
session ends.

When the host lets the browser send it files, the Files panel also has
**Send files…**, and files dropped on the page are sent too. They go one at
a time, and each row shows the name the host saved it under.

## Stats

**Stats** shows an overlay. **Stats level** chooses how much it shows:

- **Essential** (default): `OK`, or warnings in yellow or red, worst first:
  reduced resolution or bitrate, slow connection (round trip 80 ms or
  more), lossy connection (1% or more), TCP fallback in use, frozen
  picture, dropped frames, keyframe repairs, fewer frames shown than the
  host sends, slow decode or encode. Each stays up 4 seconds after it was
  last seen. Then resolution, frame rate, bitrate, round trip, route and
  estimated latency.
- **Detailed:** every browser figure, then the host's side (encoder bitrate
  and ceiling, bandwidth estimate, encode time, keyframe requests and why).
- **Debug:** adds the network route, the configuration the host runs,
  running totals and the host's raw statistics.

## Phones and tablets

Chrome on Android is the main target. iPhone and iPad Safari work with the
limits below.

- The Settings dropdown spans a narrow screen and its controls are
  finger-sized. Buttons over the stream (top right) stay available in
  fullscreen: **Fit** (while zoomed), **Keyboard**, **Keys** and
  **Fullscreen**.
- **Mouse: Relative** works as a trackpad: drag moves the cursor, tap
  clicks, two-finger tap right-clicks, two-finger drag scrolls. To drag
  (move a window, select text), tap and then touch again straight away and
  drag, like a laptop trackpad; holding still for half a second and then
  dragging also works.
- **Mouse: Absolute** is direct touch: the cursor goes where your finger
  is. Tap clicks there, drag drags, hold or two-finger tap right-clicks,
  two-finger drag scrolls.
- **Pinch** zooms into the stream on the phone (the host's screen doesn't
  change), and moving the pinch pans. A two-finger drag still scrolls on the
  host; turn on **Pan** (shown while zoomed) to make it pan the view
  instead. In Relative mode the view follows the pointer. **Fit**, or
  pinching back out, returns to the full view. Trackpad motion gets finer
  as you zoom in.
- **Keyboard** opens the phone's keyboard. Text is sent as key presses on a
  US layout (the host's layout decides what they produce); characters a US
  keyboard can't type are skipped. Some phone keyboards only send a word
  once it is finished.
- **Keys** shows Esc, Tab, Ctrl, Alt, Shift, Win, arrows, Del, Backspace
  and Enter. Ctrl, Alt, Shift and Win stay held until the next key (tap
  again to release), so Ctrl+Esc is Ctrl, then Esc.
- **Fullscreen** on Android also rotates the phone to the host display's
  shape. iPhone Safari can't make a page fullscreen, so the stream fills the
  window instead; adding the page to the home screen gets closest.
- If the browser blocks sound until the page is touched (iOS), the first
  touch on the stream starts it.
- Bluetooth controllers work through the Gamepad API as on desktop. There is
  no on-screen gamepad yet.
