//! Virtual time shared by the engine and the fake MCP servers of an episode.

use std::sync::Arc;

use ptc_core::Timestamp;
use ptc_engine::adapters::VirtualClock;
use ptc_engine::ports::{Clock, VirtualTime};

/// Monday 2026-03-02T09:00:00Z: when every episode starts, in virtual time.
pub const T0: Timestamp = Timestamp(1_772_442_000_000);

/// One minute, one hour and one day, in milliseconds.
pub const MINUTE: u64 = 60_000;
pub const HOUR: u64 = 60 * MINUTE;
pub const DAY: u64 = 24 * HOUR;

/// The engine's virtual clock, shared: the fake servers read the time the engine is at, so a
/// deployment started by a script finishes an hour of virtual time later.
#[derive(Debug, Clone)]
pub struct SharedClock(Arc<VirtualClock>);

impl SharedClock {
    pub fn new(start: Timestamp) -> Self {
        Self(Arc::new(VirtualClock::new(start)))
    }
}

impl Clock for SharedClock {
    fn now(&self) -> Timestamp {
        self.0.now()
    }
}

impl VirtualTime for SharedClock {
    fn advance_to(&self, at: Timestamp) {
        self.0.advance_to(at);
    }
}

/// `2026-03-02T09:00:00Z`.
pub fn rfc3339(time: Timestamp) -> String {
    ptc_api::common::timestamp(time).to_string()
}

/// Reads an RFC 3339 time, as tools receive them.
pub fn parse_rfc3339(text: &str) -> Option<Timestamp> {
    let time: ptc_api::common::Timestamp = text.parse().ok()?;
    u64::try_from(time.as_millisecond()).ok().map(Timestamp)
}

/// Midnight UTC of a `YYYY-MM-DD` date.
pub fn parse_date(date: &str) -> Option<Timestamp> {
    if date.len() != 10 {
        return None;
    }
    parse_rfc3339(&format!("{date}T00:00:00Z"))
}

/// `2026-03-02`.
pub fn date_of(time: Timestamp) -> String {
    rfc3339(time)[..10].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t0_is_a_monday_morning_in_march() {
        assert_eq!(rfc3339(T0), "2026-03-02T09:00:00Z");
        assert_eq!(parse_rfc3339("2026-03-02T09:00:00Z"), Some(T0));
        assert_eq!(parse_date("2026-03-02"), Some(Timestamp(T0.0 - 9 * HOUR)));
        assert_eq!(date_of(T0), "2026-03-02");
        assert_eq!(parse_date("March 2"), None);
    }

    #[test]
    fn the_engine_and_the_servers_share_one_time() {
        let clock = SharedClock::new(T0);
        let servers = clock.clone();
        clock.advance_to(Timestamp(T0.0 + HOUR));
        assert_eq!(servers.now(), Timestamp(T0.0 + HOUR));
    }
}
