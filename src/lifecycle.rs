// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! When the daemon may stop by itself (#114).
//!
//! The daemon stops once it has been idle for the idle period: no request
//! arrived, no data came in on any port, and no long-lived client holds a
//! lease. Listening counts as idle, an open but silent port as well as a
//! listening HTTP interface; only something actually happening resets the
//! timer. A lease is how a long-lived client (a window, the terminal monitor,
//! an MCP session, `read --follow`) says it is still there, since every client
//! opens a fresh connection per request and there is no standing connection to
//! watch.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::protocol::{IdleSource, LEASE_TTL_SECS, LeaseKind, LifecycleState};

/// The idle period when nothing sets another.
pub const DEFAULT_IDLE: Duration = Duration::from_mins(15);

/// A length of time as people write it: `90s`, `15m`, `2h`, or `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeout(pub Duration);

impl Default for IdleTimeout {
    fn default() -> Self {
        Self(DEFAULT_IDLE)
    }
}

impl FromStr for IdleTimeout {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text == "0" {
            return Ok(Self(Duration::ZERO));
        }
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("'{text}' needs a unit: s, m or h, as in 15m"))?;
        let (number, unit) = text.split_at(split);
        let number: u64 = number
            .parse()
            .map_err(|_| format!("'{text}' does not start with a number"))?;
        let seconds = match unit {
            "s" => Some(number),
            "m" => number.checked_mul(60),
            "h" => number.checked_mul(3600),
            _ => return Err(format!("'{unit}' is not a unit; use s, m or h, as in 15m")),
        }
        .ok_or_else(|| format!("'{text}' is too long"))?;
        Ok(Self(Duration::from_secs(seconds)))
    }
}

impl fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        match seconds {
            0 => f.write_str("0"),
            s if s % 3600 == 0 => write!(f, "{}h", s / 3600),
            s if s % 60 == 0 => write!(f, "{}m", s / 60),
            s => write!(f, "{s}s"),
        }
    }
}

impl Serialize for IdleTimeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IdleTimeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A remaining time as people read it: `14m 59s`, `2h 5m`, `40s`.
///
/// Unlike [`IdleTimeout`]'s form this is never parsed back, so it may combine
/// units and drop the smallest one once hours are involved.
#[must_use]
pub fn format_remaining(seconds: u64) -> String {
    let (hours, minutes, secs) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes, secs) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

/// Where the current time comes from, so tests need not wait.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct Inner {
    period: Duration,
    source: IdleSource,
    last_activity: Instant,
    leases: HashMap<String, (LeaseKind, Instant)>,
    last_lines: Option<u64>,
    /// Requests being served right now.
    in_flight: usize,
}

/// The daemon's idle timer and its leases. Shared by every clone of the
/// engine.
pub struct Lifecycle {
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

impl fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lifecycle").finish_non_exhaustive()
    }
}

