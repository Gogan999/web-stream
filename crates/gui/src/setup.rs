//! `ws-host-gui setup ...`: the steps the installer and uninstaller run.
//!
//! ```text
//! setup autostart on|off   start with Windows for this user (run as the user)
//! setup mic-driver         install Steam's virtual microphone (elevated)
//! setup upnp-remove        remove this host's port mappings from the router
//! ```
//!
//! None of them take a path or anything else to act on from the command
//! line: the elevated ones only touch fixed places.

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

fn upnp_remove() -> anyhow::Result<String> {
    Ok(match crate::upnp::find_router()? {
        Some(router) => format!("removed {} port mappings", router.unmap_ours()),
        None => "no router answers UPnP; nothing to remove".into(),
    })
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
