# Building from source

## Requirements

- Rust stable (`rust-toolchain.toml`).
- To capture and encode: Windows 10 or 11, an NVIDIA GPU, Visual Studio
  Build Tools with the C++ tools and Windows SDK, and CMake.
- For the browser client's tests: Node.js.

The workspace builds and its tests run on any OS. Capture, encoding, input
injection and audio only work on Windows; elsewhere the host still serves
the web client, pairing, signaling and data channels (input is logged, not
injected), which is enough to work on the browser side.

```
cargo build --workspace
cargo test --workspace
```

CI (`.github/workflows/`) runs `cargo fmt --check`, `cargo clippy` and the
tests on Windows and Linux, the browser client's tests, a WebRTC end-to-end
test in headless Chromium, and on Windows an install and uninstall test of
the installer.

## Setting up a Windows PC

`scripts\install.ps1` installs the build dependencies (Visual Studio Build
Tools, CMake, Rust) through winget and builds the host. Add
`-GamepadDriver` to install ViGEmBus and `-MicDriver` to install the Steam
Streaming Microphone (see
[Microphone, game audio and controllers](../guide/audio-and-gamepads.md)).

```
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

On a PC without the repository, this clones it into
`%USERPROFILE%\web-stream` first:

```
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gogan999/web-stream/main/scripts/install.ps1)))
```

The script adds **web-stream host** to the Start menu, pointing at
`target\release\ws-host-gui.exe`. Windows Firewall asks about it the first
time it listens; allow Private and Public networks.

The tray app (`ws-host-gui`) is the only host program. The old console host
(`ws-host.exe`) is retired, and `install.ps1` and `update.ps1` delete copies
left by earlier builds.

## Updating a source install

```
powershell -ExecutionPolicy Bypass -File scripts\update.ps1
```

It pulls `main` and builds while the host keeps running, then restarts the
tray app on the new build only if the build succeeded, so it works from a
streaming session (the browser reconnects after a few seconds). If the build
fails, the old build keeps running. The previous build is kept, and
`scripts\update.ps1 -Rollback` switches back to it.

It needs a clean git clone on `main` (a GitHub "Download ZIP" folder can't
be updated) and installs nothing new; run `install.ps1` again for new build
dependencies or drivers. It also updates the Windows service if one is
installed (one UAC prompt).

### Updating from the browser

With **let the browser update and restart this host** turned on in the tray
app's settings (`--allow-remote-update`), the browser's settings have
**Update host** under Other. It asks for confirmation and runs the same
`update.ps1` on the host. The stream keeps running during the build; once
it has built, the tray app starts the new build and exits, the stream drops
for a few seconds and the browser reconnects. The browser shows the result
if there was nothing to update or the build failed, and the script's output
is in `update.log` in `%LOCALAPPDATA%\web-stream`.

Only paired browsers can use it, it is off by default, and the browser
can't choose what is built: it always builds `main` from the clone the host
runs from. The Windows service can't restart itself, so it refuses this
option.

### Building only signed releases

Building `main` means trusting everyone who can merge to it, and the GitHub
account behind it. To build only what you have signed yourself, list your
SSH signing key in `.git\web-stream-allowed-signers` in the clone, in git's
[allowed signers](https://git-scm.com/docs/git-config#Documentation/git-config.txt-gpgsshallowedSignersFile)
format (for example `you@example.com ssh-ed25519 AAAA...`).

From then on `update.ps1`, from the PC or the browser, builds only the
newest tag named `host-*` on `main` whose SSH signature verifies against
that file. OpenPGP and X.509 signatures are refused, as is a tag object
whose own name differs from the `host-*` name it was pushed under. A
checkout that has moved past the newest release (a manual `git pull`) is
refused until it is reset to a signed release. To release a build, sign a
tag on `main` with that key and push it:

```
git tag -s host-2026-10-22 -m "host release"   # with gpg.format=ssh and user.signingkey set
git push origin host-2026-10-22
```

The file lives inside `.git`, so nothing pulled from GitHub can change it.

## Test tools

Two examples in `crates/host`:

- **test-host**, a headless host that takes `pair`, `drop`, `unpair-all`,
  `send <path>` and `quit` on stdin. The browser tests use it.

  ```
  cargo run -p ws-host --example test-host -- --pair
  ```

- **capture-test**, which encodes a display to a file and prints frame
  rate, drops and latency.

  ```
  cargo run --release -p ws-host --example capture-test -- --help
  ```

For the browser client's own dev server, mock host and tests, see
[`web/README.md`](../../web/README.md). For tracing freezes and unexpected
keyframes, see [Diagnostic logging](diagnostics.md).

## The installer

`installer/` holds the Inno Setup script (`web-stream.iss`), its build
script and the install test; releases build it in CI (see
[Releasing](releasing.md)). It shows one screen of options:

| Option | Default | What it does |
|---|---|---|
| Gamepad driver (ViGEmBus) | on | Installs the bundled, signed ViGEmBus 1.22 driver, unless it is already present |
| Microphone driver | on | Shown only when Steam is installed: installs Steam's signed Streaming Microphone from Steam's folder |
| Allow use at the login screen | off | Installs the [Windows service](../guide/windows-service.md) |
| Start with Windows | on | Starts the tray app when you sign in |

It installs into `C:\Program Files\web-stream`, adds **web-stream host** to
the Start menu, and adds Windows Firewall rules that let the tray app (and
the service's copy, `ws-host.exe`) accept connections on any network
profile. Settings, paired browsers and logs stay in
`%LOCALAPPDATA%\web-stream`. Running a newer installer over an old one keeps
them and updates the service in place.

Uninstalling removes the service, the firewall rules, the router forwards
UPnP made (if the router answers within 15 seconds; otherwise their lease
runs out within the hour) and the autostart entry, and asks whether to
delete the settings and paired browsers. The autostart entry and settings it
removes are those of the account the uninstaller runs as, which is the
signed-in user when they are an administrator; other accounts' stay. The
drivers stay, since other programs use them. Setup and uninstall only check
the signed-in session for a running tray app; quit it in other users'
sessions first.

Silent installs take the usual Inno Setup options, for example
`/VERYSILENT /TASKS="gamepad,autostart"`. A silent uninstall keeps the
settings unless given `/PURGE`.
