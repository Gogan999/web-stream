# Releasing

A release is a tag `vX.Y.Z` (or `vX.Y.Z-rc.1`, published as a
pre-release) on a commit on `main`, with `version` in `Cargo.toml`'s
`[workspace.package]` set to `X.Y.Z`. Pushing the tag runs
`.github/workflows/release.yml`:

1. **check**: the tag's version matches `Cargo.toml` and the commit is on
   `main`; anything else stops here.
2. **build** (windows-latest, no build cache): the same fmt, clippy and test
   gates as CI with `--locked`, then `cargo build --release --locked -p
   ws-host-gui`, the zip
   `web-stream-host-X.Y.Z-windows-x64.zip`, the installer
   `web-stream-setup-X.Y.Z.exe` (Inno Setup, `installer/`), and the same
   silent install, start and uninstall test CI runs.
3. **publish** waits for approval if the `release` environment has a
   required reviewer. It writes `manifest.json` (version, time, each file's
   size and SHA-256, and the tag's message as release notes) and
   `SHA256SUMS.txt`, and creates the GitHub release with them and the files.

Release files are not signed, and nothing needs a key or a secret. The
hashes in `manifest.json` let the host's built-in updater check that a
download is complete and is the file the release lists; trust in the
release itself comes from GitHub (only the repository's maintainers can
publish one, over HTTPS). Windows code signing isn't used either: friends
click past SmartScreen (see [Getting started](../guide/getting-started.md)).

## Optional: approve each release

With nothing set up, the publish job runs as soon as the build is green
(GitHub makes the `release` environment on the first run). To approve each
release by hand instead: repository Settings > Environments > `release`
(or New environment `release`). Under **Deployment protection rules** tick
**Required reviewers** and add yourself, and leave **Prevent self-review**
unticked (you push the tags yourself). Under **Deployment branches and
tags**, choose **Selected branches and tags** and add the tag rule `v*`.

## Making a release

Set `version` in `Cargo.toml` to the full new version, pre-release suffix included (`1.0.0-rc.1`, not `1.0.0`), and merge that to `main`. The built app reports `Cargo.toml`'s version, and the updater compares it with the release's, so a release candidate must say `-rc.N` in the binary too.
Then either:

- **From the Actions tab** (no git needed): Actions > release > **Run
  workflow**, branch `main`, the version (`0.2.0`, or `0.2.0-rc.1` for a
  pre-release) and the release notes (what changed, in a few lines: this
  text is shown in the tray before updating). The tag `v0.2.0` is made at
  the built commit only after the build has passed, so a failed run leaves
  nothing behind; run it again once fixed.
- **With git**:

  ```
  git checkout main && git pull
  git tag -a v0.2.0 -m "What changed, in a few lines: this text is shown in the tray before updating"
  git push origin v0.2.0
  ```

  A failed run publishes nothing. Delete the tag, fix, and tag again.

If you set up approval: Actions > release > the run > **Review
deployments** > Approve, once build is green. A test of the pipeline:
release `0.1.0-rc.1` with the current version; it lands as a pre-release,
which the updater will ignore, and can be deleted afterwards (the release
and its tag).

## How the installed app updates

Code: `crates/gui/src/update_check.rs` (check, download, verify),
`crates/gui/src/win.rs` (hand-off), `installer/web-stream.iss` (`/RELAUNCH`).
Everything is started by a click in the tray app (Settings > Updates, or the
tray's **Check for updates**); there is no automatic update. An optional
"Look for a new version when the app starts" (off by default, saved in
`update-prefs.json` in the data folder) does one check at start and only
announces the result.

1. **Check:** HTTPS GET of
   `https://github.com/Gogan999/web-stream/releases/latest/download/manifest.json`
   (GitHub's `latest` skips pre-releases, and a pre-release version in the
   manifest is ignored anyway). `ureq` with rustls and the Windows
   certificate store, 10 s timeouts, `User-Agent: web-stream-host/<version>`.
   `version` is compared with the running version as semver, so `1.0.0`
   replaces `1.0.0-rc.1`. The manifest must have `product` =
   `web-stream-host` and list `web-stream-setup-<version>.exe` with a size
   and a 64-digit SHA-256; the app only ever downloads that exact name from
   `.../releases/download/v<version>/`.
2. **Download:** to `%LOCALAPPDATA%\web-stream\updates\` as `.part`; a
   size or SHA-256 mismatch deletes the file and shows an error. Old
   installers there are removed on the next download and at start.
3. **Hand-off:** the app stops the host, closes its `Local\web-stream-host-gui`
   mutex (the installer's `AppMutex` would otherwise refuse to run), and
   starts setup with `/SILENT /SUPPRESSMSGBOXES /NORESTART /RELAUNCH` through
   `ShellExecute runas`; setup shows the UAC prompt. The app then exits,
   leaving the router's UPnP forwards in place for the new copy. If the
   prompt is declined, the app takes the mutex back, starts hosting again
   and says "Update cancelled".
4. **Installer:** with `/RELAUNCH` the last `[Run]` entry starts the app as
   the user who clicked Update (`runasoriginaluser nowait`) and the usual
   "Open web-stream host" checkbox entry is skipped. The installer already
   reuses the previous task choices on a silent run, stops the Windows
   service before replacing files and runs `service update --quiet`.
5. **After:** the app compares the version in `last-version.txt` in the data
   folder with its own and shows "Updated to version X" once.

To test the whole path on a PC, release a pre-release first (`X.Y.Z-rc.1`),
install it by hand, then publish the final release and use the app's
**Check for updates**. `installer/test-install.ps1` checks the installer's
side in CI: a silent `/RELAUNCH` setup over an installed copy leaves the app
running, and a plain silent setup starts nothing. The manifest and download
checks have unit tests (`cargo test -p ws-host-gui update_check`); against
the live release: `cargo test -p ws-host-gui live_manifest -- --ignored`.

Release notes are the tag's message (the Actions input for a dispatched
release); the app shows them verbatim, so keep them to a few plain lines.
