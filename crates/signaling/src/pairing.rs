//! One-time pairing codes shown on the host and typed into the browser.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use crate::Error;

pub const CODE_TTL: Duration = Duration::from_secs(120);
const ATTEMPTS_PER_CODE: u8 = 3;
const MAX_STARTS: usize = 5;
const STARTS_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURES: usize = 5;
const FAILURES_WINDOW: Duration = Duration::from_secs(3600);

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
pub enum ConfirmError {
    BadCode { attempts_left: u8 },
    Expired,
    RateLimited,
}

/// Only one pairing is in flight at a time; starting another replaces it.
/// Failed guesses are limited across all pairings, so restarting does not
/// buy an attacker more attempts.
#[derive(Debug, Default)]
pub struct Pairing {
    pending: Option<Pending>,
    starts: VecDeque<Instant>,
    failures: VecDeque<Instant>,
}

impl Pairing {
    /// `None` when rate-limited.
    pub fn start(&mut self, device_name: &str, now: Instant) -> Result<Option<Started>, Error> {
        prune(&mut self.starts, now, STARTS_WINDOW);
        prune(&mut self.failures, now, FAILURES_WINDOW);
        if self.starts.len() >= MAX_STARTS || self.failures.len() >= MAX_FAILURES {
            return Ok(None);
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
        Ok(Some(started))
    }

    /// On success returns the device name and consumes the pairing.
    pub fn confirm(
        &mut self,
        pairing_id: &str,
        code: &str,
        now: Instant,
    ) -> Result<String, ConfirmError> {
        prune(&mut self.failures, now, FAILURES_WINDOW);
        if self.failures.len() >= MAX_FAILURES {
            return Err(ConfirmError::RateLimited);
        }
        let pending = match &mut self.pending {
            Some(p) if p.id.as_bytes().ct_eq(pairing_id.as_bytes()).into() => p,
            _ => return Err(ConfirmError::Expired),
        };
        if now >= pending.expires {
            self.pending = None;
            return Err(ConfirmError::Expired);
        }
        if bool::from(pending.code.as_bytes().ct_eq(code.trim().as_bytes())) {
            let name = pending.device_name.clone();
            self.pending = None;
            return Ok(name);
        }
        self.failures.push_back(now);
        pending.attempts_left -= 1;
        let attempts_left = pending.attempts_left;
        if attempts_left == 0 {
            self.pending = None;
        }
        Err(ConfirmError::BadCode { attempts_left })
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }
}

fn prune(times: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while times
        .front()
        .is_some_and(|t| now.duration_since(*t) >= window)
    {
        times.pop_front();
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
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
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

    #[test]
    fn right_code_pairs_once() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let s = p.start("Chrome\n on laptop", now).unwrap().unwrap();
        assert_eq!(s.code.len(), 6);
        assert_eq!(s.device_name, "Chrome on laptop");
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now),
            Ok("Chrome on laptop".into())
        );
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn code_expires() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let s = p.start("x", now).unwrap().unwrap();
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now + CODE_TTL),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn three_wrong_guesses_burn_the_code() {
        let mut p = Pairing::default();
        let now = Instant::now();
        let s = p.start("x", now).unwrap().unwrap();
        let wrong = if s.code == "000000" {
            "111111"
        } else {
            "000000"
        };
        assert_eq!(
            p.confirm(&s.pairing_id, wrong, now),
            Err(ConfirmError::BadCode { attempts_left: 2 })
        );
        assert_eq!(
            p.confirm(&s.pairing_id, wrong, now),
            Err(ConfirmError::BadCode { attempts_left: 1 })
        );
        assert_eq!(
            p.confirm(&s.pairing_id, wrong, now),
            Err(ConfirmError::BadCode { attempts_left: 0 })
        );
        assert_eq!(
            p.confirm(&s.pairing_id, &s.code, now),
            Err(ConfirmError::Expired)
        );
    }

    #[test]
    fn failures_are_limited_across_restarts() {
        let mut p = Pairing::default();
        let mut now = Instant::now();
        let mut failures = 0;
        while failures < MAX_FAILURES {
            now += STARTS_WINDOW; // stay clear of the start limit
            let s = p.start("x", now).unwrap().unwrap();
            let wrong = if s.code == "000000" {
                "111111"
            } else {
                "000000"
            };
            assert!(matches!(
                p.confirm(&s.pairing_id, wrong, now),
                Err(ConfirmError::BadCode { .. })
            ));
            failures += 1;
        }
        assert_eq!(p.start("x", now).unwrap(), None);
        assert_eq!(
            p.confirm("any", "000000", now),
            Err(ConfirmError::RateLimited)
        );
        // The window slides.
        assert!(p.start("x", now + FAILURES_WINDOW).unwrap().is_some());
    }

    #[test]
    fn starts_are_rate_limited() {
        let mut p = Pairing::default();
        let now = Instant::now();
        for _ in 0..MAX_STARTS {
            assert!(p.start("x", now).unwrap().is_some());
        }
        assert_eq!(p.start("x", now).unwrap(), None);
    }
}
