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
hashes in `manifest.json` let the host's built-in updater (planned) check that a
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

Set `version` in `Cargo.toml` to the new version and merge that to `main`.
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
