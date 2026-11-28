# Architecture

web-stream has two parts: a Rust **host** on the Windows PC being streamed,
and a plain-JavaScript **browser client** that the host serves itself. There
is no separate server: the host serves the web page, handles pairing and
signaling over one HTTPS port, and is the WebRTC endpoint (ICE-lite), so no
STUN or TURN server is needed.

```
 Browser                                   Windows host
┌────────────────────┐   HTTPS 8443/tcp   ┌───────────────────────────────┐
│ web client         │◄──────────────────►│ signaling: web client, /api,  │
│  pairing, /ws      │                    │  /ws                          │
│                    │   WebRTC 8443/udp  │                               │
│ <video>, <audio>   │◄───────────────────│ transport ◄── capture (NVENC) │
│ input, mic, files  │───────────────────►│   │       ◄── audio (Opus)    │
└────────────────────┘ (ICE-TCP 8444/tcp) │   └──► input, audio (mic),    │
                                          │        host (control, files)  │
                                          └───────────────────────────────┘
```

## Crates

| Path | Crate | What it does |
|---|---|---|
| `crates/proto` | `ws-proto` | Shared interface types: frames, displays, input events, control, pairing and signaling messages. The source of truth for the wire contract |
| `crates/capture` | `ws-capture` | Screen capture (DXGI Desktop Duplication on Windows, KMS on Linux) and hardware encoding (NVENC, Media Foundation, VA-API) |
| `crates/nvenc-sys` | `ws-nvenc-sys` | Raw NVENC (NVIDIA Video Codec SDK 12.1) bindings, loaded at runtime |
| `crates/desktop` | `ws-desktop` | Following the input desktop (lock screen, UAC) so capture and input work there |
| `crates/transport` | `ws-transport` | WebRTC peer connection (str0m), media tracks, data channels, pacing and frame dropping |
| `crates/signaling` | `ws-signaling` | HTTPS server in the host: web client, device pairing, `/ws` signaling, certificates |
| `crates/input` | `ws-input` | Keyboard, mouse and virtual gamepad injection (SendInput and ViGEm on Windows, uinput on Linux) |
| `crates/audio` | `ws-audio` | Loopback capture and Opus encoding (WASAPI, PulseAudio/PipeWire); browser mic to a virtual microphone |
| `crates/host` | `ws-host` (lib, and the `web-stream` bin) | Wires the crates together; the Windows service and its process split; the headless Linux host (`src/linux/`: systemd service, settings page in `admin/`). `examples/` has a headless test host and a capture test |
| `crates/gui` | `ws-host-gui` (bin) | The host program: tray app with the status and settings window, which also runs the Windows service |
| `crates/diag` | `ws-diag` | Optional diagnostic logging (see [diagnostics.md](diagnostics.md)) |
| `web/` | | Browser client: HTML, CSS and ES modules, no build step |
| `installer/` | | Inno Setup installer, the one-line installers (`web-stream-install.ps1`, `web-stream-install.sh`) and their tests |
| `scripts/` | | Source install and update scripts for Windows |

## Data flow

On the host, `capture` emits `EncodedFrame`s and `audio` emits
`AudioPacket`s into bounded channels that `transport` drains. `transport`
hands `InputEvent`s to `input` and `ClientControl` messages to `host`.

Latency comes first: frames that arrive late are dropped, never queued.
Bounded channels drop rather than block, mouse motion and gamepad state go
over an unordered, unreliable data channel, and a lost frame is repaired
with an intra refresh rather than retransmitted late (see
[Frame dropping](signaling.md#frame-dropping)).

## Wire contract

- [signaling.md](signaling.md): ports, TLS, pairing, the `/ws` signaling
  exchange, data channels and frame dropping. Authoritative.
- [protocol.md](protocol.md): what goes over the data channels: binary
  input messages, control JSON and file transfer.

Signaling messages are snake_case; control messages are camelCase. Both are
defined in `ws-proto`, and the browser and the host change together.

## Windows service

Installed as a service, the host runs as two processes: the screen side
(capture, encoding, input, audio) as SYSTEM, and the network side (HTTPS,
signaling, the peer connection) as LocalService with no privileges. The
screen side accepts only what a browser could send. See
[Security](../guide/windows-service.md#security).

## Linux service

On Linux the `web-stream` binary runs as the systemd unit `web-stream.service`.
`service run` (root) is the media side; it starts `service network` as the
`web-stream` user with no capabilities, its stdin one end of a socket pair
carrying the same bridge as the Windows service, and fd 3 a second socket for
the settings page's requests, which the root side checks and saves before
restarting both. See [Linux host](../guide/linux.md#security).
