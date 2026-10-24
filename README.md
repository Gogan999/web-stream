# web-stream

Low-latency remote access to a Windows PC from a browser: a Rust host agent
captures and hardware-encodes the desktop and streams it over WebRTC, with
input, audio both ways, display switching and host-to-client file transfer.
The host also serves the web client, pairing and signaling itself over one
HTTPS port (see `docs/signaling.md`).

## Layout

| Path | Crate | What it does |
| --- | --- | --- |
| `crates/proto` | `ws-proto` | Shared interface types: frames, displays, input events, control, pairing and signaling messages |
| `crates/capture` | `ws-capture` | DXGI desktop duplication + hardware encode (NVENC, then AMF/QSV) |
| `crates/transport` | `ws-transport` | WebRTC peer connection, media tracks, data channels |
| `crates/signaling` | `ws-signaling` | HTTPS server in the host: web client, device pairing, `/ws` signaling |
| `crates/input` | `ws-input` | Keyboard/mouse injection on the host |
| `crates/audio` | `ws-audio` | Loopback capture + Opus, browser mic to virtual mic |
| `crates/host` | `ws-host` (lib) | Host agent wiring the crates together, and the Windows service; `examples/` has a headless test host and a capture test |
| `crates/gui` | `ws-host-gui` (bin) | The host program: tray app with status/settings window, which also runs the Windows service |
| `web/` | | Browser client, plain and fast |

Data flow on the host: `capture` emits `EncodedFrame`s and `audio` emits
`AudioPacket`s into bounded channels that `transport` drains; `transport`
hands `InputEvent`s to `input` and `ClientControl` messages to `host`.
Late frames are dropped, never queued.

## Install

Friends hosting on their own PC: download `web-stream-setup-X.Y.Z.exe` from
[Releases](https://github.com/Gogan999/web-stream/releases) and follow
[the friend guide](docs/friend-guide.md). Releases are built and published
by CI (`docs/releasing.md`); the installer source is in `installer/`.

## Build

```
cargo build --workspace
cargo test --workspace
```

The host crates build on any OS but only capture/encode on Windows. To run
the host see `docs/host-setup.md`. Elsewhere the tray app, or the headless
test host (`cargo run -p ws-host --example test-host -- --pair`), still
serves the web client, pairing, signaling and data channels (input is
logged, not injected), which is handy for testing the browser side.
