//! A strictly increasing microsecond clock and Positorium time formatting.

use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, TimeZone, Utc};

/// Hands out strictly increasing UTC timestamps with microsecond precision, so
/// two writes in one process never share a time ("latest" stays well defined).
#[derive(Debug, Default)]
pub struct Clock {
    last: AtomicI64,
}

impl Clock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current time, strictly after every earlier result of this clock.
    pub fn now(&self) -> DateTime<Utc> {
        let wall = Utc::now().timestamp_micros();
        let mut prev = self.last.load(Ordering::Relaxed);
        loop {
            let next = wall.max(prev + 1);
            match self.last.compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => return micros(next),
                Err(actual) => prev = actual,
            }
        }
    }
}

fn micros(us: i64) -> DateTime<Utc> {
    Utc.timestamp_micros(us).single().expect("valid timestamp")
}

/// `2026-11-20 14:03:07.123456`, the body of a Traqula time token.
pub fn format(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

/// Parse a Traqula time (with or without surrounding quotes, date-only allowed).
pub fn parse(text: &str) -> Option<DateTime<Utc>> {
    let t = text.trim().trim_matches('\'');
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(t, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok().map(|d| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

/// One microsecond before `t` (the cut "just before" an event).
pub fn just_before(t: DateTime<Utc>) -> DateTime<Utc> {
    micros(t.timestamp_micros() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strictly_increasing_and_round_trips() {
        let c = Clock::new();
        let a = c.now();
        let b = c.now();
        assert!(b > a);
        let text = format(b);
        assert_eq!(parse(&text), Some(b));
        assert_eq!(parse(&format!("'{text}'")), Some(b));
        assert_eq!(format(just_before(b)).len(), 26);
    }
}
