//! The headless Linux host: `web-stream serve`, the systemd service and the commands that control it.

#[cfg(target_os = "linux")]
fn main() {
    if let Err(e) = ws_host::linux::main(std::env::args().skip(1).collect()) {
        eprintln!("web-stream: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("web-stream: this program is the Linux host; on Windows run ws-host-gui");
    std::process::exit(1);
}