impl Lifecycle {
    /// A timer that starts now.
    #[must_use]
    pub fn new(period: Duration, source: IdleSource, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            clock,
            inner: Mutex::new(Inner {
                period,
                source,
                last_activity: now,
                leases: HashMap::new(),
                last_lines: None,
                in_flight: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Something happened: a request arrived.
    pub fn touch(&self) {
        let now = self.clock.now();
        self.lock().last_activity = now;
    }

    /// A request is being served. Until the returned guard is dropped the
    /// daemon does not stop through idleness, and the idle period starts
    /// again when it is: a file transfer or a flash may take longer than the
    /// idle period without a line arriving.
    #[must_use]
    pub fn serving(&self) -> Serving<'_> {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.in_flight += 1;
        inner.last_activity = now;
        drop(inner);
        Serving { lifecycle: self }
    }

    /// Report the total number of captured lines across all ports. A change
    /// since the last report means data arrived, which resets the timer.
    pub fn observe_lines(&self, total: u64) {
        let now = self.clock.now();
        let mut inner = self.lock();
        if inner.last_lines.is_some_and(|seen| seen != total) {
            inner.last_activity = now;
        }
        inner.last_lines = Some(total);
    }

    /// Create or extend a lease.
    pub fn renew(&self, client: &str, kind: LeaseKind) {
        let expires = self.clock.now() + Duration::from_secs(LEASE_TTL_SECS);
        self.lock()
            .leases
            .insert(client.to_string(), (kind, expires));
    }

    /// End a lease. Releasing counts as activity: the idle period starts
    /// when the last client leaves, not when it last renewed.
    pub fn release(&self, client: &str) {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.remove(client);
        inner.last_activity = now;
    }

    /// Change the idle period, recording where the new value came from.
    pub fn set_period(&self, period: Duration, source: IdleSource) {
        let mut inner = self.lock();
        inner.period = period;
        inner.source = source;
    }

    /// The state as a client sees it. Lapsed leases are dropped first.
    #[must_use]
    pub fn state(&self) -> LifecycleState {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.retain(|_, (_, expires)| *expires > now);
        let mut leases: Vec<LeaseKind> = inner.leases.values().map(|(kind, _)| *kind).collect();
        leases.sort();
        let stops_in_seconds = (inner.period > Duration::ZERO && leases.is_empty()).then(|| {
            inner
                .period
                .saturating_sub(now.saturating_duration_since(inner.last_activity))
                .as_secs()
        });
        LifecycleState {
            idle_seconds: inner.period.as_secs(),
            source: inner.source,
            stops_in_seconds,
            leases,
        }
    }

    /// Whether the daemon should stop now.
    #[must_use]
    pub fn idle_expired(&self) -> bool {
        let now = self.clock.now();
        let mut inner = self.lock();
        inner.leases.retain(|_, (_, expires)| *expires > now);
        inner.period > Duration::ZERO
            && inner.leases.is_empty()
            && inner.in_flight == 0
            && now.saturating_duration_since(inner.last_activity) >= inner.period
    }
}

/// A request being served; see [`Lifecycle::serving`].
pub struct Serving<'a> {
    lifecycle: &'a Lifecycle,
}

impl Drop for Serving<'_> {
    fn drop(&mut self) {
        let now = self.lifecycle.clock.now();
        let mut inner = self.lifecycle.lock();
        inner.in_flight = inner.in_flight.saturating_sub(1);
        inner.last_activity = now;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A clock that moves only when told to.
    pub struct ManualClock {
        start: Instant,
        offset: Mutex<Duration>,
    }

    impl ManualClock {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                start: Instant::now(),
                offset: Mutex::new(Duration::ZERO),
            })
        }

        pub fn advance(&self, by: Duration) {
            *self.offset.lock().unwrap() += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.start + *self.offset.lock().unwrap()
        }
    }

    const MIN: Duration = Duration::from_secs(60);
    /// One second short of fifteen minutes.
    const ALMOST_15: Duration = Duration::from_secs(15 * 60 - 1);

    fn lifecycle(minutes: u32) -> (Lifecycle, Arc<ManualClock>) {
        let clock = ManualClock::new();
        (
            Lifecycle::new(MIN * minutes, IdleSource::Default, clock.clone()),
            clock,
        )
    }

    #[test]
    fn durations_read_and_print_as_people_write_them() {
        for (text, seconds) in [("0", 0), ("90s", 90), ("15m", 900), ("2h", 7200)] {
            let parsed: IdleTimeout = text.parse().expect(text);
            assert_eq!(parsed.0.as_secs(), seconds, "{text}");
            assert_eq!(parsed.to_string(), text, "{text}");
        }
        assert_eq!("120s".parse::<IdleTimeout>().unwrap().to_string(), "2m");
        for bad in ["", "15", "m", "15x", "-1m", "1.5h"] {
            assert!(
                bad.parse::<IdleTimeout>().is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_remaining_time_reads_as_people_say_it() {
        for (seconds, text) in [
            (40, "40s"),
            (900, "15m"),
            (899, "14m 59s"),
            (7200, "2h"),
            (7500, "2h 5m"),
        ] {
            assert_eq!(format_remaining(seconds), text);
        }
    }

    #[test]
    fn the_daemon_stops_after_the_idle_period_and_not_before() {
        let (life, clock) = lifecycle(15);
        clock.advance(ALMOST_15);
        assert!(!life.idle_expired());
        clock.advance(Duration::from_secs(1));
        assert!(life.idle_expired());
    }

    #[test]
    fn a_request_and_arriving_data_each_reset_the_timer() {
        let (life, clock) = lifecycle(15);
        clock.advance(14 * MIN);
        life.touch();
        clock.advance(14 * MIN);
        assert!(!life.idle_expired(), "a request resets");

        life.observe_lines(10);
        clock.advance(2 * MIN);
        life.observe_lines(12);
        clock.advance(14 * MIN);
        assert!(!life.idle_expired(), "data resets");

        // The same count again is a silent port, which is idle.
        life.observe_lines(12);
        clock.advance(MIN);
        assert!(life.idle_expired(), "silence does not reset");
    }

    #[test]
    fn a_lease_holds_the_daemon_until_it_lapses_or_is_released() {
        let (life, clock) = lifecycle(15);
        life.renew("gui-1", LeaseKind::Gui);
        clock.advance(60 * MIN);
        // Lapsed: three minutes without renewal.
        assert!(life.idle_expired(), "an unrenewed lease lapses");

        let (life, clock) = lifecycle(15);
        for _ in 0..60 {
            life.renew("tui-1", LeaseKind::Tui);
            clock.advance(MIN);
        }
        assert!(!life.idle_expired(), "renewed every minute, it holds");
        assert_eq!(life.state().leases, vec![LeaseKind::Tui]);
        assert_eq!(life.state().stops_in_seconds, None);

        life.release("tui-1");
        clock.advance(ALMOST_15);
        assert!(
            !life.idle_expired(),
            "the idle period starts at the release"
        );
        clock.advance(Duration::from_secs(1));
        assert!(life.idle_expired());
    }

    /// A request longer than the idle period holds the daemon until it ends,
    /// and the idle period starts again from its end.
    #[test]
    fn a_request_being_served_holds_the_daemon() {
        let (lifecycle, clock) = lifecycle(15);
        let serving = lifecycle.serving();
        clock.advance(Duration::from_mins(40));
        assert!(!lifecycle.idle_expired(), "stopped while serving a request");

        drop(serving);
        clock.advance(Duration::from_mins(14));
        assert!(
            !lifecycle.idle_expired(),
            "the period did not restart at the end"
        );
        clock.advance(Duration::from_mins(1));
        assert!(lifecycle.idle_expired());
    }

    #[test]
    fn zero_means_never() {
        let (life, clock) = lifecycle(0);
        clock.advance(1000 * MIN);
        assert!(!life.idle_expired());
        assert_eq!(life.state().stops_in_seconds, None);
    }

    #[test]
    fn the_state_names_period_source_and_remaining_time() {
        let (life, clock) = lifecycle(15);
        clock.advance(5 * MIN);
        life.set_period(30 * MIN, IdleSource::Runtime);
        let state = life.state();
        assert_eq!(state.idle_seconds, 1800);
        assert_eq!(state.source, IdleSource::Runtime);
        assert_eq!(state.stops_in_seconds, Some(1500));
    }
}
