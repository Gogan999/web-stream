//! Embeds the web client (`web/index.html`, `style.css`, the manifest and
//! `js/`) in the binary, so the client served is always the one this host
//! was built with. `--web-root` still overrides it.

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn main() {
    let web = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web");
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for name in ["index.html", "style.css", "manifest.webmanifest"] {
        files.push((name.to_string(), web.join(name)));
    }
    let js = web.join("js");
    let mut scripts: Vec<_> = std::fs::read_dir(&js)
        .expect("web/js")
        .map(|e| e.expect("web/js entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect();
    scripts.sort();
    for p in scripts {
        let name = p.file_name().unwrap().to_str().expect("utf-8 file name");
        files.push((format!("js/{name}"), p));
    }

    let mut out = String::from("pub static WEB_FILES: &[(&str, &[u8])] = &[\n");
    for (path, file) in &files {
        let file = file
            .canonicalize()
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        println!("cargo:rerun-if-changed={}", file.display());
        writeln!(
            out,
            "    ({path:?}, include_bytes!({:?})),",
            file.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n");
    println!("cargo:rerun-if-changed={}", js.display());
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("web_files.rs");
    std::fs::write(dest, out).unwrap();
}
