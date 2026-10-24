//! `ws-host-gui setup ...`: the steps the installer and uninstaller run.
//!
//! ```text
//! setup autostart on|off   start with Windows for this user (run as the user)
//! setup mic-driver         install Steam's virtual microphone (elevated)
//! setup upnp-remove        remove this host's port mappings from the router
//! ```
//!
//! None of them take a path or anything else to act on from the command
//! line: the elevated ones only touch fixed places. Their log, setup.log,
//! goes to the data folder of whoever runs them: for the elevated steps,
//! the administrator account UAC ran them as.

use anyhow::bail;

/// The exit code.
pub fn main(args: &[String]) -> i32 {
    match run(args) {
        Ok(text) => {
            tracing::info!("setup {args:?}: {text}");
            println!("{text}");
            0
        }
        Err(e) => {
            tracing::error!("setup {args:?}: {e:#}");
            eprintln!("{e:#}");
            1
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["autostart", on @ ("on" | "off")] => autostart(*on == "on"),
        ["mic-driver"] => mic_driver(),
        ["upnp-remove"] => upnp_remove(),
        _ => bail!("usage: setup autostart on|off | mic-driver | upnp-remove"),
    }
}

#[cfg(windows)]
fn autostart(on: bool) -> anyhow::Result<String> {
    crate::win::set_autostart(on)?;
    Ok(format!(
        "start with Windows: {}",
        if on { "on" } else { "off" }
    ))
}

#[cfg(not(windows))]
fn autostart(_on: bool) -> anyhow::Result<String> {
    bail!("Windows only")
}

#[cfg(windows)]
fn mic_driver() -> anyhow::Result<String> {
    let inf = crate::win::steam_mic_inf()?;
    if !inf.exists() {
        bail!(
            "Steam's microphone driver is not on this PC ({}): install Steam, or VB-CABLE",
            inf.display()
        );
    }
    let reboot = crate::win::install_driver(&inf)?;
    Ok(if reboot {
        "installed Steam Streaming Microphone; Windows needs a restart to finish".into()
    } else {
        "installed Steam Streaming Microphone".into()
    })
}

#[cfg(not(windows))]
fn mic_driver() -> anyhow::Result<String> {
    bail!("Windows only")
}

/// Bounded, so a slow router can't hold up the uninstaller; whatever is
/// left expires within the hour anyway (the leases aren't renewed).
fn upnp_remove() -> anyhow::Result<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("ws-upnp-remove".into())
        .spawn(move || {
            let _ = tx.send(crate::upnp::find_router().map(|r| r.map(|r| r.unmap_all_ours())));
        })?;
    match rx.recv_timeout(crate::upnp::REMOVE_DEADLINE) {
        Ok(Ok(Some(n))) => Ok(format!("removed {n} port mappings")),
        Ok(Ok(None)) => Ok("no router answers UPnP; nothing to remove".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            Ok("the router didn't answer in time; its forwards expire within the hour".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_steps_are_refused() {
        for args in [
            &["autostart"][..],
            &["autostart", "maybe"],
            &["mic-driver", "x.inf"],
            &[],
        ] {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert!(run(&args).is_err(), "{args:?}");
        }
    }
}
