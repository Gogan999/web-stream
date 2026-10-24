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
3. **publish** waits for approval in the `release` environment. It writes
   `manifest.json` (version, time, each file's size and SHA-256, and the
   tag's message as release notes), signs the manifest, the zip and the
   installer with minisign (Ed25519), checks the signatures against
   `release/minisign.pub`, and creates the GitHub release with the files,
   their `.minisig` signatures and `SHA256SUMS.txt`.

The signatures are what the host's updater (Phase 2) will check, against a
public key compiled into the host. They are separate from Windows code
signing, which the project doesn't use: friends click past SmartScreen
(see `friend-guide.md`).

## One-time setup (Logan)

1. **Make the signing key**, on your own PC, without a password (the key
   is protected by the environment instead, and CI can't type one):

   ```
   minisign -G -W -p minisign.pub -s minisign.key
   ```

   minisign is on winget (`winget install jedisct1.minisign`). Keep a copy
   of `minisign.key` offline (a USB stick) in case the environment's copy
   is lost; anyone with it can sign releases.

2. **Commit the public key** as `release/minisign.pub` (a PR, as usual).

3. **Make the environment**: repository Settings > Environments > New
   environment `release`. Under **Deployment protection rules** tick
   **Required reviewers** and add yourself, and leave **Prevent
   self-review** unticked (you push the tags yourself). Under **Deployment branches and tags**, choose
   **Selected branches and tags** and add the tag rule `v*`. Then
   **Add environment secret** `MINISIGN_SECRET_KEY` with the whole contents
   of `minisign.key`.

   Only the publish job names this environment, so only it can read the
   key, and only after you approve that run.

## Making a release

```
# Cargo.toml: version = "0.2.0", merged to main
git checkout main && git pull
git tag -a v0.2.0 -m "What changed, in a few lines: this text is shown in the tray before updating"
git push origin v0.2.0
```

Then Actions > release > the run > **Review deployments** > Approve, once
build is green. A test of the pipeline: tag `v0.1.0-rc.1` with the current
version; it lands as a pre-release, which the updater will ignore, and can
be deleted afterwards.

A failed run publishes nothing. Delete the tag, fix, and tag again.
