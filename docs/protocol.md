# Client ↔ host wire protocol

What the browser client in `web/` speaks. Control-channel JSON follows `ws-proto` (`crates/proto`: `type`-tagged, camelCase). Agreed between the browser client, WebRTC transport and host input threads; change both sides together.

## Pairing, signaling and channels

Defined by the WebRTC transport thread in `docs/signaling.md`, which is authoritative. The client implements it as written:

- Pairing via `GET /api/session`, `POST /api/pair/start`, `POST /api/pair/confirm` and `POST /api/unpair`, using an HttpOnly `ws_device` cookie. A refused WebSocket upgrade sends the user back to the pairing screen.
- WebSocket at `/ws`, same origin. The host sends `welcome {ice_servers}` and then `offer {sdp}`, and the browser replies `answer {sdp}` and `candidate {candidate,sdpMid,sdpMLineIndex}`. Once the connection is up and `control` is open, the browser sends `handoff` and the host closes the socket; the session then runs on the peer connection alone, and a `bye` comes over `control`. When ICE fails, the browser sends `restart` (up to 3 times) if the socket is still open, or opens a new socket (also up to 3 times) after a handoff, and answers the fresh offer on a new peer connection. `bye {reason}` and `error {code,message}` are shown to the user.
- The host opens the data channels. The client uses all four:

| Label | Options | Carries |
|---|---|---|
| `input-motion` | unordered, `maxRetransmits: 0` | mouse motion, gamepad state |
| `input` | ordered, reliable | keys, mouse buttons, wheel, gamepad disconnect, release-all |
| `control` | ordered, reliable, JSON text | `ClientControl` / `HostControl` |
| `file` | ordered, reliable, binary | bytes of a shared file (see File transfer) |

The client sets `jitterBufferTarget = 0` on video receivers. It always answers audio `sendrecv`; the mic comes and goes with `replaceTrack` on that transceiver (no track: nothing is sent), alongside `{"type":"setMicEnabled","enabled":true|false}` on `control`, which the host refuses with `{"type":"micUnavailable","message":"…"}` when it runs with `--no-mic` or can't open its mic device; the client then stops sending and unticks Mic. The host's mic is off at the start of every session until the client turns it on.

## Input messages (`input`, `input-motion`)

One binary frame per message, little-endian, first byte is the type. The host input thread implements this in `ws-proto::input`.

| Type | Name | Layout | Size | Channel |
|---|---|---|---|---|
| `0x01` | mouse relative | `u8 type, i16 dx, i16 dy` | 5 | input-motion |
| `0x02` | mouse absolute | `u8 type, u16 x, u16 y` (0..65535 across the active display) | 5 | input-motion (input before a touch click) |
| `0x03` | mouse button | `u8 type, u8 button, u8 down` (0 left, 1 middle, 2 right, 3 back, 4 forward; down 1/0) | 3 | input |
| `0x04` | wheel | `u8 type, i16 dx, i16 dy` (120 = one notch; +dy away from the user, +dx right, as Windows) | 5 | input |
| `0x05` | key | `u8 type, u16 usage, u8 down` (USB HID usage, Keyboard page 0x07, physical position) | 4 | input |
| `0x06` | gamepad state | `u8 type, u8 index, u16 buttons, i16 lx, i16 ly, i16 rx, i16 ry, u8 lt, u8 rt` | 14 | input-motion |
| `0x07` | gamepad disconnect | `u8 type, u8 index` | 2 | input |
| `0x08` | release all | `u8 type` | 1 | input |

- Motion is never queued: when `input-motion` has more than 1 KiB buffered the client holds relative deltas and sends their sum on the next free send; absolute keeps only the latest point.
- Touch in direct (absolute) mode sends the position where a click or drag starts as a `mouse absolute` on `input`, ahead of the button, so a lost or held motion packet can't move the click. The host decodes both channels the same way.
- Gamepad `buttons` are `XINPUT_GAMEPAD_*` flags (DPAD_UP 0x0001 … Y 0x8000, plus 0x0400 for Guide). Sticks use XInput's sign (+y up). Index 0–3. Full state is sent on change and every 100 ms as a keepalive; the first state for an index plugs in a virtual pad, disconnect unplugs it.
- `release all` is sent on window blur, tab hide, pointer-lock loss and disconnect; the host releases every key and button it holds for this client.
- Key repeat: repeated `keydown`s are forwarded as further key-down messages.
- Client hotkeys (Ctrl+Alt+Shift + 1–9, S, F, M, Q, E) are handled in the browser and never forwarded.

