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
//!
//! [`Stamped`] is a cached value and its stamp in one. The value is
//! private and [`offer`](Stamped::offer) is the only way in, so a store
//! that does not ask the stamp cannot be written. A bare [`ReadStamp`] is
//! for a driver whose answers move the stamp with no value to store.

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

/// A cached value and the instant it was read.
///
/// The value is private: [`offer`](Self::offer) is the only way in, so a
/// store cannot skip the stamp. `T` is what one read brings back: a flag, a
/// status frame, the readings one reply carries together. A reading the
/// device can report as absent is `T = Option<_>`, which keeps "never read"
/// (an empty cache) apart from "read, and absent" (a stored `None`).
#[derive(Debug, Clone)]
pub struct Stamped<T> {
    value: Option<T>,
    stamp: ReadStamp,
}

/// The empty cache. Not derived, so that it asks nothing of `T`.
impl<T> Default for Stamped<T> {
    fn default() -> Self {
        Self {
            value: None,
            stamp: ReadStamp::default(),
        }
    }
}

impl<T> Stamped<T> {
    /// Store `value`, read (or set by a command answered) at `read_at`,
    /// unless the held value was read strictly later.
    ///
    /// Says whether the value was stored, so that what follows a store
    /// runs only after one. A read at the held instant is stored, as
    /// [`ReadStamp::admit`] admits it.
    #[must_use = "a refused value was not stored"]
    pub fn offer(&mut self, value: T, read_at: Instant) -> bool {
        let stored = self.stamp.admit(read_at);
        if stored {
            self.value = Some(value);
        }
        stored
    }

    /// The held value, or `None` before the first stored offer.
    #[must_use]
    pub const fn get(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// When the held value was read, or `None` before the first stored
    /// offer.
    #[must_use]
    pub const fn read_at(&self) -> Option<Instant> {
        self.stamp.read_at()
    }
}

impl<T: Copy> Stamped<T> {
    /// A copy of the held value, or `None` before the first stored offer.
    #[must_use]
    pub const fn value(&self) -> Option<T> {
        self.value
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

    #[test]
    fn an_empty_cache_has_no_value_and_no_instant() {
        let cache = Stamped::<u16>::default();
        assert_eq!(cache.get(), None);
        assert_eq!(cache.value(), None);
        assert_eq!(cache.read_at(), None);
    }

    #[test]
    fn the_first_offer_is_stored() {
        let mut cache = Stamped::default();
        let read_at = Instant::now();
        assert!(cache.offer(7_u16, read_at));
        assert_eq!(cache.value(), Some(7));
        assert_eq!(cache.read_at(), Some(read_at));
    }

    #[test]
    fn an_earlier_offer_is_refused_and_leaves_the_value_and_the_instant_alone() {
        let mut cache = Stamped::default();
        let earlier = Instant::now();
        let later = earlier + Duration::from_millis(1);
        assert!(cache.offer(7_u16, later));
        assert!(!cache.offer(3, earlier));
        assert_eq!(cache.value(), Some(7));
        assert_eq!(cache.read_at(), Some(later));
    }

    #[test]
    fn an_offer_at_the_held_instant_is_stored() {
        let mut cache = Stamped::default();
        let read_at = Instant::now();
        assert!(cache.offer(7_u16, read_at));
        assert!(cache.offer(3, read_at));
        assert_eq!(cache.value(), Some(3));
        assert_eq!(cache.read_at(), Some(read_at));
    }

    #[test]
    fn a_later_offer_replaces_the_value_and_the_instant() {
        let mut cache = Stamped::default();
        let earlier = Instant::now();
        let later = earlier + Duration::from_millis(1);
        assert!(cache.offer(7_u16, earlier));
        assert!(cache.offer(3, later));
        assert_eq!(cache.value(), Some(3));
        assert_eq!(cache.read_at(), Some(later));
    }

    #[test]
    fn a_reading_reported_absent_is_stored_as_a_value() {
        let mut cache = Stamped::<Option<f64>>::default();
        assert!(cache.offer(None, Instant::now()));
        assert_eq!(cache.value(), Some(None));
    }

    #[test]
    fn a_value_that_is_neither_copy_nor_default_is_read_by_reference() {
        #[derive(Debug, PartialEq)]
        struct Frame(String);

        let mut cache = Stamped::default();
        assert!(cache.offer(Frame("status".to_string()), Instant::now()));
        assert_eq!(cache.get(), Some(&Frame("status".to_string())));
    }
}
