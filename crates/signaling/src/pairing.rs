//! One-time pairing codes shown on the host and typed into the browser.
//!
//! Pairing is closed unless the host user opens it (tray or console action)
//! for a short window, so nobody on the internet can start one or guess at
//! an unattended host. Too many wrong codes lock pairing until the host user
//! opens it again, and that lockout survives restarts.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::Error;

pub const CODE_TTL: Duration = Duration::from_secs(120);
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(120);
const ATTEMPTS_PER_CODE: u8 = 3;
/// Wrong codes in total (across codes and restarts) before pairing locks.
const MAX_FAILURES: u32 = 5;
const MAX_STARTS: usize = 5;
const STARTS_WINDOW: Duration = Duration::from_secs(60);
const STATE_FILE: &str = "pairing.json";

#[derive(Debug)]
struct Pending {
    id: String,
    code: String,
    device_name: String,
    expires: Instant,
    attempts_left: u8,
}

/// A code the host must show to its user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub pairing_id: String,
    pub code: String,
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// The host user has not opened pairing.
    Closed,
    /// Too many wrong codes; the host user must open pairing again.
    Locked,
    RateLimited,
    /// Another browser's code is still pending. The first request keeps the
    /// window, so a stranger cannot replace the code the host user is
    /// about to read out.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmError {
    BadCode {
        attempts_left: u8,
    },
    Expired,
    /// This wrong code was the last one allowed; pairing is now locked.
    Locked,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    failures: u32,
    locked: bool,
}

#[derive(Debug)]
pub struct Pairing {
    path: PathBuf,
    state: Persisted,
    open_until: Option<Instant>,
    pending: Option<Pending>,
    starts: VecDeque<Instant>,
}

impl Pairing {
    pub fn load(data_dir: &Path) -> Result<Self, Error> {
        let path = data_dir.join(STATE_FILE);
        let state = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| Error::Store(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            state,
            open_until: None,
            pending: None,
            starts: VecDeque::new(),
        })
    }

    /// Host-side action: accept pairing requests for `window`. Also clears a
    /// lockout, since the host user is evidently present.
    pub fn open(&mut self, now: Instant, window: Duration) -> Result<(), Error> {
        self.open_until = Some(now + window);
        self.pending = None;
        if self.state.failures != 0 || self.state.locked {
            self.state = Persisted::default();
            self.save()?;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        self.open_until = None;
        self.pending = None;
    }

    pub fn is_locked(&self) -> bool {
        self.state.locked
    }

    /// Only one code is pending at a time; a new one can start once it is
    /// confirmed, burned by wrong guesses, or expired.
    pub fn start(
        &mut self,
        device_name: &str,
        now: Instant,
    ) -> Result<Result<Started, StartError>, Error> {
        if self.state.locked {
            return Ok(Err(StartError::Locked));
        }
        if !self.open_until.is_some_and(|t| now < t) {
            return Ok(Err(StartError::Closed));
        }
        if self.pending.as_ref().is_some_and(|p| now < p.expires) {
            return Ok(Err(StartError::Busy));
        }
        while self
            .starts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= STARTS_WINDOW)
        {
            self.starts.pop_front();
        }
        if self.starts.len() >= MAX_STARTS {
            return Ok(Err(StartError::RateLimited));
        }
        self.starts.push_back(now);
        let started = Started {
            pairing_id: crate::store::random_token()?,
            code: random_code()?,
            device_name: clean_name(device_name),
        };
        self.pending = Some(Pending {
            id: started.pairing_id.clone(),
            code: started.code.clone(),
            device_name: started.device_name.clone(),
            expires: now + CODE_TTL,
            attempts_left: ATTEMPTS_PER_CODE,
        });
        Ok(Ok(started))
    }

    /// On success returns the device name, consumes the code and closes
    /// pairing.
    pub fn confirm(
        &mut self,
        pairing_id: &str,
        code: &str,
        now: Instant,
    ) -> Result<Result<String, ConfirmError>, Error> {
        if self.state.locked {
            return Ok(Err(ConfirmError::Locked));
        }
        let pending = match &mut self.pending {
            Some(p) if p.id.as_bytes().ct_eq(pairing_id.as_bytes()).into() => p,
            _ => return Ok(Err(ConfirmError::Expired)),
        };
        if now >= pending.expires {
            self.pending = None;
            return Ok(Err(ConfirmError::Expired));
        }
        if bool::from(pending.code.as_bytes().ct_eq(code.trim().as_bytes())) {
            let name = pending.device_name.clone();
            self.close();
            if self.state.failures != 0 {
                self.state.failures = 0;
                self.save()?;
            }
            return Ok(Ok(name));
        }
        pending.attempts_left -= 1;
        let attempts_left = pending.attempts_left;
        if attempts_left == 0 {
            self.pending = None;
        }
        self.state.failures += 1;
        let locked = self.state.failures >= MAX_FAILURES;
        if locked {
            self.state.locked = true;
            self.close();
        }
        self.save()?;
        Ok(Err(if locked {
            ConfirmError::Locked
        } else {
            ConfirmError::BadCode { attempts_left }
        }))
    }

    fn save(&self) -> Result<(), Error> {
        crate::store::write_private(
            &self.path,
            &serde_json::to_vec(&self.state).expect("state serializes"),
        )
    }
}

