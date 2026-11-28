//! The headless Linux host: `web-stream serve`, the systemd service and the commands that control it.

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == ws_host::clipboard::HELPER_ARG)
    {
        std::process::exit(ws_host::clipboard::helper_main());
    }
    if args
        .first()
        .is_some_and(|a| a == ws_host::display_mode::HELPER_ARG)
    {
        std::process::exit(ws_host::display_mode::helper_main());
    }
    if let Err(e) = ws_host::linux::main(args) {
        eprintln!("web-stream: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("web-stream: this program is the Linux host; on Windows run ws-host-gui");
    std::process::exit(1);
}
