# Testing the media path without a GPU

Capture, NVENC, WASAPI and input injection only run on Windows with a GPU, which CI and cloud
sandboxes don't have. Three pieces let the rest of the media path (pacing, dropping late frames,
keyframe recovery, display switching, Opus, the transport, the browser) be tested anywhere, with
numbers instead of "it connected".

## Synthetic source

`test-host --synthetic[=N]`, built with `--features synthetic`:

```text
cargo build -p ws-host --example test-host --features synthetic
./target/debug/examples/test-host --synthetic=3 --pair
```

- **Video.** N fake displays (1 to 4, the first primary; different sizes and positions). Each frame
  is drawn in memory and encoded as H.264 by OpenH264, through the same pacing and backpressure
  code as the Windows pipeline (`crates/capture/src/pacing.rs`). The picture moves every frame
  and carries a stamp: two rows of 32 black or white cells at the top holding the capture time,
  display, frame index and a CRC (`crates/capture/src/synthetic/stamp.rs`). Display switching,
  resolution changes (`--allow-resolution-change`), bitrate changes and keyframe requests work.
  H.264 only, and a dropped frame is repaired with a keyframe (OpenH264 can't invalidate
  references like NVENC, so that path isn't covered).
- **Audio** (Linux and macOS). Game audio is a 100 ms 1 kHz beep at the top of every Unix second,
  Opus-encoded like the real capture. The browser mic plays into a meter that logs
  `synthetic mic: peak … dBFS` once a second.

What it can't tell you: DXGI, NVENC behaviour, WASAPI devices, SendInput. Those need the real
host on Windows.

## Measuring from the browser

`web/test/lib/probe.js` runs in the client page. For every frame the `<video>` presents it reads
the stamp, so each frame's index, display and age are known; it hears the beeps, and keeps the peer
connections for `getStats`. `web/test/lib/metrics.js` turns that into frame rate, lost frames
(gaps in the index, minus frames the browser presented but the probe missed), latency
percentiles, freezes (a gap of 200 ms or more), audio latency (how far past the whole second a beep
is heard) and the A/V offset.

```text
cd web && WS_HOST_BIN=../target/debug/examples/test-host npm run test:media
```

Needs Google Chrome (Playwright's Chromium has no H.264); `CHROME_PATH` picks a binary.
`MEDIA_REPORT=<file>` writes the figures as JSON. The limits in `web/test/media/*.test.js` are
loose on purpose: CI encodes in software on 4 cores, so they catch a broken path, not a few
milliseconds. Latency on loopback is capture-to-display with no real network in it.

## Bad networks

`web/test/lib/impair.js` is a UDP proxy between the browser and the host's media port (the probe
rewrites the offer's candidates to point at it). It adds delay and jitter, drops packets, limits
bandwidth with a router-like queue (towards the browser), or cuts the link, deterministically
(seeded). `web/test/media/network.test.js` runs the stream through 2% loss with 60 ms round trip, a
2.5 Mb/s link for an 8 Mb/s stream, and a 3 s outage. It runs as an ordinary user, so it works
in any CI and needs no `tc netem`.

Unit tests for the stamp, metrics and proxy are in `web/test/unit` (the stamp test checks the same
vector as the Rust one, so the two sides can't drift).

## Real hardware (one command)

The synthetic source can't tell you about DXGI, NVENC, WASAPI or SendInput. `web/test/hardware/run.mjs`
checks those on a real PC, with the real host, in a minute or two:

```text
cargo build --release -p ws-host --example test-host
cd web && npm ci
node test/hardware/run.mjs --displays "0,0;2560,0" --gpu-load
```

It opens a full-screen Chrome on each display (`source.html`: the stamp, moving noise, and a beep at
the top of every Unix second), streams them with the host, and measures from a headless Chrome
client: fps, latency, freezes, bitrate, audio latency and A/V offset, then display switching, then
input (mouse and keys sent from the client must reach the page on the streamed display), then the
mic (the client's fake microphone must come out of the virtual cable; needs VB-CABLE), and with
`--gpu-load` all of it again while `load.html` keeps the GPU busy. It writes a JSON report and
exits 1 if any check found a problem. Run it with `--help`-style options in the file's header.

It takes over the displays and moves the mouse pointer while it runs, so the PC can't be in use.
Needs an unlocked interactive session (a disconnected one has nothing to capture), Google Chrome,
and an audio output device. The first run may raise a Windows Firewall prompt for `test-host.exe`;
Cancel is fine, and answering it before the input phase matters, because the prompt would swallow
the injected input. The host is bound to every LAN address, as in real use: a host bound to
loopback only can't answer the LAN addresses Chrome offers once it has media permission.
