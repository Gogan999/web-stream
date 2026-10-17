# web-stream

Low-latency remote access to a Windows PC from a browser: a Rust host agent
captures and hardware-encodes the desktop and streams it over WebRTC, with
input, audio both ways, display switching and host-to-client file transfer.
A self-hosted signaling server and TURN relay connect the two sides.

## Layout

| Path | Crate | What it does |
| --- | --- | --- |
| `crates/proto` | `ws-proto` | Shared interface types: frames, displays, input events, control and signaling messages |
| `crates/capture` | `ws-capture` | DXGI desktop duplication + hardware encode (NVENC, then AMF/QSV) |
| `crates/transport` | `ws-transport` | WebRTC peer connection, media tracks, data channels |
| `crates/input` | `ws-input` | Keyboard/mouse injection on the host |
| `crates/audio` | `ws-audio` | Loopback capture + Opus, browser mic to virtual mic |
| `crates/signaling` | `ws-signaling` (bin) | WebSocket signaling and TURN credentials |
| `crates/host` | `ws-host` (bin) | Host agent wiring the crates together |
| `web/` | | Browser client, plain and fast |

Data flow on the host: `capture` emits `EncodedFrame`s and `audio` emits
`AudioPacket`s into bounded channels that `transport` drains; `transport`
hands `InputEvent`s to `input` and `ClientControl` messages to `host`.
Late frames are dropped, never queued.

## Build

```
cargo build --workspace
cargo test --workspace
```

The host crates build on any OS but only capture/encode on Windows.
