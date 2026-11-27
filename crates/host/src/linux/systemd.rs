//! Installing and controlling the systemd service.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use super::{Paths, admin, lookup_user, replace_file, require_root};

pub const UNIT: &str = "web-stream.service";
const UDEV_RULE: &str = "rules.d/70-web-stream-uinput.rules";
const MODULES: &str = "web-stream.conf";

/// `/etc`, or `WEB_STREAM_ETC_DIR` for tests.
fn etc() -> PathBuf {
    std::env::var_os("WEB_STREAM_ETC_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("/etc"), PathBuf::from)
}

/// `/usr/local/bin`, or `WEB_STREAM_BIN_DIR` for tests.
fn bin() -> PathBuf {
    std::env::var_os("WEB_STREAM_BIN_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("/usr/local/bin"), PathBuf::from)
}

/// The unit file for a host installed at `exe` with its data in `data`.
pub fn unit(exe: &Path, data: &Path) -> String {
    format!(
        "\
# Installed by `web-stream service install`; changes go in `systemctl edit web-stream`.
[Unit]
Description=web-stream remote desktop host
Documentation=https://github.com/Gogan999/web-stream/blob/main/docs/guide/linux.md
Wants=network-online.target
After=network-online.target systemd-udevd.service systemd-logind.service
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart={exe} service run
Restart=on-failure
RestartSec=2s
RestartSteps=5
RestartMaxDelaySec=60s
TimeoutStopSec=15s
KillMode=mixed
Environment=RUST_LOG=info,str0m=warn
StateDirectory=web-stream
StateDirectoryMode=0755
RuntimeDirectory=web-stream
RuntimeDirectoryMode=0700
UMask=0077
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_NICE CAP_SETUID CAP_SETGID CAP_CHOWN CAP_DAC_READ_SEARCH
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths={data}
PrivateTmp=yes
ProtectHome=read-only
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictSUIDSGID=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
LockPersonality=yes
KeyringMode=private
SystemCallArchitectures=native
SystemCallFilter=~@clock @cpu-emulation @debug @module @mount @obsolete @raw-io @reboot @swap
SystemCallErrorNumber=EPERM
DevicePolicy=closed
DeviceAllow=/dev/uinput rw
DeviceAllow=char-drm rw
DeviceAllow=char-nvidia* rw
DeviceAllow=char-alsa rw

[Install]
WantedBy=multi-user.target
",
        exe = exe.display(),
        data = data.display(),
    )
}

/// Lets the host make virtual keyboards, mice and gamepads.
const UDEV: &str = "\
# Installed by web-stream: /dev/uinput, for its virtual keyboard, mouse and gamepads.
KERNEL==\"uinput\", SUBSYSTEM==\"misc\", MODE=\"0600\", OPTIONS+=\"static_node=uinput\"
";

/// Runs `systemctl args`, or `WEB_STREAM_SYSTEMCTL` in its place (for tests).
pub fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let program = std::env::var("WEB_STREAM_SYSTEMCTL").unwrap_or_else(|_| "systemctl".into());
    let status = Command::new(&program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Runs a helper whose failure only gets a warning.
fn try_run(program: &str, args: &[&str]) {
    match Command::new(program).args(args).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "warning: {program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("warning: {program}: {e}"),
    }
}

fn write(path: &Path, text: &str, mode: u32) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("{}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// The shell for a user nobody logs in as.
fn nologin() -> &'static str {
    ["/usr/sbin/nologin", "/sbin/nologin", "/usr/bin/nologin"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("/bin/false")
}

/// `service install`.
pub fn install(paths: &Paths, args: &[String]) -> anyhow::Result<()> {
    if !args.is_empty() {
        bail!("service install takes no options: change the settings on the settings page");
    }
    require_root("service install")?;
    let here = std::env::current_exe().context("this program's path")?;
    let exe = paths.exe();
    std::fs::create_dir_all(&paths.lib)?;
    std::fs::set_permissions(&paths.lib, std::fs::Permissions::from_mode(0o755))?;
    if here.canonicalize().ok() != exe.canonicalize().ok() {
        replace_file(&here, &exe)?;
        let cloudflared = here.with_file_name("cloudflared");
        if cloudflared.is_file() {
            replace_file(&cloudflared, &paths.lib.join("cloudflared"))?;
        }
        println!("copied the host to {}", exe.display());
    }
    let link = bin().join("web-stream");
    if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(bin())?;
        std::os::unix::fs::symlink(&exe, &link).with_context(|| format!("{}", link.display()))?;
    }

    if lookup_user(&paths.user)?.is_none() {
        let status = Command::new("useradd")
            .args(["--system", "--user-group", "--no-create-home", "--home-dir"])
            .arg(paths.network())
            .args([
                "--shell",
                nologin(),
                "--comment",
                "web-stream host (network side)",
            ])
            .arg(&paths.user)
            .status()
            .context("run useradd")?;
        if !status.success() {
            bail!("useradd {} failed ({status})", paths.user);
        }
        println!("created the {} user", paths.user);
    }

    let etc = etc();
    write(&etc.join("udev").join(UDEV_RULE), UDEV, 0o644)?;
    write(
        &etc.join("modules-load.d").join(MODULES),
        "# Installed by web-stream.\nuinput\n",
        0o644,
    )?;
    try_run("modprobe", &["uinput"]);
    try_run("udevadm", &["control", "--reload-rules"]);
    try_run(
        "udevadm",
        &[
            "trigger",
            "--subsystem-match=misc",
            "--sysname-match=uinput",
        ],
    );

    write(
        &etc.join("systemd/system").join(UNIT),
        &unit(&exe, &paths.data),
        0o644,
    )?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    systemctl(&["restart", UNIT])?;
    println!(
        "the web-stream service is running. Next:\n  sudo web-stream pair       pair a device\n  \
         sudo web-stream settings   open the settings page"
    );
    Ok(())
}

/// `service uninstall [--purge]`: everything but the data dir, which `--purge` removes too.
pub fn uninstall(paths: &Paths, args: &[String]) -> anyhow::Result<()> {
    let purge = match args {
        [] => false,
        [p] if p == "--purge" => true,
        _ => bail!("usage: web-stream service uninstall [--purge]"),
    };
    require_root("service uninstall")?;
    let etc = etc();
    let unit_file = etc.join("systemd/system").join(UNIT);
    if unit_file.exists() {
        let _ = systemctl(&["disable", "--now", UNIT]);
        std::fs::remove_file(&unit_file)?;
        systemctl(&["daemon-reload"])?;
    }
    for f in [
        etc.join("udev").join(UDEV_RULE),
        etc.join("modules-load.d").join(MODULES),
    ] {
        let _ = std::fs::remove_file(f);
    }
    try_run("udevadm", &["control", "--reload-rules"]);
    if lookup_user(&paths.user)?.is_some() {
        try_run("userdel", &[&paths.user]);
    }
    let link = bin().join("web-stream");
    if std::fs::read_link(&link).ok().as_deref() == Some(paths.exe().as_path()) {
        let _ = std::fs::remove_file(&link);
    }
    if paths.lib.exists() {
        std::fs::remove_dir_all(&paths.lib).with_context(|| format!("{}", paths.lib.display()))?;
    }
    if purge {
        if paths.data.exists() {
            std::fs::remove_dir_all(&paths.data)?;
        }
        println!("web-stream is uninstalled, its data removed");
    } else {
        println!(
            "web-stream is uninstalled; its certificate, paired devices and settings stay in {}",
            paths.data.display()
        );
    }
    Ok(())
}

/// `service status`.
pub fn status(paths: &Paths) -> anyhow::Result<()> {
    let r = systemctl(&["--no-pager", "status", UNIT]);
    if paths.network().join(admin::TOKEN_FILE).exists() {
        println!("\nsettings page: sudo web-stream settings");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_the_installed_host_hardened() {
        let u = unit(
            Path::new("/usr/local/lib/web-stream/web-stream"),
            Path::new("/var/lib/web-stream"),
        );
        assert!(u.contains("ExecStart=/usr/local/lib/web-stream/web-stream service run\n"));
        assert!(u.contains("ReadWritePaths=/var/lib/web-stream\n"));
        for line in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "PrivateTmp=yes",
            "Restart=on-failure",
        ] {
            assert!(u.lines().any(|l| l == line), "{line}");
        }
        let caps = u
            .lines()
            .find_map(|l| l.strip_prefix("CapabilityBoundingSet="))
            .unwrap();
        assert!(caps.contains("CAP_SYS_ADMIN") && !caps.contains("CAP_NET_ADMIN"));
    }
}
