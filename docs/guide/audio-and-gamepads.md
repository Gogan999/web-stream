# Microphone, game audio and controllers

Keyboard, mouse and game audio work out of the box. Controllers and the
browser's microphone each need a driver on the host PC. The installer
offers both.

## Game audio

The host captures whatever plays on the PC's default output device, and
follows it if the default changes mid-session. To capture a specific
output instead, choose it in the host app's settings (`--audio-device`).

Audio is sent as Opus at 128 kb/s in 10 ms frames by default. **Audio
quality** in the host app's settings changes this (32 to 510 kb/s,
`--audio-kbps`; 10 or 20 ms frames, `--audio-frame-ms`), and **Audio** in
the browser's settings overrides it for that browser. 20 ms frames sound
noticeably better at the same bitrate and arrive 10 ms later.

## Controllers (ViGEmBus)

Browser gamepads appear on the PC as virtual Xbox 360 controllers, or as
DualShock 4 controllers if **Gamepads** is set to DS4 in the host app's
settings (`--gamepad ds4`). Rumble works on Xbox 360 pads.

This uses the ViGEmBus driver. The installer includes the signed ViGEmBus
1.22 driver and installs it unless it is already present. To install it by
hand, use the latest signed release from
[github.com/nefarius/ViGEmBus](https://github.com/nefarius/ViGEmBus/releases)
and reboot if asked. ViGEmBus is no longer developed, but its last release
works on Windows 10 and 11.

Without the driver the host still streams; controller input is ignored and
the log says "ViGEmBus driver is not installed".

## Microphone

Windows doesn't let a program add a microphone by itself; it takes an audio
driver. The host plays the browser's microphone into a playback device whose
other end Windows shows as a microphone. With the default setting
(`--mic-device auto`) the host uses the first of these it finds:

| Driver | The host plays into | Games and chat apps use |
|---|---|---|
| VB-CABLE | **CABLE Input** | **CABLE Output** |
| Steam Streaming Microphone | **Speakers (Steam Streaming Microphone)** | **Microphone (Steam Streaming Microphone)** |

To use the mic, tick **Mic** in the browser's settings. The host plays
nothing until a browser asks.

### Steam Streaming Microphone (no extra download)

Steam ships a signed virtual microphone driver for Remote Play. If Steam is
installed, the installer offers to install it from Steam's folder. If you
have used Steam Remote Play on this PC it may already be there: look for
"Steam Streaming Microphone" under **Sound, video and game controllers** in
Device Manager.

After installing:

1. Check that your speakers or headphones are still the default output in
   Sound settings.
2. In each game or chat app, pick **Microphone (Steam Streaming
   Microphone)** as the microphone.
3. If **Speakers (Steam Streaming Microphone)** is disabled in the Sound
   control panel, enable it.

### VB-CABLE

[VB-Audio Virtual Cable](https://vb-audio.com/Cable/) is donationware. Its
licence doesn't allow bundling it with another program, so install it
yourself:

1. Install VB-CABLE and reboot.
2. Leave **CABLE Input** as a playback device, but do **not** make it the
   default output.
3. In each game or chat app, pick **CABLE Output** as the microphone.

### Notes

- A mic cable must never be the default output: game audio follows the
  default output, so the browser would hear its own mic. The host logs a
  warning if it is.
- Another virtual cable works too: enter part of its playback device name
  as **Browser mic** in the host app's settings (`--mic-device`); the
  settings window lists the devices.
- If no device matches, the browser shows an error listing the playback
  devices the host can see, and unticks **Mic**.
- `--no-mic` turns the mic off on the host whatever the browser asks.
