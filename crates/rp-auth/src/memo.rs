//! The verification memo: recognises a credential the layer has already
//! proved, so the Argon2id KDF runs once per credential instead of once per
//! request.
//!
//! Pure and synchronous. Time is injected, so the TTL logic is testable
//! without a runtime, and nothing here touches argon2 or the OS RNG.
//!
//! A credential is reduced to a [`Tag`]: a keyed BLAKE2b-256 MAC over a
//! domain string and the length-prefixed username, password and stored PHC
//! string, under a key that lives only in this process. The memo holds one
//! positive slot — one `AuthConfig` admits exactly one credential, so there
//! is nothing to evict — and a small ring of negative tags. Tags are PRF
//! outputs: without the key they are not a cracking target, and even with
//! the key a hit still requires presenting the full credential.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use blake2::digest::consts::U32;
use blake2::digest::{KeyInit, Mac};
use blake2::Blake2bMac;
use subtle::{Choice, ConstantTimeEq};
use zeroize::{Zeroize, Zeroizing};

/// Sliding idle TTL of the positive slot. A poller that keeps presenting the
/// credential never re-pays the KDF; a client that goes quiet for this long
/// pays one KDF when it returns.
pub const POSITIVE_IDLE_TTL: Duration = Duration::from_mins(15);

/// Fixed TTL of a negative tag: the identical wrong credential answers 401
/// without a KDF for this long, so a stale poller with an old password costs
/// one KDF per window instead of one per poll.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(5);

/// Capacity of the negative ring. Beyond this the oldest entry is evicted; a
/// flood of distinct wrong passwords can only ever evict other negatives.
pub const NEGATIVE_CAP: usize = 8;

/// Domain separation for the tag input, so a future tag format can never
/// collide with this one.
const DOMAIN: &[u8] = b"rp-auth-tag-v1\0";

/// The per-layer MAC key. 64 bytes is the full `BLAKE2b` key size.
pub type MacKey = Zeroizing<[u8; 64]>;

/// A keyed BLAKE2b-256 tag of one presented credential. Zeroized on drop.
#[derive(Clone)]
pub struct Tag([u8; 32]);

impl Drop for Tag {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Tag {
    /// Reduce a presented credential to its tag under `key`.
    ///
    /// Every field is prefixed with its length as a little-endian `u64`, so
    /// `("ab", "c")` and `("a", "bc")` can never share a tag, and the stored
    /// PHC string is bound in so a tag proved against one hash is worthless
    /// against another.
    #[must_use]
    pub fn compute(key: &MacKey, username: &[u8], password: &[u8], phc: &[u8]) -> Self {
        let mut mac = <Blake2bMac<U32> as KeyInit>::new(&(**key).into());
        mac.update(DOMAIN);
        for field in [username, password, phc] {
            mac.update(&len_prefix(field));
            mac.update(field);
        }
        Self(mac.finalize().into_bytes().into())
    }

    /// Constant-time equality of two tags.
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> Choice {
        self.0.ct_eq(&other.0)
    }
}

/// The LE64 length prefix of one field. A `usize` always fits a `u64` on the
/// targets this workspace builds for; the fallback only exists to keep the
/// conversion total.
fn len_prefix(field: &[u8]) -> [u8; 8] {
    u64::try_from(field.len()).unwrap_or(u64::MAX).to_le_bytes()
}

/// The outcome of a memo lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// This exact credential was proved within the TTL: allow without a KDF.
    Hit,
    /// This exact credential was refuted within the TTL: deny without a KDF.
    NegativeHit,
    /// Unknown: the KDF has to run.
    Miss,
}

struct Positive {
    tag: Tag,
    last_hit: Instant,
}

struct Negative {
    tag: Tag,
    inserted: Instant,
}

/// One positive slot plus a bounded negative ring.
pub struct Memo {
    positive: Option<Positive>,
    negative: VecDeque<Negative>,
}

