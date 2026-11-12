//! `service sandbox-test <folder>`: checks, as the service would, that the
//! network host's folder gets its label and that a process with the network
//! host's token can do what it needs and not what it mustn't. Run by CI as
//! SYSTEM (installer/test-network-sandbox.ps1), because none of this behaves
//! the same for an ordinary user, and it can't be learned on a real PC
//! without taking the stream down.

use std::net::{TcpListener, UdpSocket};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};

use super::{launch, scm};

const OLD_FILE: &str = "sandbox-test-old.txt";
const NEW_FILE: &str = "sandbox-test-new.txt";

/// The parent: `outside` is a folder this makes, open to Everyone and to
/// Users, for the child to fail to write in.
pub fn run(args: &[String]) -> anyhow::Result<String> {
    let [outside] = args else {
        bail!("sandbox-test needs a scratch folder to make");
    };
    let outside = Path::new(outside);
    std::fs::create_dir_all(outside)?;
    // The app has no console: what happened goes in a file.
    let report = outside.join("report.txt");
    let result = run_in(outside, &report);
    let line = match &result {
        Ok(text) => format!("RESULT {text}\n"),
        Err(e) => format!("RESULT FAIL {e:#}\n"),
    };
    note(&report, &line);
    result
}

fn note(report: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(report)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn run_in(outside: &Path, report: &Path) -> anyhow::Result<String> {
    // Open to every user, at medium integrity: only the token's restrictions
    // keep the network host out.
    launch::secure_dir(
        outside,
        "D:P(A;OICI;FA;;;WD)(A;OICI;FA;;;BU)(A;OICI;FA;;;SY)",
    )?;
    let outside_file = outside.join("outside.txt");

    // A file from before the label, which the network host must still be
    // able to write.
    let dir = scm::network_dir()?;
    std::fs::create_dir_all(&dir)?;
    let old = dir.join(OLD_FILE);
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::remove_file(dir.join(NEW_FILE));
    std::fs::write(&old, b"old")?;

    // What the label calls do here, and whether icacls can set one at all.
    let probe = outside.join("probe");
    std::fs::create_dir_all(&probe)?;
    for (what, path) in [("temp folder", &probe), ("network folder", &dir)] {
        for line in launch::label_diagnose(path, launch::NETWORK_DIR_LABEL_SDDL, true) {
            note(report, &format!("[{what}] {line}\n"));
        }
        let icacls = std::process::Command::new("icacls")
            .arg(path)
            .args(["/setintegritylevel", "(OI)(CI)L"])
            .output();
        match icacls {
            Ok(o) => note(
                report,
                &format!(
                    "[{what}] icacls /setintegritylevel: {} {}{}\n",
                    o.status,
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            ),
            Err(e) => note(report, &format!("[{what}] icacls did not run: {e}\n")),
        }
    }

    let (dir, low) = scm::prepare_network_dir()?;
    if !low {
        bail!("the folder and its files could not all be labelled low integrity");
    }
    note(report, &format!("labelled {}\n", dir.display()));

    let token = launch::network_token(true)?;
    let exe = std::env::current_exe()?;
    let log = outside.join("child.log");
    let args = vec![
        "service".to_string(),
        "sandbox-child".into(),
        dir.display().to_string(),
        outside_file.display().to_string(),
    ];
    let child = launch::spawn_network(&token, &exe, &args, &log, &[])?;
    if !child.wait(Duration::from_secs(60)) {
        child.kill();
        bail!("the child did not finish");
    }
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    note(report, &output);
    let _ = std::fs::remove_file(dir.join(OLD_FILE));
    let _ = std::fs::remove_file(dir.join(NEW_FILE));
    if child.exit_code() != Some(0) {
        bail!("the child failed ({:?})", child.exit_code());
    }
    Ok("sandbox ok".into())
}

/// The child, with the network host's token.
pub fn child(args: &[String]) -> anyhow::Result<String> {
    let [dir, outside_file] = args else {
        bail!("sandbox-child needs two paths");
    };
    let dir = Path::new(dir);
    let mut failed = false;
    let mut check = |what: &str, ok: bool| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        failed |= !ok;
    };
    check(
        "write a new file in its folder",
        std::fs::write(dir.join(NEW_FILE), b"x").is_ok(),
    );
    check(
        "write a file that was there before the label",
        std::fs::write(dir.join(OLD_FILE), b"y").is_ok(),
    );
    check(
        "read it back",
        std::fs::read(dir.join(NEW_FILE)).is_ok_and(|c| c == b"x"),
    );
    check(
        "bind a TCP socket",
        TcpListener::bind("127.0.0.1:0").is_ok(),
    );
    check("bind a UDP socket", UdpSocket::bind("0.0.0.0:0").is_ok());
    check(
        "NOT write in a folder open to Everyone and Users",
        std::fs::write(outside_file, b"z").is_err(),
    );
    check(
        "NOT write in the folder above its own",
        std::fs::write(
            dir.parent()
                .context("no parent")?
                .join("sandbox-escape.txt"),
            b"z",
        )
        .is_err(),
    );
    check(
        "NOT write in the Windows folder",
        std::fs::write(r"C:\Windows\sandbox-escape.txt", b"z").is_err(),
    );
    if failed {
        bail!("some checks failed");
    }
    Ok(String::new())
}
