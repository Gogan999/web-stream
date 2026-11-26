//! Ctrl+Alt+Del.

use std::sync::OnceLock;

type Sender = Box<dyn Fn() -> Result<(), String> + Send + Sync>;

static SENDER: OnceLock<Sender> = OnceLock::new();

/// Sets how Ctrl+Alt+Del is sent, once per process.
pub fn install(send: impl Fn() -> Result<(), String> + Send + Sync + 'static) {
    let _ = SENDER.set(Box::new(send));
}

/// Whether [`request`] can work here.
pub fn available() -> bool {
    SENDER.get().is_some()
}

/// Asks for Ctrl+Alt+Del on the host.
pub fn request() -> Result<(), String> {
    match SENDER.get() {
        Some(send) => send(),
        None => Err(
            "Ctrl+Alt+Del needs the host to run as the web-stream service \
                     (install it from the tray app's settings, see docs/guide/windows-service.md)"
                .into(),
        ),
    }
}
