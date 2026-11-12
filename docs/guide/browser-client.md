# Using the browser client

The host serves the web page itself, so there is nothing to install on the
device you connect from. Chromium-based browsers (Chrome, Edge) on desktop
and Android are the main target; Safari on iPhone and iPad works with the
limits noted under [Phones and tablets](#phones-and-tablets).

## Connecting

- **First visit:** scan the host app's QR code (see
  [Getting started](getting-started.md)), or press **Pair a device** on the
  host, request a pairing code on the page and type the 6-digit code the
  host app shows. The browser then stays paired until you click **Unpair**.
- A paired browser **connects by itself** when the page opens (**Settings >
  Host > Connect automatically**, on by default), so the home-screen icon
  opens straight into the stream. With it off, click **Connect**.
  The browser may warn about the certificate the first time; that is
  expected and explained in Getting started.

Not connected, a large **Connect** button sits in the middle of the page.
Connected, the toolbar holds **Connect/Disconnect** and the status, then:
a **display** switch (only when the host has two or more displays), **Mouse**
(Game / Desktop; Trackpad / Direct on a touch screen), **Mic** (on/off),
**Ctrl+Alt+Del**, **Files** (only when files are offered or accepted),
**Fullscreen** and **Settings ▾**. Settings is one flat dropdown of the
things you change now and then; everything you set once is under its folded
**Advanced** section (remembered). Click outside the dropdown or press Esc
to close it.

## Mouse and keyboard

- Click the video to capture the mouse (relative mode).
  **Ctrl+Alt+Shift+Q** releases it.
- **Fullscreen** also turns on Keyboard Lock, so Esc, Alt+Tab and the
  Windows key go to the host. Hold Esc to leave fullscreen. Use the
  Fullscreen button or Ctrl+Alt+Shift+F, not F11, which doesn't lock the
  keyboard.
- In a window the browser keeps Esc for itself (to free the mouse), so it
  never reaches the host. Use **Ctrl+Alt+Shift+E** to send Esc.
