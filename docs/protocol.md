# Client ↔ host wire protocol

Message formats on the data channels, owned jointly by the browser client and host input threads. The browser client in `web/` implements this as written; change both sides together.

## Signaling, pairing and channels

Defined by the WebRTC transport thread in `docs/signaling.md`. In short: the client pairs once through `/api/pair/*` (HttpOnly cookie), connects to `wss://<host>/ws`, receives `welcome` then an `offer` from the host and answers. The host opens the data channels; the client uses these three:

| Label | Options | Carries |
|---|---|---|
| `input-motion` | unordered, `maxRetransmits: 0` | mouse motion, gamepad state |
| `input` | ordered, reliable | keys, mouse buttons, wheel, gamepad disconnect, release-all |
| `control` | ordered, reliable, JSON text | display list/switch, rumble |

The client sets `jitterBufferTarget = 0` on every receiver and answers audio `recvonly` until the microphone is added.

## Input messages (`input`, `input-motion`)

One binary frame per message, little-endian, first byte is the type.

| Type | Name | Layout | Size | Channel |
|---|---|---|---|---|
| `0x01` | mouse relative | `u8 type, i16 dx, i16 dy` | 5 | input-motion |
| `0x02` | mouse absolute | `u8 type, u16 x, u16 y` (0..65535 across the active display) | 5 | input-motion |
| `0x03` | mouse button | `u8 type, u8 button, u8 down` (0 left, 1 middle, 2 right, 3 back, 4 forward; down 1/0) | 3 | input |
| `0x04` | wheel | `u8 type, i16 dx, i16 dy` (120 = one notch; +dy away from the user, +dx right, as Windows) | 5 | input |
| `0x05` | key | `u8 type, u16 usage, u8 down` (USB HID usage, Keyboard page 0x07, physical position) | 4 | input |
| `0x06` | gamepad state | `u8 type, u8 index, u16 buttons, i16 lx, i16 ly, i16 rx, i16 ry, u8 lt, u8 rt` | 14 | input-motion |
| `0x07` | gamepad disconnect | `u8 type, u8 index` | 2 | input |
| `0x08` | release all | `u8 type` | 1 | input |

- Gamepad `buttons` are `XINPUT_GAMEPAD_*` flags (DPAD_UP 0x0001 … Y 0x8000, plus 0x0400 for Guide), so the host can copy it into `XINPUT_GAMEPAD.wButtons`. Sticks use XInput's sign (+y up). Index is 0–3. Full state is sent on every change and again every 100 ms as a keepalive, because the channel is lossy.
- `release all` is sent on window blur, tab hide, pointer-lock loss and disconnect. The host must release every key and button it is holding for this client.
- Key repeat: repeated `keydown`s from the browser are forwarded as further key-down messages.
- Client hotkeys (Ctrl+Alt+Shift + 1–9, S, F, M, Q) are handled in the browser and never forwarded.

## Control messages (`control`)

JSON text.

| Direction | Message |
|---|---|
| host → client | `{"type":"displays","displays":[{"id":0,"name":"DELL U2720Q","width":3840,"height":2160,"refresh":60,"primary":true}],"active":0}`. Sent when the channel opens and again after every switch or display change. `id` is a number or string; the client echoes it back unchanged. |
| client → host | `{"type":"select-display","id":0}` |
| host → client | `{"type":"rumble","index":0,"strong":0.0-1.0,"weak":0.0-1.0}`. Sent on change; `0,0` stops. |

A display switch changes resolution mid-stream; the client handles that without renegotiation.
