# Client ↔ host wire protocol

What the browser client in `web/` speaks. JSON message names follow `ws-proto` (`crates/proto`: `type`-tagged, camelCase). Agreed between the browser client, WebRTC transport and host input threads; change both sides together.

## Pairing (HTTP, same origin)

Cookie-based, defined by the transport thread. All POST bodies are JSON with `Content-Type: application/json`; the device credential is an HttpOnly cookie the page never sees.

| Request | Body | Responses |
|---|---|---|
| `GET /api/session` | | `200 {"paired":true,"device":"…"}` or `401 {"paired":false}` |
| `POST /api/pair/start` | `{"device_name":"Chrome on Windows"}` | `200 {"pairing_id":"…","expires_in":120}`, host shows a 6-digit code; `429 {"error":"rate_limited"}` |
| `POST /api/pair/confirm` | `{"pairing_id":"…","code":"123456"}` | `200 {"paired":true}` + cookie; `403 {"error":"bad_code","attempts_left":n}`; `410 {"error":"expired"}`; `429` |
| `POST /api/unpair` | `{}` | `200`, clears the cookie |

## Signaling

WebSocket at `/signal`, same origin as the page (`ws:`/`wss:` matching it). The cookie authenticates the upgrade; a 401 means re-pair. The browser is the offerer.

| Direction | Message |
|---|---|
| host → browser | `{"type":"iceServers","servers":[{"urls":["turn:…"],"username":"…","credential":"…"}]}` sent first; the browser offers without servers if it hasn't arrived within 1 s |
| browser → host | `{"type":"offer","sdp":"…"}` (also re-sent with `iceRestart` after ICE failure, up to 3 times) |
| host → browser | `{"type":"answer","sdp":"…"}` |
| both | `{"type":"iceCandidate","candidate":"candidate:…","sdpMid":"0","sdpMLineIndex":0}` |
| host → browser | `{"type":"error","message":"…"}` |
| host → browser | `{"type":"bye","reason":"replaced"\|"hostShutdown"\|"revoked"}` before closing |

### Offer contents

- Video `recvonly`, H.264 first via `setCodecPreferences`. Host sends the `playout-delay` header extension at 0/0.
- Audio `sendrecv` with no track until the mic is turned on (`replaceTrack`, no renegotiation).
- Every receiver gets `jitterBufferTarget = 0`.
- Data channels created by the browser (in-band):

| Label | Options | Carries |
|---|---|---|
| `input-rt` | `ordered: false, maxRetransmits: 0` | mouse motion, gamepad state |
| `input` | ordered, reliable | keys, mouse buttons, wheel, gamepad disconnect, release-all |
| `control` | ordered, reliable, JSON text | `ClientControl` / `HostControl` |
| `file` | ordered, reliable | host → client file transfer (format TBD) |

## Input messages (`input`, `input-rt`)

One binary frame per message, little-endian, first byte is the type. The host input thread implements this in `ws-proto::input`.

| Type | Name | Layout | Size | Channel |
|---|---|---|---|---|
| `0x01` | mouse relative | `u8 type, i16 dx, i16 dy` | 5 | input-rt |
| `0x02` | mouse absolute | `u8 type, u16 x, u16 y` (0..65535 across the active display) | 5 | input-rt |
| `0x03` | mouse button | `u8 type, u8 button, u8 down` (0 left, 1 middle, 2 right, 3 back, 4 forward; down 1/0) | 3 | input |
| `0x04` | wheel | `u8 type, i16 dx, i16 dy` (120 = one notch; +dy away from the user, +dx right, as Windows) | 5 | input |
| `0x05` | key | `u8 type, u16 usage, u8 down` (USB HID usage, Keyboard page 0x07, physical position) | 4 | input |
| `0x06` | gamepad state | `u8 type, u8 index, u16 buttons, i16 lx, i16 ly, i16 rx, i16 ry, u8 lt, u8 rt` | 14 | input-rt |
| `0x07` | gamepad disconnect | `u8 type, u8 index` | 2 | input |
| `0x08` | release all | `u8 type` | 1 | input |

- Motion is never queued: when `input-rt` has more than 1 KiB buffered the client holds relative deltas and sends their sum on the next free send; absolute keeps only the latest point.
- Gamepad `buttons` are `XINPUT_GAMEPAD_*` flags (DPAD_UP 0x0001 … Y 0x8000, plus 0x0400 for Guide). Sticks use XInput's sign (+y up). Index 0–3. Full state is sent on change and every 100 ms as a keepalive; the first state for an index plugs in a virtual pad, disconnect unplugs it.
- `release all` is sent on window blur, tab hide, pointer-lock loss and disconnect; the host releases every key and button it holds for this client.
- Key repeat: repeated `keydown`s are forwarded as further key-down messages.
- Client hotkeys (Ctrl+Alt+Shift + 1–9, S, F, M, Q) are handled in the browser and never forwarded.

## Control messages (`control`)

JSON text, `ws-proto` `ClientControl` / `HostControl`. The client uses:

| Direction | Message |
|---|---|
| browser → host | `{"type":"listDisplays"}` when the channel opens |
| host → browser | `{"type":"displays","displays":[{"id":0,"name":"…","x":0,"y":0,"width":2560,"height":1440,"refreshMhz":143998,"primary":true}],"active":0}` |
| browser → host | `{"type":"selectDisplay","id":1}` |
| host → browser | `{"type":"displayChanged","id":1,"config":{"codec":"h264","width":1920,"height":1080,"fps":60,"bitrateKbps":20000}}` |
| host → browser | `{"type":"rumble","index":0,"strong":0.0,"weak":0.0}` (0–1; sent on change, 0/0 stops). Proposed addition to `HostControl`. |

A display switch changes resolution mid-stream; the client handles that without renegotiation.
