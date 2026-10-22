# Diagnostic logging (temporary)

For chasing freezes and unexpected keyframes. Off by default; costs nothing
when off. Everything lives in `crates/diag` (host) and `web/js/diag.js`
(browser) plus `diag!` / `diag?.` call sites, so it can be removed with a grep.

## Turning it on

Host, any of:

- From a terminal: `$env:WS_DIAG = 1; .\target\release\ws-host-gui.exe`
  (quit the running tray app first)
- Tray app or the web-stream service: create an empty file `ws-diag.on` next
  to the exe that runs the stream (`target\release\` for the tray app built by
  `update.ps1`, `C:\Program Files\web-stream\` for the service), then restart
  it. Delete the file to turn it off.

The log is `ws-diag-<unix seconds>.log` next to that exe (the temp folder if
that isn't writable). The normal log says `WS_DIAG on: diagnostic log at ...`.

Browser: open the page with `?diag` (for example `https://host:8443/?diag`),
connect, reproduce, then Settings > Save diag log.

## A useful run

1. Turn both on and connect. Leave the desktop still for ~5 s.
2. Drag a window around for ~20 s, until a freeze or keyframe shows.
3. Stop dragging, wait ~5 s, save the browser log, quit or disconnect.
4. Send both files (the host one is usually a few MB a minute).

## Host lines

`<unix ms> <host ms> <kind> <fields>`

| kind | what it says |
|---|---|
| `dxgi` | Desktop Duplication delivered an image: gap since the last, `acc` updates folded into it (>1 means DWM produced frames we missed), convert time |
| `frame` | One encode: `idr`, `why` (requested by the transport or the start/a hand-off drop), `intra`/`repair` after invalidation, `repeat` (idle re-encode), size, `held_ms` capture to encode start, `enc_ms`, `out` (`sent`, `full_*` = the hand-off channel to the transport was full) |
| `handoff` | The frame reached the host's forwarding task, `since_enc_ms` after encode |
| `send` | The transport loop wrote it: result (`Sent`, `Late`, `Broken`, `Skipped`), `since_enc_ms`, `batch` frames taken at once, the pacer queue's age/bytes/packets before the write |
| `late_drop`, `broken_drop`, `video_queue_drop` | A frame did not go out, and why |
| `frames_lost`, `keyframe_needed` | What the transport asked the encoder for, and the cause (`browser`, `repair_timed_out`, `keyframe_lost`, `connected`, ...; `suppressed` = within 300 ms of the last ask) |
| `browser_keyframe_request` | PLI/FIR from the browser |
| `cap_cmd` | The capture thread got a keyframe, recover (invalidated or not) or bitrate command |
| `bwe`, `encoder_bitrate`, `transport_stats`, `video_egress` | Bandwidth estimate, encoder rate changes, and once-a-second totals (NACKs, PLIs, loss, RTT) |
| `loop_timer_late` | The transport's timer fired more than 4 ms late (pool busy or coarse timer) |
| `media_slow_event` | The media task took over 5 ms on one event, holding up those behind it |
| `send_drop` | The socket refused a packet |

## Browser lines

`<unix ms> <page ms> <kind> <json>`: `video` (inbound counters every 250 ms:
frames, drops, keyframes decoded, freezes, NACK/PLI counts, loss), `decoder`
(on change; a switch to a software decoder is a clue), `frame_gap` (no new
frame shown for over 50 ms), `pair` (RTT), `host_stats` (the host's
once-a-second `videoStats`).

Line the two up by wall-clock time; the machines' clocks may differ by a
little, so anchor on a keyframe (`frame idr=true` on the host,
`keyFramesDecoded` going up in the browser).