- **Ctrl+Alt+Del** in the toolbar (shown only when the host runs as
  a [Windows service](windows-service.md#ctrlaltdel)) sends it to the host.

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

Settings are remembered in this browser. **Reset to defaults** (Advanced)
puts them all back. The host adapts the stream to the connection; these
choices are the limits and the preference it works within.

In Settings:

- **Priority:** *Smooth* (fast encode, lowest delay, softest picture),
  *Balanced* (default), *Sharp* (slowest encode the frame rate allows,
  more detail in fast motion) or *Source* (for reading and working: every
  frame is coded as close to the desktop as the encoder allows, and the
  frame rate, up to Max FPS, drops to stay within the bitrate, so a still
  screen stays sharp and fast motion drops frames instead of going soft.
  Choosing it also sets host-native resolution, 60 fps, Auto resolution
  floor Off and Auto bitrate). Replaces the old *Quality*; a saved Speed,
  Balanced or Clarity becomes Smooth, Balanced or Sharp, and a saved
  *Source* stays Source.
- **Max resolution:** *Match this screen* (default) streams at this
  screen's size in device pixels, fitted to the host display's shape and
  never larger than it. *Host native* is the display's own size; the others
  are fixed heights. On a slow connection the host may send less.
- **Max FPS:** the highest frame rate to ask for.
- **Second monitor** streams another host display next to the first, each
  at its own full detail, as a second video. It needs about as much
  bandwidth again. *Pop out* moves it to a window of its own, which you can
  put on another screen of yours (double-click it for fullscreen; the
  browser may ask you to allow pop-ups first). Mouse, wheel and keys work in
  both. With *Mouse: Desktop*, moving across from one video to the other
  crosses between the host's monitors; in Game mode the host pointer simply
  moves across them.
- **Mouse** and **Mic** (also on the toolbar). **Mic** sends this browser's
  microphone to the host. Off by default; the browser asks for permission
  the first time, and turning it on or off doesn't reconnect. The host needs
  a virtual microphone driver (see
  [Microphone](audio-and-gamepads.md#microphone)).
- **Show stats** toggles the overlay.

In Advanced:

- **Codec:** overrides the host's codec for this browser. The host falls
  back to H.264 if either side can't handle the choice.
- **Max bitrate:** *Auto* picks from resolution and frame rate (20 Mb/s at
  1080p60). It is a ceiling: the host follows the network's measured
  bandwidth below it.
- **Auto resolution floor:** how far the host may shrink the picture while
  the connection is too slow (On = as low as needed, 75%, 50%, Off).
- **Keyframe size**, **Skip dropped frames**, **Match host resolution**
  (switches the host display to the largest resolution that fits this
  screen, undone on disconnect; off by default; the host must also allow
  it in its settings).
- **Audio bitrate** and **Audio frame:** game audio quality. *Host setting*
  keeps the host's own (128 kb/s and 10 ms unless changed). 20 ms frames
  sound better at the same bitrate and arrive 10 ms later. Changes apply
  immediately.
- **Stats level**, and the shortcut list.
- **Auto-tune (test)** tries bitrates from 10 to 150 Mb/s, then Sharp and
  Balanced, about 7 seconds each, and uses the best that streams without
  freezes, drops, loss or slow encoding or decoding, minus 15% headroom.
  The result lasts for this connection only; press **Keep these** to save it
  as Max bitrate and Priority. Run it while something moves on screen. Click
  again to stop. The stats overlay and auto-tune judge health by the same
  limits (`web/js/health.js`).
- **Update host** appears when the host allows updates from the browser. See
  [Building from source](../dev/building.md#updating-from-the-browser).
- **Unpair this browser** and **Reset to defaults**.

## Clipboard

Text you copy on the host appears on this device's clipboard, and text you
copy here is sent to the host, while connected. It is on by default, shown
as **Clipboard** in the bar and **Share clipboard** in Settings; it
disappears if the host turned it off (`--no-clipboard`) or its clipboard
can't be opened (see [Windows service](windows-service.md#clipboard)).
Text only, up to 256 KiB; files go through **Files**.

- **Chrome, Edge (computer):** press Ctrl+V as usual. The first time, the
  browser asks to let the page read the clipboard; allow it. This device's
  clipboard goes to the host first and then Ctrl+V is pressed there, so the
  paste matches what you copied here. Copy on the host and it is on this
  device's clipboard straight away (the page must be focused).
- **iPhone, iPad (Safari), and when a browser refuses a write:** the bar
  button reads **Copy from PC** when the host has copied something this
  device could not store yet. Tap it, or just tap the stream, to store it.
  To send the other way, tap **Clipboard** (iOS shows its own **Paste**
  bubble), or use **Paste** over the stream on a touch screen: it sends
  this device's clipboard and presses Ctrl+V on the host. If the clipboard
  can't be shared, Paste types the text instead.
- Long-press **Paste** in the on-screen keyboard also works: the text goes
  to the host's clipboard and is typed.

Characters that have no key on a US keyboard (accents, symbols, other
scripts) typed on the phone are sent as Unicode text and appear as typed.

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
- **Mouse: Trackpad** (Game) works as a trackpad: drag moves the cursor, tap
  clicks, two-finger tap right-clicks, two-finger drag scrolls. To drag
  (move a window, select text), tap and then touch again straight away and
  drag, like a laptop trackpad; holding still for half a second and then
  dragging also works.
- **Mouse: Direct** (Desktop) is direct touch: the cursor goes where your finger
  is. Tap clicks there, drag drags, hold or two-finger tap right-clicks,
  two-finger drag scrolls. A three-finger tap middle-clicks in both modes.
- **Pinch** zooms into the stream on the phone (the host's screen doesn't
  change), and moving the pinch pans. A two-finger drag still scrolls on the
  host; turn on **Pan** (shown while zoomed) to make it pan the view
  instead. In Trackpad mode the view follows the pointer. **Fit**, or
  pinching back out, returns to the full view. Trackpad motion gets finer
  as you zoom in.
- **Keyboard** opens the phone's keyboard. Text is sent as key presses on a
  US layout (the host's layout decides what they produce); characters a US
  keyboard can't type are skipped. Some phone keyboards only send a word
  once it is finished.
- **Keys** shows Esc, Tab, Ctrl, Alt, Shift, Win, arrows, Del, Backspace,
  Enter, Ctrl+C, Ctrl+V, Ctrl+Z and Ctrl+Alt+Del, and a scrolling row of F1 to F12. Ctrl, Alt, Shift and Win stay held until the next key (tap
  again to release), so Ctrl+Esc is Ctrl, then Esc.
- **Fullscreen** on Android also rotates the phone to the host display's
  shape. iPhone Safari can't make a page fullscreen, so the stream fills the
  window instead; adding the page to the home screen gets closest.
- If the browser blocks sound until the page is touched (iOS), the first
  touch on the stream starts it.
- Bluetooth controllers work through the Gamepad API as on desktop. There is
  no on-screen gamepad yet.
