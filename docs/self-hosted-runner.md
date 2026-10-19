# Self-hosted CI runner (Windows)

CI runs on a Windows PC registered as a GitHub Actions self-hosted runner, not
on GitHub-hosted runners. Both workflows (`ci.yml`, `web-client.yml`) target
`runs-on: [self-hosted, windows, x64]`; those are the labels every Windows x64
runner gets by default, so no custom labels are needed. Jobs queue while the
PC is off and start when it comes back.

**Security:** keep the repository private. A self-hosted runner executes the
code of every pull request on this PC, so it must never be attached to a
public repository.

## 1. Install the toolchain

From an elevated PowerShell:

```powershell
winget install --id Git.Git -e
winget install --id Rustlang.Rustup -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id Kitware.CMake -e      # opusic-sys builds libopus with CMake
winget install --id NASM.NASM -e          # used by aws-lc-sys when present
```

Then, in a new shell as the user the runner will run as:

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

Node 22 is fetched by `actions/setup-node` and Playwright's Chromium by the
workflow itself, so neither needs installing.

## 2. Register the runner

1. On GitHub: repository **Settings > Actions > Runners > New self-hosted
   runner**, pick **Windows / x64**.
2. Run the commands that page shows in PowerShell from a short path such as
   `C:\actions-runner` (long paths break some builds).
3. When `config.cmd` asks:
   - runner group, name, labels, work folder: press Enter for the defaults;
   - **run as service: Y**, and give **your own Windows account** (not the
     default NETWORK SERVICE), so the service sees the rustup toolchain
     installed under your profile.
4. The runner shows as **Idle** on the Runners page once it is up.

## 3. Optional: hosted Linux job

`ci.yml` also has an `ubuntu-latest` job for the signaling server on Linux.
It uses GitHub-hosted minutes, so it is off unless the repository variable
`HOSTED_LINUX_CI` is set to `true` (Settings > Secrets and variables >
Actions > Variables).
