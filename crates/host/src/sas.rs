//! Ctrl+Alt+Del (the secure attention sequence). Windows takes it only from
//! a service, so the host hands the request to whoever installed a sender:
//! the web-stream service's session host does, the tray app and console
//! host don't.

use std::sync::OnceLock;

type Sender = Box<dyn Fn() -> Result<(), String> + Send + Sync>;

static SENDER: OnceLock<Sender> = OnceLock::new();

/// Sets how Ctrl+Alt+Del is sent, once per process.
pub fn install(send: impl Fn() -> Result<(), String> + Send + Sync + 'static) {
    let _ = SENDER.set(Box::new(send));
}

/// Asks for Ctrl+Alt+Del on the host. The error is for the browser.
pub fn request() -> Result<(), String> {
    match SENDER.get() {
        Some(send) => send(),
        None => Err(
            "Ctrl+Alt+Del needs the host to run as the web-stream service \
                     (install it from the tray app's settings, see docs/host-setup.md)"
                .into(),
        ),
    }
}
