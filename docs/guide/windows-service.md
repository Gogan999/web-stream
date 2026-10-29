# Sign-in screen and administrator programs

Run normally, the host runs as you, and Windows keeps it out of three
places:

- programs running as administrator ignore its input;
- UAC prompts, and
- the lock and sign-in screens

are on a separate, protected desktop it can neither capture nor type into.
The stream freezes there until your desktop comes back.

To reach those too, install the host as a Windows service. This is what
**Allow use at the login screen** in the installer does.

## Installing the service

In the host app's settings, under **Windows service**, click **Install the
service** and accept the administrator prompt. It copies the host to
`C:\Program Files\web-stream`, registers the **web-stream** service (which
starts with Windows) and starts it, using the saved settings. A message says
how it went. **Reinstall with these settings** changes the settings later.

The service then keeps a host running on the PC's screen from the sign-in
screen on: before anyone signs in, while the PC is locked, after signing out
and when switching users. It follows Windows onto the UAC and lock screens
and types into programs running as administrator.

## Using it

- **The tray app stops its own host** when installing the service, and
  doesn't start it while the service is installed, because they use the
  same ports. It keeps running in the tray as the service's front end.
- **Pair again.** The service has its own certificate (the browser warns
  about a new one) and its own paired browsers. Click **Pair through the
  service** (an administrator prompt), request a code in the browser, and
  type the code the service console window shows. **Service console** opens
  the same window without pairing, with **Disconnect the browser**,
  **Share files...** and **Unpair every browser**.
- **Updating:** a newer installer updates the service in place. From
  source, `scripts\update.ps1` updates it too (one UAC prompt), or build and
  click **Update the service** in the tray app.
- **Logs and data** are in
  `C:\Windows\System32\config\systemprofile\AppData\Local\web-stream`:
  `host.log` (the part on the screen), `network.log` (the network part) and
  `service.log` (the service starting and restarting them). The certificate
  and paired browsers are in its `network` folder.
- **Uninstall** (an administrator prompt) removes the service and the
  Program Files copy, and leaves its data. Click **Start** in the tray app
  to host from it again.

The service refuses a few settings: serving the web client from a folder,
receiving files from the browser, and updates from the browser. Sending
files from the host still works through the service console.

## Ctrl+Alt+Del

A browser can't send Ctrl+Alt+Del as keys, so use the **Ctrl+Alt+Del**
button in the browser's top bar (or under **Keys** on a phone). The service
asks Windows for it.

Windows only accepts it from a service when the policy *Disable or enable
software Secure Attention Sequence* (Computer Configuration > Administrative
Templates > Windows Components > Windows Logon Options) is enabled for
**Services**. That is the registry value `SoftwareSASGeneration` = 1 under
`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`. It is off
by default, and `service.log` says so when a request comes in. Most PCs
don't need Ctrl+Alt+Del to sign in: a click or a key opens the lock screen.

## Security

Whoever is paired with the service can do anything on this PC, including
answering UAC prompts. The service is built to keep that power away from
everyone else:

- **Pairing needs an administrator.** The pairing code is shown only to an
  administrator: the service's host is controlled through a pipe that
  requires an elevated prompt, so a program running as a normal user can't
  open pairing.
- **The network side has no privileges.** The host runs as two processes.
  Everything that reads what comes off the network (HTTPS and TLS, the
  signaling socket, the peer connection's DTLS, SRTP and data channels,
  Let's Encrypt) runs as LocalService with every privilege removed but one
  (it can't impersonate other accounts, which would be a way back to
  SYSTEM), and can only read and write its own data folder. A bug there
  gives an attacker that process: they could stream the screen and send
  input, as a paired browser can, but not write to System32 or the registry
  or install a service.
- **The screen side runs as SYSTEM.** Capture, encoding, input and audio
  run as SYSTEM, which is effectively full control of the PC. Its privileges
  are removed (it can't load drivers, debug other processes or take
  ownership of files), but it keeps the SYSTEM account itself. It takes from
  the network process only what a browser could send (input, control
  messages, files it asked for), size-limited and checked. The browser mic's
  audio is decoded there.
- **Fallback.** If the network process can't be started at all (Windows
  refuses it a logon or a window station before any of its code runs), the
  service runs the whole host as SYSTEM, and `service.log` and `host.log`
  say so. It then keeps its certificate and paired browsers in the
  service's own data folder, so browsers have to pair again. A network
  process that crashes, or a `network` folder that can't be set up, is
  retried later, never replaced by a SYSTEM one.
- **Only administrators can change what runs.** What runs as SYSTEM is the
  copy in Program Files, never a build folder in a source checkout.
  `--cert`, `--key` and the DuckDNS token are copied into the network
  process's data folder on install, so a user who can write where they came
  from can't swap them.
- **The control pipe can't be impersonated.** The service picks a new
  random name for it each time it starts the host and records it, with the
  host's process id, in its data folder. The service console connects only
  if that process answers.
- **Brief gap on restart.** While the host restarts (signing out, switching
  users) the ports are briefly free and any local program could listen on
  them. A browser that connects then sees a certificate other than the one
  it trusts, and warns.
- **Refused settings.** `--web-root` (a folder a user could change would run
  their code in a browser that controls the PC), `--data-dir`, and
  `--allow-upload` (it would write as SYSTEM into a folder a user controls).