impl Default for Memo {
    fn default() -> Self {
        Self::new()
    }
}

impl Memo {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            positive: None,
            negative: VecDeque::new(),
        }
    }

    /// Look `tag` up at time `now`. Expired entries are dropped first (an
    /// expired positive slot is cleared, and its tag zeroized, rather than
    /// merely skipped); the scans compare every remaining entry in constant
    /// time without an early exit. A positive hit refreshes the sliding TTL.
    pub fn lookup(&mut self, tag: &Tag, now: Instant) -> Lookup {
        self.expire(now);
        let positive_hit = self
            .positive
            .as_ref()
            .map_or_else(|| Choice::from(0), |p| p.tag.ct_eq(tag));
        let mut negative_hit = Choice::from(0);
        for entry in &self.negative {
            negative_hit |= entry.tag.ct_eq(tag);
        }
        if bool::from(positive_hit) {
            if let Some(p) = self.positive.as_mut() {
                p.last_hit = now;
            }
            return Lookup::Hit;
        }
        if bool::from(negative_hit) {
            return Lookup::NegativeHit;
        }
        Lookup::Miss
    }

    /// Record a completed verdict for `tag` at time `now`. Only a completed
    /// KDF comparison may call this; a verdict that was never computed must
    /// store nothing.
    pub fn store(&mut self, tag: Tag, ok: bool, now: Instant) {
        if ok {
            self.positive = Some(Positive { tag, last_hit: now });
            return;
        }
        self.expire(now);
        while self.negative.len() >= NEGATIVE_CAP {
            self.negative.pop_front();
        }
        self.negative.push_back(Negative { tag, inserted: now });
    }

    fn expire(&mut self, now: Instant) {
        if self
            .positive
            .as_ref()
            .is_some_and(|p| now.saturating_duration_since(p.last_hit) >= POSITIVE_IDLE_TTL)
        {
            self.positive = None;
        }
        self.negative
            .retain(|n| now.saturating_duration_since(n.inserted) < NEGATIVE_TTL);
    }

    /// Whether the positive slot currently holds a tag (test introspection).
    #[cfg(test)]
    pub(crate) const fn has_positive(&self) -> bool {
        self.positive.is_some()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn key(byte: u8) -> MacKey {
        Zeroizing::new([byte; 64])
    }

    fn tag(username: &str, password: &str) -> Tag {
        Tag::compute(
            &key(7),
            username.as_bytes(),
            password.as_bytes(),
            b"$argon2id$stub",
        )
    }

    fn after(base: Instant, d: Duration) -> Instant {
        base.checked_add(d).unwrap()
    }

    #[test]
    fn tag_is_deterministic_for_the_same_input() {
        assert_eq!(tag("u", "p").0, tag("u", "p").0);
    }

    #[test]
    fn tag_changes_with_the_key() {
        let a = Tag::compute(&key(1), b"u", b"p", b"h");
        let b = Tag::compute(&key(2), b"u", b"p", b"h");
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn tag_changes_with_the_password() {
        assert_ne!(tag("u", "p").0, tag("u", "q").0);
    }

    #[test]
    fn tag_changes_with_the_username() {
        assert_ne!(tag("u", "p").0, tag("v", "p").0);
    }

    #[test]
    fn tag_changes_with_the_stored_hash() {
        let a = Tag::compute(&key(7), b"u", b"p", b"$argon2id$one");
        let b = Tag::compute(&key(7), b"u", b"p", b"$argon2id$two");
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn tag_framing_distinguishes_a_shifted_field_boundary() {
        // Without length prefixes "ab"+"c" and "a"+"bc" would MAC identically.
        assert_ne!(tag("ab", "c").0, tag("a", "bc").0);
    }

    #[test]
    fn tag_framing_distinguishes_an_empty_field_from_a_moved_one() {
        let a = Tag::compute(&key(7), b"", b"up", b"h");
        let b = Tag::compute(&key(7), b"u", b"p", b"h");
        assert_ne!(a.0, b.0);
    }

    #[test]
    fn empty_memo_misses() {
        let mut memo = Memo::new();
        assert_eq!(memo.lookup(&tag("u", "p"), Instant::now()), Lookup::Miss);
    }

    #[test]
    fn stored_positive_hits() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), true, now);
        assert_eq!(memo.lookup(&tag("u", "p"), now), Lookup::Hit);
    }

    #[test]
    fn stored_positive_does_not_match_another_credential() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), true, now);
        assert_eq!(memo.lookup(&tag("u", "q"), now), Lookup::Miss);
    }

    #[test]
    fn positive_expires_after_the_idle_ttl_and_the_slot_is_cleared() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), true, now);
        let later = after(now, POSITIVE_IDLE_TTL);
        assert_eq!(memo.lookup(&tag("u", "p"), later), Lookup::Miss);
        assert!(
            !memo.has_positive(),
            "an expired slot must be cleared, not skipped"
        );
    }

    #[test]
    fn positive_ttl_slides_on_every_hit() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), true, now);
        let step = POSITIVE_IDLE_TTL
            .checked_sub(Duration::from_secs(1))
            .unwrap();
        let t1 = after(now, step);
        assert_eq!(memo.lookup(&tag("u", "p"), t1), Lookup::Hit);
        // Past the original expiry, but within the window refreshed at t1.
        let t2 = after(t1, step);
        assert_eq!(memo.lookup(&tag("u", "p"), t2), Lookup::Hit);
    }

    #[test]
    fn stored_negative_hits_negatively() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "wrong"), false, now);
        assert_eq!(memo.lookup(&tag("u", "wrong"), now), Lookup::NegativeHit);
    }

    #[test]
    fn negative_expires_after_its_ttl() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "wrong"), false, now);
        let later = after(now, NEGATIVE_TTL);
        assert_eq!(memo.lookup(&tag("u", "wrong"), later), Lookup::Miss);
    }

    #[test]
    fn negative_ring_evicts_the_oldest_beyond_capacity() {
        let now = Instant::now();
        let mut memo = Memo::new();
        for i in 0..NEGATIVE_CAP {
            memo.store(tag("u", &format!("wrong{i}")), false, now);
        }
        assert_eq!(memo.lookup(&tag("u", "wrong0"), now), Lookup::NegativeHit);
        memo.store(tag("u", "one-more"), false, now);
        assert_eq!(memo.lookup(&tag("u", "wrong0"), now), Lookup::Miss);
        assert_eq!(memo.lookup(&tag("u", "wrong1"), now), Lookup::NegativeHit);
        assert_eq!(memo.lookup(&tag("u", "one-more"), now), Lookup::NegativeHit);
    }

    #[test]
    fn negatives_never_evict_the_positive_slot() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), true, now);
        for i in 0..(NEGATIVE_CAP * 4) {
            memo.store(tag("u", &format!("wrong{i}")), false, now);
        }
        assert_eq!(memo.lookup(&tag("u", "p"), now), Lookup::Hit);
    }

    #[test]
    fn a_positive_store_replaces_the_previous_slot() {
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "old"), true, now);
        memo.store(tag("u", "new"), true, now);
        assert_eq!(memo.lookup(&tag("u", "old"), now), Lookup::Miss);
        assert_eq!(memo.lookup(&tag("u", "new"), now), Lookup::Hit);
    }

    #[test]
    fn a_negative_for_the_proved_credential_does_not_hide_the_positive() {
        // Cannot happen with a correct KDF, but the lookup order must prefer
        // the positive slot so a stray negative can never deny a proved one.
        let now = Instant::now();
        let mut memo = Memo::new();
        memo.store(tag("u", "p"), false, now);
        memo.store(tag("u", "p"), true, now);
        assert_eq!(memo.lookup(&tag("u", "p"), now), Lookup::Hit);
    }
}
