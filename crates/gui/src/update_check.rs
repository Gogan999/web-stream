//! Updates from the GitHub release manifest. Always started by the user
//! (a click, or at most one check when the app opens if they asked for
//! that): the manifest is fetched over HTTPS, the installer it lists is
//! downloaded over HTTPS and must match the manifest's size and SHA-256
//! before it is run. Nothing here is signed: trust comes from GitHub
//! (only the repository's maintainers publish releases) and the hashes
//! only prove the download is complete and is the file the release lists.
//!
//! The manifest is `.github/workflows/release.yml`'s `manifest.json`:
//! `{product, version, released, files: [{name, size, sha256}], notes}`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where the newest non-pre-release's manifest is; GitHub redirects to the
/// asset, and `latest` skips pre-releases.
pub const MANIFEST_URL: &str =
    "https://github.com/Gogan999/web-stream/releases/latest/download/manifest.json";
const DOWNLOAD_BASE: &str = "https://github.com/Gogan999/web-stream/releases/download";
const PRODUCT: &str = "web-stream-host";
/// The manifest is a few KiB; refuse anything absurd.
const MAX_MANIFEST: u64 = 1 << 20;
/// Setup is tens of MiB; a manifest claiming more than this is wrong.
const MAX_INSTALLER: u64 = 1 << 30;
const TIMEOUT: Duration = Duration::from_secs(10);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub product: String,
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// A newer release, with the installer to fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

impl Release {
    pub fn url(&self) -> String {
        format!("{DOWNLOAD_BASE}/v{}/{}", self.version, self.name)
    }
}

/// The release in `json` if it is newer than `current`; `None` if not.
/// Errors for a manifest that isn't ours or lists no usable installer.
pub fn parse_manifest(json: &str, current: &str) -> anyhow::Result<Option<Release>> {
    let m: Manifest = serde_json::from_str(json).context("the update manifest is not valid")?;
    if m.product != PRODUCT {
        bail!(
            "the update manifest is for \"{}\", not {PRODUCT}",
            m.product
        );
    }
    let latest = semver::Version::parse(&m.version)
        .with_context(|| format!("the manifest's version \"{}\" is not a version", m.version))?;
    // Pre-releases are only ever installed by hand.
    if !latest.pre.is_empty() {
        return Ok(None);
    }
    let current = semver::Version::parse(current).context("this app's version")?;
    if latest <= current {
        return Ok(None);
    }
    // The exact name the release workflow gives it, so a manifest can't
    // point the download anywhere else (or at a path).
    let name = format!("web-stream-setup-{}.exe", m.version);
    let file = m
        .files
        .iter()
        .find(|f| f.name == name)
        .with_context(|| format!("the release {} has no {name}", m.version))?;
    if file.size == 0 || file.size > MAX_INSTALLER {
        bail!("the manifest gives an unreasonable size for {name}");
    }
    if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the manifest gives no valid SHA-256 for {name}");
    }
    Ok(Some(Release {
        version: m.version,
        notes: m.notes.trim().to_string(),
        name,
        size: file.size,
        sha256: file.sha256.to_ascii_lowercase(),
    }))
}

fn agent() -> ureq::Agent {
    // Windows' own certificate store (and so a company's proxy root).
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(false)
        .user_agent(format!("web-stream-host/{VERSION}"))
        // Per request: the connection and the response headers. The body
        // of a download is limited by its own stall check below.
        .timeout_connect(Some(TIMEOUT))
        .timeout_recv_response(Some(TIMEOUT))
        .timeout_send_request(Some(TIMEOUT))
        .build()
        .into()
}

/// Asks GitHub for the latest release; `Some` if it is newer than this app.
pub fn check_latest() -> anyhow::Result<Option<Release>> {
    check_url(MANIFEST_URL, VERSION)
}

fn check_url(url: &str, current: &str) -> anyhow::Result<Option<Release>> {
    let mut resp = agent()
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("could not reach GitHub: {e}"))?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST)
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("could not read the update manifest: {e}"))?;
    parse_manifest(&body, current)
}

/// Downloads `release`'s installer into `dir`, checking its size and
/// SHA-256 against the manifest (a mismatch deletes it). `progress` gets
/// the bytes so far, at most a few times a second.
pub fn download(
    release: &Release,
    dir: &Path,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<PathBuf> {
    download_from(&release.url(), release, dir, progress)
}

fn download_from(
    url: &str,
    release: &Release,
    dir: &Path,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    clean_dir(dir);
    let target = dir.join(&release.name);
    let part = dir.join(format!("{}.part", release.name));
    let result = fetch_to(url, release, &part, progress)
        .and_then(|()| std::fs::rename(&part, &target).context("could not save the download"));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        let _ = std::fs::remove_file(&target);
        return Err(e);
    }
    Ok(target)
}