## Control messages (`control`)

JSON text, `ws-proto` `ClientControl` / `HostControl`. The client uses:

| Direction | Message |
|---|---|
| browser → host | `{"type":"listDisplays"}` when the channel opens; the host answers with `displays` and then `displayChanged` for what is streaming now |
| host → browser | `{"type":"displays","displays":[{"id":0,"name":"…","x":0,"y":0,"width":2560,"height":1440,"refreshMhz":143998,"primary":true}],"active":0}` |
| browser → host | `{"type":"selectDisplay","id":1}` |
| host → browser | `{"type":"displayChanged","id":1,"config":{"codec":"h264","width":1920,"height":1080,"fps":60,"bitrateKbps":20000,"quality":"balanced"}}` |
| browser → host | `{"type":"setVideoConfig","config":{"codec":"h264","width":2560,"height":1440,"fps":60,"bitrateKbps":30000,"quality":"clarity"}}`. A bitrate-only change applies in place; anything else rebuilds the encoder (at most once a second; a change that comes sooner is applied when the second is up). `quality` is `speed`, `balanced` (default) or `clarity`. `bitrateKbps` is the ceiling: the encoder follows the bandwidth estimate below it. |
| browser → host | `{"type":"setHostResolution","width":2560,"height":1600}` switches the streamed display to the largest mode that fits; `{"type":"restoreHostResolution"}` undoes it. Refused with an `error` unless the host runs with `--allow-resolution-change`; undone on disconnect. The host replies with a fresh `displays`. |
| browser → host | `{"type":"setAudioConfig","bitrateKbps":192,"frameMs":20}` sets game audio quality; `null` keeps the host's own (`--audio-kbps`, `--audio-frame-ms`). Bitrate is clamped to 32–510; frames are 10 or 20 ms. Sent when the channel opens and on every change; the host goes back to its own setting when the browser leaves. |
| host → browser | `{"type":"videoStats","encoderKbps":18000,"maxKbps":20000,"bweKbps":21500,"fps":60.0,"sentKbps":17200,"encodeMs":3.1,"keyframeRequests":0,"loss":0.0}` about once a second while video runs |
| host → browser | `{"type":"bye","reason":"replaced"}` (or `host_shutdown`, `revoked`) after a handoff, just before the host closes the peer connection; the same reasons as signaling's `bye`. |
| host → browser | `{"type":"rumble","index":0,"strong":0.0,"weak":0.0}` (0–1; sent on change, 0/0 stops). Proposed addition to `HostControl`. |
| host → browser | `{"type":"cursorShape","kind":"ew-resize","id":3,"image":{"width":32,"height":32,"hotX":15,"hotY":15,"rgba":"<base64>"}}` when the stream starts and whenever the host pointer changes shape. `kind` is a CSS cursor keyword (`default`, `pointer`, `text`, `wait`, `progress`, `crosshair`, `move`, `ew-resize`, `ns-resize`, `nwse-resize`, `nesw-resize`, `not-allowed`, `help`), `none` when the pointer is hidden, or `custom`. `id` names the host's image of the pointer for the session (always set for `custom`); `image` (RGBA, straight alpha, top row first, at most 128×128) comes only the first time an `id` is used, so the browser caches it by id (the host's id table and the browser's cache both hold 256). Images bigger than 128 px are scaled down to fit. |
| browser → host | `{"type":"setCursorPositions","enabled":true}` when the channel opens and when the mouse mode changes: `true` in relative mode, where the browser draws the pointer. Off until asked. |
| host → browser | `{"type":"cursorPosition","x":812,"y":440}`: the pointer's hotspot in host pixels from the streamed display's top-left (outside the display when it is on another one). Sent on change while the pointer shows and positions are enabled; only the newest matters (the host keeps just the latest and skips any it can't write). |

A display switch changes resolution mid-stream; the client handles that without renegotiation.

## File transfer (`control` and `file`)

The host user shares files from the tray app (**Send file...** or dropping files on its window) or the console (`send <path>`). Only those can be sent: the browser asks by the id the host gave, never by path. Offers last for the session; a new connection starts with none.

| Direction | Message |
|---|---|
| host → browser | `{"type":"fileOffer","id":6684867476655281,"name":"report.pdf","size":2202009}`. `id` is random, below 2^53; `name` is a bare file name, which the browser cleans again before suggesting it. |
| browser → host | `{"type":"requestFile","id":…}` asks for the bytes, from offset 0. Asking again starts over. An id not on offer gets `fileFailed`. |
| host → browser | bytes on `file`, below |
| browser → host | `{"type":"fileAck","id":…,"received":262144}` after storing every 256 KiB, and at the end. The host keeps at most 1 MiB in flight past the last ack, and counts the file as sent when `received` is its size. |
| browser → host | `{"type":"cancelFile","id":…}` stops the transfer; the file stays on offer. |
| host → browser | `{"type":"fileFailed","id":…,"message":"…"}` the host couldn't send it (read error, not on offer). |
| host → browser | `{"type":"fileWithdrawn","id":…}` the host user stopped sharing it. |

Each binary message on `file` is a 16-byte header, the id then the offset of its first byte (both big-endian u64), followed by at most 16 KiB of the file. One file is sent at a time; requests queue in order. The browser keeps a chunk only if its offset is where the file left off, which drops bytes still in flight from a cancelled try. There is no "done" message: the control and file channels aren't ordered with each other, so the browser finishes when it has `size` bytes.

File data isn't part of the video's bandwidth estimate, so while video streams the host paces it to a quarter of that estimate (at least 1 Mb/s), leaving the game stream its room. Without video it goes as fast as the connection allows.

The host opens each file when it is shared and reads only that handle, so renaming or replacing the file later can't change what is sent. On Windows it is opened with read sharing only: other programs can read it but not change or delete it while it is on offer, and a file another program is writing can't be shared.

The browser saves with Chrome's save dialog, writing to disk as the bytes arrive. Without that API (Chrome on Android) it collects the file in memory, up to 512 MiB, and hands it to the browser's downloads.

### Browser to host

Off unless the host runs with `--allow-upload` (the tray app's "let the browser send files to this PC"). Files go to `--upload-dir`, by default `Downloads\web-stream` in the host user's profile.

| Direction | Message |
|---|---|
| host → browser | `{"type":"uploads","enabled":true,"maxSize":4294967296}` when the control channel opens. The browser shows **Send files…** and takes dropped files only when enabled. |
| browser → host | `{"type":"uploadFile","id":1,"name":"save.dat","size":123456}`. `id` is the browser's own, new for each file. One file at a time: the next is offered after this one is saved or failed. |
| host → browser | `{"type":"uploadAccepted","id":1}`, then the browser sends the bytes on `file` as above (same header), at most 1 MiB past the host's last ack. |
| host → browser | `{"type":"uploadAck","id":1,"received":262144}` every 256 KiB written, and at the end. |
| host → browser | `{"type":"uploadSaved","id":1,"name":"save (2).dat"}`: the name it was saved under. |
| host → browser | `{"type":"uploadFailed","id":1,"message":"…"}`: refused (off, too big, another file still coming) or broken off. Nothing is kept. |
| browser → host | `{"type":"cancelUpload","id":1}` stops it; the host deletes what it has. |

The browser's name is only a suggestion. The host keeps the bare name, replaces what Windows would read as a path, a stream (`:`) or a wildcard, prefixes device names (`CON`, `COM1`…), drops trailing dots and spaces, and never overwrites: a taken name becomes `name (2).ext`. Bytes go to `name.part`, renamed when complete; a cancel, a gap in the offsets, too many bytes, a write error or a disconnect deletes it. On Windows the saved file gets a Mark of the Web (`Zone.Identifier`, internet zone), so SmartScreen and Office treat it as a download. The host never opens what it receives.

The video carries no pointer. In absolute mouse mode the browser sets the video's CSS `cursor` from `cursorShape` (its own cursor for standard kinds, the host image for `custom`), so it moves with no added latency. In relative mode pointer lock hides the browser's pointer, so it draws the host image over the video at `cursorPosition`.
