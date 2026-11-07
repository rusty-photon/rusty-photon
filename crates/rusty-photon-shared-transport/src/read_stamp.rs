//! Keeping the value read last when stores land out of wire order.
//!
//! Requests take turns on the wire: the command lock runs each exchange
//! end to end. But a caller stores what its request brought back after
//! its turn ends, outside the lock, so two stores can land in the
//! opposite order to the reads behind them. A poll task that is slow to
//! run again stores the frame it read just before a write after the
//! write's readback, and puts the old value back; a poll exchange that
//! is on the wire while a request commits its own belief to the cache
//! stores over it the moment the exchange ends.
//!
//! [`ReadStamp`] keeps the order. A cached value carries the instant it
//! was read — [`WireTiming::received_at`](crate::WireTiming::received_at)
//! of the request that brought it, or of the command whose effect the
//! driver records — and a store dated before it is refused. Exchanges on
//! one connection never overlap, so `received_at` puts every read in wire
//! order; a reconnect's reads come later still.

use tokio::time::Instant;

/// The instant a cached value was read off the wire, and the rule that
/// keeps the value read last.
///
/// One stamp dates every field one read or one command stores together.
/// A driver offers each store with the instant behind it, and stores it
/// only if [`admit`](Self::admit) says yes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStamp(Option<Instant>);

impl ReadStamp {
    /// Whether a value read at `read_at` may replace the one this stamp
    /// dates — yes unless that one was read strictly later — and, if so,
    /// date the held value `read_at`.
    ///
    /// A refused value is older than the held one and must not be stored,
    /// side effects included. Only a strictly earlier read is refused, so
    /// reads at one instant — every read on a paused test clock — are
    /// admitted in turn.
    #[must_use = "a refused value must not be stored"]
    pub fn admit(&mut self, read_at: Instant) -> bool {
        if self.0.is_some_and(|held| read_at < held) {
            return false;
        }
        self.0 = Some(read_at);
        true
    }

    /// When the held value was read, or `None` before the first admitted
    /// read.
    #[must_use]
    pub const fn read_at(&self) -> Option<Instant> {
        self.0
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_first_read_is_admitted_and_dates_the_value() {
        let mut stamp = ReadStamp::default();
        let read_at = Instant::now();
        assert!(stamp.admit(read_at));
        assert_eq!(stamp.read_at(), Some(read_at));
    }

    #[test]
    fn a_later_read_is_admitted() {
        let mut stamp = ReadStamp::default();
        let earlier = Instant::now();
        let later = earlier + Duration::from_millis(1);
        assert!(stamp.admit(earlier));
        assert!(stamp.admit(later));
        assert_eq!(stamp.read_at(), Some(later));
    }

    #[test]
    fn an_earlier_read_is_refused_and_leaves_the_stamp_alone() {
        let mut stamp = ReadStamp::default();
        let earlier = Instant::now();
        let later = earlier + Duration::from_millis(1);
        assert!(stamp.admit(later));
        assert!(!stamp.admit(earlier));
        assert_eq!(stamp.read_at(), Some(later));
    }

    #[test]
    fn a_read_at_the_held_instant_is_admitted() {
        let mut stamp = ReadStamp::default();
        let read_at = Instant::now();
        assert!(stamp.admit(read_at));
        assert!(stamp.admit(read_at));
    }
}
