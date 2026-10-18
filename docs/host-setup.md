# Host setup (Windows)

## Running the host

```
cargo build --release -p ws-host
target\release\ws-host.exe --pair
```

`ws-host` (or `ws-host serve`) starts everything: the HTTPS server with the
web client, pairing and signaling on TCP 8443, WebRTC media on UDP 8443 and
the ICE-TCP fallback on TCP 8444. It serves the web client from `web\` next
to the executable or in the working directory (`--web-root` to override) and
keeps its certificate and paired browsers in `%LOCALAPPDATA%\web-stream`.

The console takes a few commands: `pair` opens pairing for two minutes (the
code to type in the browser is printed here), `drop` ends the session,
`unpair-all` forgets every paired browser, `quit` exits. `--pair` opens
pairing at startup. `ws-host --help` lists the options: ports, public IPs for
port forwarding, a real certificate, the starting display and stream
settings, and turning the mic or game audio off.

Capture, game audio, the mic and input start when a browser connects and
stop when it leaves; dropping the session releases every held key and
unplugs the virtual gamepads. The encoder follows the connection's bandwidth
estimate, up to `--kbps`.

Two drivers are needed on the host for gamepads and the microphone. Keyboard,
mouse and game audio need nothing extra.

## Gamepads: ViGEmBus

Virtual Xbox 360 and DualShock 4 controllers are created through the ViGEmBus
driver. Install the latest signed release from
https://github.com/nefarius/ViGEmBus/releases and reboot if asked.

Without it the host still streams; gamepad messages are dropped and the log
says "ViGEmBus driver is not installed". ViGEmBus is no longer developed but
its last release works on Windows 10 and 11.

Browser gamepads appear as Xbox 360 controllers by default (`GamepadKind`
in `ws-input` switches to DualShock 4). Rumble is supported on Xbox 360 pads.

## Microphone: a virtual audio cable

The browser mic is played into a render device whose other end shows up in
Windows as a microphone. The default is VB-Audio Virtual Cable
(https://vb-audio.com/Cable/, donationware; not bundled because its licence
does not allow redistribution without permission).

1. Install VB-CABLE and reboot.
2. Leave **CABLE Input** as a playback device but do *not* make it the
   default output; game audio capture follows the default output, and making
   the cable the default would loop the mic back to the client.
3. In each game or chat app, pick **CABLE Output** as the microphone.

Another cable works too: set `MicConfig::device` to part of its playback
device name. If no device matches, the error lists the playback devices the
host can see.

## Game audio

Loopback capture follows the default output device, including when it changes
mid-session. Any device format is accepted; it is converted to 48 kHz stereo
and sent as 10 ms Opus packets at 128 kb/s.

## Elevated windows and the secure desktop

Input is injected as the logged-in user, so Windows blocks it into windows
running as administrator and on the UAC and lock screens. The planned SYSTEM
helper covers those; until then, run games without elevation.
