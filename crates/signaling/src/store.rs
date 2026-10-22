//! Paired devices, kept on disk as hashes of their tokens.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;

const DEVICES_FILE: &str = "devices.json";
const TOUCH_INTERVAL_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub name: String,
    token_sha256: String,
    pub paired_at: u64,
    pub last_used: u64,
}

pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<Device>,
    idle_expiry: Duration,
}

impl DeviceStore {
    pub fn open(data_dir: &Path, idle_expiry: Duration) -> Result<Self, Error> {
        let path = data_dir.join(DEVICES_FILE);
        let devices = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Error::Store(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            devices,
            idle_expiry,
        })
    }

    /// Pair a new device and return the token to hand to it. The token itself
    /// is never stored.
    pub fn add(&mut self, name: &str, now: SystemTime) -> Result<String, Error> {
        let token = random_token()?;
        let secs = unix_secs(now);
        self.devices.push(Device {
            name: name.to_string(),
            token_sha256: hash(&token),
            paired_at: secs,
            last_used: secs,
        });
        self.save()?;
        Ok(token)
    }

    /// Look up a token without marking it used.
    pub fn check(&self, token: &str, now: SystemTime) -> Option<&Device> {
        let h = hash(token);
        let cutoff = unix_secs(now).saturating_sub(self.idle_expiry.as_secs());
        self.devices
            .iter()
            .find(|d| d.token_sha256 == h && d.last_used >= cutoff)
    }

    /// Validate a token at the start of a session, refresh its idle timer and
    /// drop any devices that have expired.
    pub fn authenticate(&mut self, token: &str, now: SystemTime) -> Result<Option<Device>, Error> {
        let secs = unix_secs(now);
        let cutoff = secs.saturating_sub(self.idle_expiry.as_secs());
        let before = self.devices.len();
        self.devices.retain(|d| d.last_used >= cutoff);
        let h = hash(token);
        let mut touched = false;
        let found = self
            .devices
            .iter_mut()
            .find(|d| d.token_sha256 == h)
            .map(|d| {
                // The idle timer counts in days, so an hourly refresh is
                // plenty and keeps disk writes off most connects.
                if secs.saturating_sub(d.last_used) >= TOUCH_INTERVAL_SECS {
                    d.last_used = secs;
                    touched = true;
                }
                d.clone()
            });
        if touched || self.devices.len() != before {
            self.save()?;
        }
        Ok(found)
    }

    pub fn revoke(&mut self, token: &str) -> Result<bool, Error> {
        let h = hash(token);
        let before = self.devices.len();
        self.devices.retain(|d| d.token_sha256 != h);
        let removed = self.devices.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn revoke_all(&mut self) -> Result<(), Error> {
        self.devices.clear();
        self.save()
    }

    #[cfg(test)]
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    fn save(&self) -> Result<(), Error> {
        let json = serde_json::to_vec_pretty(&self.devices).expect("devices serialize");
        write_private(&self.path, &json)
    }
}

/// Write a file readable only by the current user, replacing it atomically.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    // A fresh random name with `create_new`, so a pre-planted file or link
    // at a predictable temp path is never written through.
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|e| Error::Random(e.to_string()))?;
    let tmp = dir.join(format!(".{}.tmp", URL_SAFE_NO_PAD.encode(suffix)));
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        crate::acl::restrict_to_owner(&tmp)?;
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

pub(crate) fn random_token() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn tokens_survive_restart_and_expire_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let token = {
            let mut store = DeviceStore::open(dir.path(), 30 * DAY).unwrap();
            store.add("laptop", t0).unwrap()
        };
        let raw = std::fs::read_to_string(dir.path().join(DEVICES_FILE)).unwrap();
        assert!(!raw.contains(&token), "token must not be stored in clear");

        let mut store = DeviceStore::open(dir.path(), 30 * DAY).unwrap();
        assert!(store.authenticate("wrong", t0).unwrap().is_none());
        let dev = store.authenticate(&token, t0 + 29 * DAY).unwrap().unwrap();
        assert_eq!(dev.name, "laptop");
        // Use at day 29 refreshed the idle timer.
        assert!(store.check(&token, t0 + 58 * DAY).is_some());
        assert!(store.authenticate(&token, t0 + 60 * DAY).unwrap().is_none());
        assert!(store.devices().is_empty());
    }

    #[test]
    fn revoke_removes_only_that_device() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let mut store = DeviceStore::open(dir.path(), DAY).unwrap();
        let a = store.add("a", now).unwrap();
        let b = store.add("b", now).unwrap();
        assert!(store.revoke(&a).unwrap());
        assert!(store.check(&a, now).is_none());
        assert!(store.check(&b, now).is_some());
    }
}
