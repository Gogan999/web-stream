# Client ↔ host wire protocol

What the browser client in `web/` speaks. Control-channel JSON follows `ws-proto` (`crates/proto`: `type`-tagged, camelCase). Agreed between the browser client, WebRTC transport and host input threads; change both sides together.

## Pairing, signaling and channels

Defined by the WebRTC transport thread in `docs/signaling.md`, which is authoritative. The client implements it as written:

- Pairing via `GET /api/session`, `POST /api/pair/start`, `POST /api/pair/confirm` and `POST /api/unpair`, using an HttpOnly `ws_device` cookie. A refused WebSocket upgrade sends the user back to the pairing screen.
- WebSocket at `/ws`, same origin. The host sends `welcome {ice_servers}` and then `offer {sdp}`, and the browser replies `answer {sdp}` and `candidate {candidate,sdpMid,sdpMLineIndex}`. When ICE fails, the browser sends `restart` (up to 3 times) and answers the fresh offer on a new peer connection. `bye {reason}` and `error {code,message}` are shown to the user.
- The host opens the data channels. The client uses three of them:

| Label | Options | Carries |
|---|---|---|
| `input-motion` | unordered, `maxRetransmits: 0` | mouse motion, gamepad state |
| `input` | ordered, reliable | keys, mouse buttons, wheel, gamepad disconnect, release-all |
| `control` | ordered, reliable, JSON text | `ClientControl` / `HostControl` |

The client sets `jitterBufferTarget = 0` on video receivers. It always answers audio `sendrecv`; the mic comes and goes with `replaceTrack` on that transceiver (no track: nothing is sent), alongside `{"type":"setMicEnabled","enabled":true|false}` on `control`, which the host refuses with an `error` when it runs with `--no-mic`.

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
- Client hotkeys (Ctrl+Alt+Shift + 1–9, S, F, M, Q) are handled in the browser and never forwarded.

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
| host → browser | `{"type":"videoStats","encoderKbps":18000,"maxKbps":20000,"bweKbps":21500,"fps":60.0,"sentKbps":17200,"encodeMs":3.1,"keyframeRequests":0,"loss":0.0}` about once a second while video runs |
| host → browser | `{"type":"rumble","index":0,"strong":0.0,"weak":0.0}` (0–1; sent on change, 0/0 stops). Proposed addition to `HostControl`. |

A display switch changes resolution mid-stream; the client handles that without renegotiation.
