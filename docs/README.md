# Documentation

## Using web-stream

| Page | What it covers |
|---|---|
| [Getting started](guide/getting-started.md) | Installing the host, first-run setup and pairing a phone |
| [Connecting from outside your home](guide/remote-access.md) | Port forwarding, testing the ports, a trusted certificate |
| [Cloudflare Tunnel](guide/cloudflare-tunnel.md) | Serving the page through Cloudflare: named and quick tunnels, dashboard setup |
| [Using the browser client](guide/browser-client.md) | Controls, hotkeys, stream settings, files, phones and tablets |
| [The host app](guide/host-app.md) | The tray app's window and settings, sharing files, where data is kept |
| [Microphone, game audio and controllers](guide/audio-and-gamepads.md) | Drivers for the virtual mic and gamepads, audio quality |
| [Sign-in screen and administrator programs](guide/windows-service.md) | The Windows service, Ctrl+Alt+Del, its security model |
| [Troubleshooting](guide/troubleshooting.md) | Common problems and fixes |

## Developing web-stream

| Page | What it covers |
|---|---|
| [Architecture](dev/architecture.md) | Crates, data flow, the wire contract |
| [Building from source](dev/building.md) | Requirements, source installs and updates, test tools, the installer |
| [Host options reference](dev/host-options.md) | Every command-line flag |
| [Signaling, pairing and transport](dev/signaling.md) | Ports, TLS, pairing API, `/ws` signaling, data channels |
| [Client and host wire protocol](dev/protocol.md) | Input messages, control JSON, file transfer |
| [Browser client development](../web/README.md) | Client files, dev server, mock host, tests |
| [Diagnostic logging](dev/diagnostics.md) | Tracing freezes and keyframes on both sides |
| [Releasing](dev/releasing.md) | Release pipeline and how to cut a release |