fn fetch_to(
    url: &str,
    release: &Release,
    part: &Path,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<()> {
    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("could not download the update: {e}"))?;
    let mut body = resp.into_body();
    let mut reader = body.as_reader();
    let mut file = std::fs::File::create(part)
        .with_context(|| format!("could not write {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    let mut last = Instant::now();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| anyhow::anyhow!("the download was interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        // The release's size is exact: a longer body is wrong, stop early.
        if total > release.size {
            bail!(
                "the download is larger than the release says ({} bytes)",
                release.size
            );
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .context("could not write the download")?;
        if last.elapsed() >= Duration::from_millis(150) {
            progress(total);
            last = Instant::now();
        }
    }
    file.sync_all().ok();
    drop(file);
    progress(total);
    if total != release.size {
        bail!(
            "the download is incomplete: {total} of {} bytes; try again",
            release.size
        );
    }
    let sum = hex(&hasher.finalize());
    if sum != release.sha256 {
        bail!("the download does not match the release's SHA-256 and was deleted; try again");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Removes earlier installers and partial downloads.
fn clean_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("web-stream-setup-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn updates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// The updater's own small settings (apart from the host's, so changing
/// them never restarts the host).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Check once when the app opens and say so if there is a newer
    /// version. Off unless the user turns it on; never installs by itself.
    pub notify_on_start: bool,
}

const PREFS_FILE: &str = "update-prefs.json";
const LAST_VERSION_FILE: &str = "last-version.txt";

impl Prefs {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(PREFS_FILE))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let path = dir.join(PREFS_FILE);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("could not write {}", path.display()))
    }
}

/// `Some(version)` once, the first time this version runs after a different
/// one did (an update); records the running version either way. Also tidies
/// up the downloaded installer.
pub fn take_updated_notice(data_dir: &Path, current: &str) -> Option<String> {
    let path = data_dir.join(LAST_VERSION_FILE);
    let previous = std::fs::read_to_string(&path).ok();
    let _ = std::fs::write(&path, current);
    clean_dir(&updates_dir(data_dir));
    previous
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty() && p != current)
        .map(|_| current.to_string())
}

/// (The downloaded path is only used to run setup, on Windows.)
#[derive(Debug, Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release, u64),
    Ready(Release, PathBuf),
    Failed(String),
}

/// The check and the download run on their own threads; the window reads
/// [`Updater::state`].
#[derive(Clone)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    dir: PathBuf,
}

