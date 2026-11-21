# Benchmarking against Steam Link

Goal: a repeatable side-by-side of web-stream and Steam Link on the same PC, phone and Wi-Fi, so we can see
*where* we lose (capture/encode, network/pacing, playout/jitter buffer, decode, display) instead of guessing.

## What is measured, and how

| What | web-stream | Steam Link | Same method for both |
|---|---|---|---|
| Glass-to-glass latency | stats + camera | camera | **Camera test** (below) |
| Input latency (tap -> change on phone) | camera | camera | **Flash test** |
| Frame pacing / dropped frames | stats `fps`, `dropped`, `freezes`, frame gaps (`?diag`) | overlay + log | Camera: counter/flip bar |
| Codec, resolution, bitrate | stats overlay | overlay + `streaming_log.txt` | Read, don't assume |
| Latency by stage | host `encode`, network RTT/2, `jitter buf`, `decode`, `recv->display` | overlay `Capture/Encode/Network/Decode/Display` | Compare stage by stage |

Rules for a fair run: same phone, same Wi-Fi spot (5 GHz), same display on the PC at the same resolution/refresh,
nothing else running, same content. Run each app 3 times, 30 s each, alternating (A B A B A B) so Wi-Fi drift
doesn't favour one.

## Test content

Open `clock.html` on the PC, full screen (F11), on the streamed display. It shows a ms counter that changes every
frame, a frame counter, an alternating green/magenta square (a repeated or skipped frame is visible), a bar that
steps 7 vw per frame (judder shows as uneven steps) and a flash on click/key (input test). Also run a real game
scene both ways for a final subjective check; the clock is for numbers.

Steam Link streams the desktop only from the Steam menu (Desktop); the stream may be capped at the game
resolution otherwise. Note which was used.

## Camera test (glass-to-glass)

Needs a second camera with slow motion (another phone at 240 fps). Put the PC monitor and the phone side by side
so both are in frame, record 10 s at 240 fps. For each of ~10 stills, `PC counter - phone counter` is the latency.
`frames.sh recording.mov` extracts the stills. Upload the video to the project thread and Claude will read it.

Flash test: tap the phone (or press a key on the phone keyboard) with the camera seeing the finger and both
screens; count frames from the touch to the phone screen going white.

## Reading numbers from each app

- web-stream: Settings > stats level "Debug" (Ctrl+Alt+Shift+L cycles). Record fps, bitrate, `decode`, `jitter buf`,
  `recv->display`, host `encode`, RTT, dropped/freezes, codec/decoder.
- Steam Link: Settings > Streaming > enable "Show performance information" (overlay while streaming). On the PC,
  `Steam\logs\streaming_log.txt` holds codec, bitrate, resolution and per-session stats.

## Decomposing WebRTC delay

web-stream already minimises the browser jitter buffer (`jitterBufferTarget = 0`). The remaining places WebRTC can
add delay: jitter buffer if the browser ignores the hint (check `jitter buf` in stats), decoder queue (hardware vs
software), render path (`recv->display`), host pacer/BWE headroom, and large keyframes. A custom protocol
(WebTransport + WebCodecs) only changes the playout/jitter-buffer portion; measure that portion first.
