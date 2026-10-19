# Self-hosted CI runner (Windows)

CI runs on a Windows PC registered as a GitHub Actions self-hosted runner, not
on GitHub-hosted runners. Both workflows (`ci.yml`, `web-client.yml`) target
`runs-on: [self-hosted, windows, x64]`; those are the labels every Windows x64
runner gets by default, so no custom labels are needed. Steps use `shell: cmd`
so Windows' PowerShell execution policy doesn't matter. Jobs queue while the
PC is off and start when it comes back.

**Security:**
- Keep the repository private. A self-hosted runner executes the code of every
  pull request on this PC, so it must never be attached to a public repository.
- Run the runner as its own standard (non-admin) local account, never as your
  own. Jobs can read everything that account can, and your account holds the
  host's TLS key and pairing store (`%LOCALAPPDATA%\web-stream`), SSH keys and
  browser profiles.
- Leave **Settings > Actions > General > Fork pull request workflows** off.

## 1. Install shared tools

From an elevated PowerShell (these install machine-wide):

```powershell
winget install --id Git.Git -e --scope machine
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install --id Kitware.CMake -e --scope machine   # opusic-sys builds libopus with CMake
winget install --id NASM.NASM -e --scope machine       # used by aws-lc-sys when present
```

Node 22 is fetched by `actions/setup-node` and Playwright's Chromium by the
workflow itself, so neither needs installing.

## 2. Create the runner account and its Rust toolchain

Still elevated:

```powershell
New-LocalUser -Name ci-runner -Password (Read-Host -AsSecureString "Password") -PasswordNeverExpires
runas /user:ci-runner powershell
```

In the window that opens (running as `ci-runner`):

```powershell
cd $env:TEMP
Invoke-WebRequest https://win.rustup.rs/x86_64 -OutFile rustup-init.exe
.\rustup-init.exe -y --default-toolchain stable-x86_64-pc-windows-msvc
```

CI updates this account's toolchain on every run; your own Rust install is
untouched.

## 3. Register the runner

1. On GitHub: repository **Settings > Actions > Runners > New self-hosted
   runner**, pick **Windows / x64**.
2. In an elevated PowerShell, run the commands that page shows from a short
   path such as `C:\actions-runner` (long paths break some builds).
3. When `config.cmd` asks:
   - runner group, name, labels, work folder: press Enter for the defaults;
   - **run as service: Y**, with account **`.\ci-runner`** and its password.
4. The runner shows as **Idle** on the Runners page once it is up.

## 4. Optional: hosted Linux job

`ci.yml` also has an `ubuntu-latest` job for the signaling server on Linux.
It uses GitHub-hosted minutes, so it is off unless the repository variable
`HOSTED_LINUX_CI` is set to `true` (Settings > Secrets and variables >
Actions > Variables).