impl Updater {
    pub fn new(data_dir: &Path, repaint: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Idle)),
            repaint: Arc::new(repaint),
            dir: updates_dir(data_dir),
        }
    }

    pub fn state(&self) -> State {
        self.state.lock().expect("updater lock").clone()
    }

    pub fn set(&self, s: State) {
        *self.state.lock().expect("updater lock") = s;
        (self.repaint)();
    }

    /// Looks for a newer release, unless one is already running.
    pub fn check(&self) {
        {
            let mut s = self.state.lock().expect("updater lock");
            if matches!(*s, State::Checking | State::Downloading(..)) {
                return;
            }
            *s = State::Checking;
        }
        (self.repaint)();
        let me = self.clone();
        std::thread::spawn(move || {
            let next = match check_latest() {
                Ok(Some(r)) => State::Available(r),
                Ok(None) => State::UpToDate,
                Err(e) => State::Failed(format!("{e:#}")),
            };
            me.set(next);
        });
    }

    /// Downloads and verifies the available release.
    pub fn download(&self) {
        let release = {
            let mut s = self.state.lock().expect("updater lock");
            let State::Available(r) = &*s else { return };
            let r = r.clone();
            *s = State::Downloading(r.clone(), 0);
            r
        };
        (self.repaint)();
        let me = self.clone();
        std::thread::spawn(move || {
            let mut progress = |n| {
                *me.state.lock().expect("updater lock") = State::Downloading(release.clone(), n);
                (me.repaint)();
            };
            let next = match download(&release, &me.dir, &mut progress) {
                Ok(path) => State::Ready(release.clone(), path),
                Err(e) => State::Failed(format!("{e:#}")),
            };
            me.set(next);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The manifest the release workflow published for v0.2.0 (shape and
    /// field order as `jq` writes it; hash shortened to a valid one).
    fn manifest(version: &str, size: u64, sha: &str) -> String {
        format!(
            r#"{{
  "product": "web-stream-host",
  "version": "{version}",
  "released": "2026-07-01T12:00:00Z",
  "files": [
    {{"name": "web-stream-host-{version}-windows-x64.zip", "size": 111, "sha256": "{sha}"}},
    {{"name": "web-stream-setup-{version}.exe", "size": {size}, "sha256": "{sha}"}}
  ],
  "notes": "Clipboard sync\n\nand fixes\n"
}}"#
        )
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn offers_a_newer_release_with_its_installer() {
        let r = parse_manifest(&manifest("0.3.0", 5000, SHA), "0.2.0")
            .unwrap()
            .unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.name, "web-stream-setup-0.3.0.exe");
        assert_eq!(r.size, 5000);
        assert_eq!(r.notes, "Clipboard sync\n\nand fixes");
        assert_eq!(
            r.url(),
            "https://github.com/Gogan999/web-stream/releases/download/v0.3.0/web-stream-setup-0.3.0.exe"
        );
    }

    #[test]
    fn same_or_older_is_not_an_update() {
        let m = manifest("0.2.0", 5000, SHA);
        assert_eq!(parse_manifest(&m, "0.2.0").unwrap(), None);
        assert_eq!(parse_manifest(&m, "0.10.0").unwrap(), None);
    }

    #[test]
    fn versions_compare_as_numbers_and_prereleases() {
        // 0.10.0 is newer than 0.9.0 (a string compare says otherwise).
        assert!(
            parse_manifest(&manifest("0.10.0", 5, SHA), "0.9.0")
                .unwrap()
                .is_some()
        );
        // The final release replaces its own release candidate...
        assert!(
            parse_manifest(&manifest("1.0.0", 5, SHA), "1.0.0-rc.1")
                .unwrap()
                .is_some()
        );
        // ...but a release candidate is never offered.
        assert_eq!(
            parse_manifest(&manifest("1.0.0-rc.2", 5, SHA), "0.2.0").unwrap(),
            None
        );
    }

    #[test]
    fn rejects_manifests_that_are_not_ours_or_unusable() {
        let wrong = manifest("0.3.0", 5, SHA).replace("web-stream-host\"", "other\"");
        assert!(parse_manifest(&wrong, "0.2.0").is_err());
        assert!(parse_manifest("not json", "0.2.0").is_err());
        assert!(parse_manifest(&manifest("0.3.0", 5, "abc"), "0.2.0").is_err());
        assert!(parse_manifest(&manifest("0.3.0", 0, SHA), "0.2.0").is_err());
        assert!(parse_manifest(&manifest("nonsense", 5, SHA), "0.2.0").is_err());
        // The installer must be listed under its release name.
        let renamed = manifest("0.3.0", 5, SHA).replace("web-stream-setup-0.3.0.exe", "evil.exe");
        assert!(parse_manifest(&renamed, "0.2.0").is_err());
    }

    /// Serves `body` once on localhost over plain HTTP.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = [0u8; 2048];
                let _ = s.read(&mut req);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        format!("http://127.0.0.1:{port}/setup.exe")
    }

    fn release_for(body: &[u8]) -> Release {
        Release {
            version: "0.3.0".into(),
            notes: String::new(),
            name: "web-stream-setup-0.3.0.exe".into(),
            size: body.len() as u64,
            sha256: hex(&Sha256::digest(body)),
        }
    }

    #[test]
    fn download_keeps_a_file_that_matches() {
        let body = vec![7u8; 300_000];
        let url = serve_once(body.clone());
        let dir = tempfile::tempdir().unwrap();
        let mut last = 0;
        let path = download_from(&url, &release_for(&body), dir.path(), &mut |n| last = n).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(last, body.len() as u64);
        assert!(!dir.path().join("web-stream-setup-0.3.0.exe.part").exists());
    }

    #[test]
    fn download_deletes_a_file_with_the_wrong_hash() {
        let body = vec![7u8; 1000];
        let url = serve_once(vec![8u8; 1000]);
        let dir = tempfile::tempdir().unwrap();
        let err = download_from(&url, &release_for(&body), dir.path(), &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("SHA-256"), "{err:#}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn download_rejects_a_wrong_size() {
        let body = vec![7u8; 1000];
        let short = serve_once(vec![7u8; 500]);
        let dir = tempfile::tempdir().unwrap();
        assert!(download_from(&short, &release_for(&body), dir.path(), &mut |_| {}).is_err());
        let long = serve_once(vec![7u8; 2000]);
        assert!(download_from(&long, &release_for(&body), dir.path(), &mut |_| {}).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn notice_shows_once_after_a_version_change() {
        let dir = tempfile::tempdir().unwrap();
        // First ever run: nothing to announce.
        assert_eq!(take_updated_notice(dir.path(), "0.2.0"), None);
        assert_eq!(take_updated_notice(dir.path(), "0.2.0"), None);
        assert_eq!(
            take_updated_notice(dir.path(), "0.3.0"),
            Some("0.3.0".into())
        );
        assert_eq!(take_updated_notice(dir.path(), "0.3.0"), None);
    }

    #[test]
    fn prefs_default_off_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Prefs::load(dir.path()).notify_on_start);
        Prefs {
            notify_on_start: true,
        }
        .save(dir.path())
        .unwrap();
        assert!(Prefs::load(dir.path()).notify_on_start);
    }

    /// Against the live release: `cargo test -p ws-host-gui live_manifest -- --ignored`.
    #[test]
    #[ignore = "needs the internet"]
    fn live_manifest_parses() {
        let r = check_url(MANIFEST_URL, "0.0.1").unwrap().unwrap();
        println!("{r:?}");
        assert!(r.name.starts_with("web-stream-setup-"));
    }
}