/// Six uniformly random digits.
fn random_code() -> Result<String, Error> {
    // Largest multiple of 10^6 that fits in u32, for rejection sampling.
    const LIMIT: u32 = u32::MAX - u32::MAX % 1_000_000;
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).map_err(|e| Error::Random(e.to_string()))?;
        let n = u32::from_le_bytes(b);
        if n < LIMIT {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

fn clean_name(name: &str) -> String {
    // The host user reads this name next to the code, so nothing invisible
    // (bidi overrides, zero-width marks) may make it look like another one.
    let cleaned: String = name
        .chars()
        .filter(|&c| ws_proto::names::printable(c))
        .take(64)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Unnamed browser".into()
    } else {
        cleaned.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrong(code: &str) -> &'static str {
        if code == "000000" { "111111" } else { "000000" }
    }

    fn open(dir: &Path, now: Instant) -> Pairing {
        let mut p = Pairing::load(dir).unwrap();
        p.open(now, DEFAULT_WINDOW).unwrap();
        p
    }

    #[test]
    fn closed_until_the_host_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = Pairing::load(dir.path()).unwrap();
        assert_eq!(p.start("x", now).unwrap(), Err(StartError::Closed));
        p.open(now, DEFAULT_WINDOW).unwrap();
        assert!(p.start("x", now).unwrap().is_ok());
        assert_eq!(
            p.start("x", now + DEFAULT_WINDOW).unwrap(),
            Err(StartError::Closed)
        );
    }

    #[test]
    fn right_code_pairs_once_and_closes_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        let s = p.start("Chrome\n on laptop", now).unwrap().unwrap();
        assert_eq!(s.code.len(), 6);
        assert_eq!(s.device_name, "Chrome on laptop");
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now).unwrap(),
            Ok("Chrome on laptop".into())
        );
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now).unwrap(),
            Err(ConfirmError::Expired)
        );
        assert_eq!(p.start("x", now).unwrap(), Err(StartError::Closed));
    }

    #[test]
    fn code_expires() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        let s = p.start("x", now).unwrap().unwrap();
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now + CODE_TTL).unwrap(),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn three_wrong_guesses_burn_the_code() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        let s = p.start("x", now).unwrap().unwrap();
        for left in [2, 1, 0] {
            assert_eq!(
                p.confirm(&s.pairing_id, wrong(&s.code), now).unwrap(),
                Err(ConfirmError::BadCode {
                    attempts_left: left
                })
            );
        }
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now).unwrap(),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn lockout_persists_until_the_host_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        let mut last = None;
        for _ in 0..MAX_FAILURES {
            let s = p.start("x", now).unwrap().unwrap();
            last = Some(p.confirm(&s.pairing_id, wrong(&s.code), now).unwrap());
            p.starts.clear(); // stay clear of the start rate limit
            p.pending = None; // and of the pending code
        }
        assert_eq!(last, Some(Err(ConfirmError::Locked)));
        assert_eq!(p.start("x", now).unwrap(), Err(StartError::Locked));

        // A restart does not clear it.
        let mut p = Pairing::load(dir.path()).unwrap();
        assert!(p.is_locked());
        assert_eq!(p.start("x", now).unwrap(), Err(StartError::Locked));
        // The host user opening pairing does.
        p.open(now, DEFAULT_WINDOW).unwrap();
        assert!(p.start("x", now).unwrap().is_ok());
        assert!(!Pairing::load(dir.path()).unwrap().is_locked());
    }

    #[test]
    fn starts_are_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        for _ in 0..MAX_STARTS {
            assert!(p.start("x", now).unwrap().is_ok());
            p.pending = None;
        }
        assert_eq!(p.start("x", now).unwrap(), Err(StartError::RateLimited));
    }

    #[test]
    fn first_pending_code_keeps_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut p = open(dir.path(), now);
        let first = p.start("mine", now).unwrap().unwrap();
        assert_eq!(p.start("theirs", now).unwrap(), Err(StartError::Busy));
        assert_eq!(
            p.confirm(&first.pairing_id, &first.code, now).unwrap(),
            Ok("mine".into())
        );

        // Once the code expires, a new request may start.
        let mut p = Pairing::load(dir.path()).unwrap();
        p.open(now, 2 * CODE_TTL).unwrap();
        p.start("x", now).unwrap().unwrap();
        assert!(p.start("y", now + CODE_TTL).unwrap().is_ok());
    }

    #[test]
    fn device_names_lose_invisible_characters() {
        assert_eq!(clean_name("Logan\u{202E}enohp\u{200B}\n"), "Loganenohp");
        assert_eq!(clean_name("\u{2066}\u{FEFF} "), "Unnamed browser");
        assert_eq!(clean_name("Pixel 9 Café"), "Pixel 9 Café");
    }
}
