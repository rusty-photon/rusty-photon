//! The camera facade's sweep lease: three ASCOM Actions that hold guiding
//! off for a whole focus sweep, and the record of the tokens they were
//! named under.
//!
//! Contract: `docs/services/phd2-guider.md` § "The sweep lease". The
//! record lives in [`GuiderOps`](super::GuiderOps), beside the capture
//! slot, so a guiding mutation checks both once it holds the guiding lock.
//! Every time here is on the monotonic clock; wall-clock times appear
//! only in messages.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::debug;

/// `Action` name: take a lease.
pub const ACQUIRE: &str = "rustyphoton:sweep-lease-acquire";
/// `Action` name: restart a held lease's TTL.
pub const RENEW: &str = "rustyphoton:sweep-lease-renew";
/// `Action` name: end a held lease.
pub const RELEASE: &str = "rustyphoton:sweep-lease-release";

/// The lease Actions, in the order `SupportedActions` lists them.
pub const ACTIONS: [&str; 3] = [ACQUIRE, RENEW, RELEASE];

const TTL_MIN_S: u64 = 10;
const TTL_MAX_S: u64 = 300;
const TTL_DEFAULT_S: u64 = 60;

/// How long a token is remembered once its lease has ended.
const TOKEN_MEMORY: Duration = Duration::from_hours(1);

/// The most tokens remembered at once, the held lease's included.
pub const TOKENS_REMEMBERED: usize = 256;

/// One of the three lease Actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseAction {
    Acquire,
    Renew,
    Release,
}

impl LeaseAction {
    /// The lease Action an ASCOM `Action` name names, matched exactly.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            ACQUIRE => Some(Self::Acquire),
            RENEW => Some(Self::Renew),
            RELEASE => Some(Self::Release),
            _ => None,
        }
    }
}

/// A lease token: 32 lowercase hex digits, named by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    fn parse(text: String) -> Result<Self, String> {
        let well_formed =
            text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if well_formed {
            Ok(Self(text))
        } else {
            Err(format!(
                "a lease token is 32 lowercase hex digits, not {text:?}"
            ))
        }
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquireParameters {
    token: String,
    ttl_s: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenParameters {
    token: String,
}

/// An acquire's parameters, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    pub token: Token,
    pub ttl_s: u64,
}

/// Parse an acquire's `Parameters`: `{"token": "<32 hex digits>",
/// "ttl_s": 10..=300}`, `ttl_s` defaulting to 60.
///
/// # Errors
///
/// Returns a message naming what is wrong: not such an object, a token
/// of another form, or a TTL outside the range.
pub fn parse_acquire(parameters: &str) -> Result<AcquireRequest, String> {
    let parsed: AcquireParameters = serde_json::from_str(parameters).map_err(|e| {
        format!(
            "{ACQUIRE} takes {{\"token\": \"<32 hex digits>\", \"ttl_s\": <{TTL_MIN_S} to \
             {TTL_MAX_S}>}}: {e}"
        )
    })?;
    let ttl_s = parsed.ttl_s.unwrap_or(TTL_DEFAULT_S);
    if !(TTL_MIN_S..=TTL_MAX_S).contains(&ttl_s) {
        return Err(format!(
            "ttl_s {ttl_s} is outside {TTL_MIN_S} to {TTL_MAX_S} seconds"
        ));
    }
    Ok(AcquireRequest {
        token: Token::parse(parsed.token)?,
        ttl_s,
    })
}

/// Parse a renewal's or a release's `Parameters`: `{"token": "<32 hex
/// digits>"}`.
///
/// # Errors
///
/// Returns a message naming what is wrong.
pub fn parse_token(action: &str, parameters: &str) -> Result<Token, String> {
    let parsed: TokenParameters = serde_json::from_str(parameters)
        .map_err(|e| format!("{action} takes {{\"token\": \"<32 hex digits>\"}}: {e}"))?;
    Token::parse(parsed.token)
}

/// The answer to an acquire or a renewal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Granted {
    pub token: String,
    pub ttl_s: u64,
}

/// The answer to a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Released {
    pub released: bool,
}

/// Why a lease ended, or why a token was recorded without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseEnd {
    Released,
    Expired,
    /// A `guiding/stop`.
    Stopped,
    Disconnected,
    /// Released before any acquire under it arrived.
    ReleasedFirst,
}

/// Why an acquire was not granted. Each maps to an ASCOM error in
/// `camera.rs`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// The camera is not connected, or PHD2 cannot be reached.
    #[error("{0}")]
    NotConnected(String),
    /// Any other refusal: the lock, a frame, a held lease, the token,
    /// the cap, or PHD2's state.
    #[error("{0}")]
    Refused(String),
}

#[derive(Debug)]
struct Held {
    token: Token,
    ttl_s: u64,
    expires: Instant,
}

#[derive(Debug)]
struct Ended {
    token: Token,
    at: Instant,
    why: LeaseEnd,
}

/// The held lease, if any, and the tokens whose lease ended within the
/// last hour. Each method takes `now`, so the clock is the caller's.
#[derive(Debug, Default)]
pub struct LeaseBook {
    held: Option<Held>,
    ended: Vec<Ended>,
}

impl LeaseBook {
    /// End a lease whose TTL has run out, then forget the tokens that
    /// ended an hour ago or more.
    fn settle(&mut self, now: Instant) {
        if self.held.as_ref().is_some_and(|held| held.expires <= now) {
            if let Some(held) = self.held.take() {
                debug!(token = %held.token, "the sweep lease expired");
                self.ended.push(Ended {
                    token: held.token,
                    at: held.expires,
                    why: LeaseEnd::Expired,
                });
            }
        }
        self.ended
            .retain(|ended| now.saturating_duration_since(ended.at) < TOKEN_MEMORY);
    }

    fn remembered(&self) -> usize {
        self.ended
            .len()
            .saturating_add(usize::from(self.held.is_some()))
    }

    fn ended_record(&self, token: &Token) -> Option<&Ended> {
        self.ended.iter().find(|ended| &ended.token == token)
    }

    /// When the held lease expires unless it is renewed, if one is held.
    pub fn held_until(&mut self, now: Instant) -> Option<Instant> {
        self.settle(now);
        self.held.as_ref().map(|held| held.expires)
    }

    /// Whether a lease could be granted under `token` now.
    ///
    /// # Errors
    ///
    /// Returns the refusal: a lease is held, the token was used before,
    /// or the record is full.
    pub fn check_grant(&mut self, token: &Token, now: Instant) -> Result<(), String> {
        self.settle(now);
        if let Some(held) = &self.held {
            return Err(format!(
                "a sweep lease is already held, until {} unless it is renewed",
                when(held.expires, now)
            ));
        }
        if let Some(ended) = self.ended_record(token) {
            return Err(format!(
                "token {token} was used before ({}); a token names one lease and is never \
                 granted again",
                what_ended(ended, now)
            ));
        }
        if self.remembered() >= TOKENS_REMEMBERED {
            let oldest = self.ended.iter().map(|ended| ended.at).min().unwrap_or(now);
            let forgotten = oldest.checked_add(TOKEN_MEMORY).unwrap_or(oldest);
            return Err(format!(
                "this facade remembers {TOKENS_REMEMBERED} lease tokens from the last hour and \
                 takes no new one until {}, when the oldest is forgotten",
                when(forgotten, now)
            ));
        }
        Ok(())
    }

    /// Grant a lease under `token`, checking again what
    /// [`Self::check_grant`] checks.
    ///
    /// # Errors
    ///
    /// Returns the refusal, as [`Self::check_grant`] does.
    pub fn grant(&mut self, token: Token, ttl_s: u64, now: Instant) -> Result<Granted, String> {
        self.check_grant(&token, now)?;
        let ttl = Duration::from_secs(ttl_s);
        let granted = Granted {
            token: token.to_string(),
            ttl_s,
        };
        self.held = Some(Held {
            token,
            ttl_s,
            expires: now.checked_add(ttl).unwrap_or(now),
        });
        Ok(granted)
    }

    /// Restart the held lease's TTL from `now`.
    ///
    /// # Errors
    ///
    /// Returns why `token` holds no lease.
    pub fn renew(&mut self, token: &Token, now: Instant) -> Result<Granted, String> {
        self.settle(now);
        if let Some(held) = self.held.as_mut().filter(|held| &held.token == token) {
            held.expires = now
                .checked_add(Duration::from_secs(held.ttl_s))
                .unwrap_or(now);
            return Ok(Granted {
                token: token.to_string(),
                ttl_s: held.ttl_s,
            });
        }
        Err(self.ended_record(token).map_or_else(
            || {
                format!(
                    "token {token} was never acquired in this process, or not within the last \
                     hour: a restart forgets every lease"
                )
            },
            |ended| {
                format!(
                    "token {token} holds no sweep lease: {}",
                    what_ended(ended, now)
                )
            },
        ))
    }

    /// End the lease `token` holds. Returns whether it held one. A token
    /// never seen is recorded, so an acquire that arrives after its
    /// release is refused, unless the record is full.
    pub fn release(&mut self, token: &Token, now: Instant) -> bool {
        self.settle(now);
        if self.held.as_ref().is_some_and(|held| &held.token == token) {
            self.end(LeaseEnd::Released, now);
            return true;
        }
        if self.ended_record(token).is_none() && self.remembered() < TOKENS_REMEMBERED {
            self.ended.push(Ended {
                token: token.clone(),
                at: now,
                why: LeaseEnd::ReleasedFirst,
            });
        }
        false
    }

    /// End the held lease, if one is held, for `why`. Returns whether
    /// one was.
    pub fn end(&mut self, why: LeaseEnd, now: Instant) -> bool {
        self.settle(now);
        let Some(held) = self.held.take() else {
            return false;
        };
        debug!(token = %held.token, ?why, "the sweep lease ended");
        self.ended.push(Ended {
            token: held.token,
            at: now,
            why,
        });
        true
    }
}

/// What became of the token `ended` records, for an acquire's or a
/// renewal's refusal.
fn what_ended(ended: &Ended, now: Instant) -> String {
    match ended.why {
        LeaseEnd::Expired => format!(
            "its lease expired at {}, with no renewal within its TTL",
            when(ended.at, now)
        ),
        LeaseEnd::Released => "its lease was released".to_string(),
        LeaseEnd::Stopped => "its lease was ended by a guiding stop".to_string(),
        LeaseEnd::Disconnected => {
            "its lease was ended when the camera was disconnected".to_string()
        }
        LeaseEnd::ReleasedFirst => "it was released before any acquire under it".to_string(),
    }
}

/// `at`, a monotonic instant, as a wall-clock time for a message.
#[must_use]
pub fn when(at: Instant, now: Instant) -> String {
    let wall_now = SystemTime::now();
    let wall = if at >= now {
        wall_now.checked_add(at.saturating_duration_since(now))
    } else {
        wall_now.checked_sub(now.saturating_duration_since(at))
    };
    humantime::format_rfc3339_seconds(wall.unwrap_or(wall_now)).to_string()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn token(n: usize) -> Token {
        Token(format!("{n:032x}"))
    }

    fn book_with_lease(now: Instant) -> LeaseBook {
        let mut book = LeaseBook::default();
        book.grant(token(1), 60, now).unwrap();
        book
    }

    #[test]
    fn the_action_names_are_matched_exactly() {
        assert_eq!(
            LeaseAction::from_name("rustyphoton:sweep-lease-acquire"),
            Some(LeaseAction::Acquire)
        );
        assert_eq!(
            LeaseAction::from_name("rustyphoton:sweep-lease-renew"),
            Some(LeaseAction::Renew)
        );
        assert_eq!(
            LeaseAction::from_name("rustyphoton:sweep-lease-release"),
            Some(LeaseAction::Release)
        );
        assert_eq!(
            LeaseAction::from_name("RustyPhoton:sweep-lease-acquire"),
            None
        );
        assert_eq!(LeaseAction::from_name("config.get"), None);
    }

    #[test]
    fn an_acquire_without_a_ttl_asks_for_60_s() {
        let request = parse_acquire(r#"{"token": "0123456789abcdef0123456789abcdef"}"#).unwrap();
        assert_eq!(request.ttl_s, 60);
        assert_eq!(
            request.token.to_string(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn a_ttl_from_10_to_300_s_is_accepted() {
        let at = |ttl: u64| {
            parse_acquire(&format!(
                r#"{{"token": "0123456789abcdef0123456789abcdef", "ttl_s": {ttl}}}"#
            ))
        };
        assert_eq!(at(10).unwrap().ttl_s, 10);
        assert_eq!(at(300).unwrap().ttl_s, 300);
        assert!(at(9).unwrap_err().contains("outside 10 to 300"));
        assert!(at(301).unwrap_err().contains("outside 10 to 300"));
    }

    #[test]
    fn a_token_is_32_lowercase_hex_digits() {
        let token = |text: &str| parse_token(RENEW, &format!(r#"{{"token": "{text}"}}"#));
        token("0123456789abcdef0123456789abcdef").unwrap();
        assert!(token("0123456789ABCDEF0123456789ABCDEF")
            .unwrap_err()
            .contains("32 lowercase hex digits"));
        token("0123").unwrap_err();
        token("0123456789abcdef0123456789abcdeg").unwrap_err();
    }

    #[test]
    fn parameters_of_another_shape_name_the_shape_expected() {
        let err = parse_acquire("not json").unwrap_err();
        assert!(err.contains(r#"{"token": "<32 hex digits>""#), "{err}");
        let err = parse_token(
            RELEASE,
            r#"{"token": "0123456789abcdef0123456789abcdef", "ttl_s": 60}"#,
        )
        .unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        let err = parse_acquire(r#"{"ttl_s": 60}"#).unwrap_err();
        assert!(err.contains("missing field `token`"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lease_refuses_a_second_and_names_its_expiry() {
        let now = Instant::now();
        let mut book = book_with_lease(now);
        assert_eq!(book.held_until(now), Some(now + Duration::from_secs(60)));
        let err = book.check_grant(&token(2), now).unwrap_err();
        assert!(err.contains("already held"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_lapses_at_its_ttl_and_a_renewal_then_says_it_expired() {
        let start = Instant::now();
        let mut book = book_with_lease(start);
        let later = start + Duration::from_secs(60);
        assert_eq!(book.held_until(later), None);
        let err = book.renew(&token(1), later).unwrap_err();
        assert!(err.contains("expired at"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_restarts_the_ttl_from_now() {
        let start = Instant::now();
        let mut book = book_with_lease(start);
        let renewed_at = start + Duration::from_secs(50);
        let granted = book.renew(&token(1), renewed_at).unwrap();
        assert_eq!(granted.ttl_s, 60);
        assert_eq!(
            book.held_until(start + Duration::from_secs(100)),
            Some(renewed_at + Duration::from_secs(60))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_names_how_the_lease_ended() {
        let now = Instant::now();
        let cases = [
            (LeaseEnd::Stopped, "guiding stop"),
            (LeaseEnd::Disconnected, "disconnected"),
            (LeaseEnd::Released, "was released"),
        ];
        for (why, needle) in cases {
            let mut book = book_with_lease(now);
            assert!(book.end(why, now));
            let err = book.renew(&token(1), now).unwrap_err();
            assert!(err.contains(needle), "{why:?}: {err}");
        }
        let mut book = LeaseBook::default();
        let err = book.renew(&token(9), now).unwrap_err();
        assert!(err.contains("never acquired"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_never_granted_twice() {
        let now = Instant::now();
        let mut book = book_with_lease(now);
        assert!(book.release(&token(1), now));
        assert!(
            !book.release(&token(1), now),
            "a second release found a lease"
        );
        let err = book.grant(token(1), 60, now).unwrap_err();
        assert!(err.contains("used before"), "{err}");
        book.grant(token(2), 60, now).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_that_comes_first_keeps_its_acquire_from_being_granted() {
        let now = Instant::now();
        let mut book = LeaseBook::default();
        assert!(!book.release(&token(1), now));
        let err = book.check_grant(&token(1), now).unwrap_err();
        assert!(err.contains("released before any acquire"), "{err}");
        let err = book.renew(&token(1), now).unwrap_err();
        assert!(err.contains("released before any acquire"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_is_forgotten_an_hour_after_its_lease_ended() {
        let start = Instant::now();
        let mut book = book_with_lease(start);
        book.release(&token(1), start);
        let almost = start + TOKEN_MEMORY - Duration::from_secs(1);
        let err = book.check_grant(&token(1), almost).unwrap_err();
        assert!(err.contains("used before"), "{err}");
        book.check_grant(&token(1), start + TOKEN_MEMORY).unwrap();
    }

    /// A book remembering `TOKENS_REMEMBERED` released tokens, the
    /// oldest released at `start`, one more each second.
    fn full_book(start: Instant) -> LeaseBook {
        let mut book = LeaseBook::default();
        for n in 0..TOKENS_REMEMBERED {
            let at = start + Duration::from_secs(u64::try_from(n).unwrap());
            book.release(&token(1000 + n), at);
        }
        book
    }

    #[tokio::test(start_paused = true)]
    async fn at_the_cap_a_new_token_is_refused_rather_than_a_young_one_forgotten() {
        let start = Instant::now();
        let mut book = full_book(start);
        let now = start + Duration::from_secs(600);
        let err = book.check_grant(&token(1), now).unwrap_err();
        assert!(err.contains("256 lease tokens"), "{err}");
        // Once the oldest has been remembered an hour, there is room again.
        book.grant(token(1), 60, start + TOKEN_MEMORY).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn at_the_cap_a_release_is_answered_but_not_recorded() {
        let start = Instant::now();
        let mut book = full_book(start);
        let now = start + Duration::from_secs(600);
        assert!(!book.release(&token(1), now));
        assert_eq!(book.ended.len(), TOKENS_REMEMBERED);
        assert!(book.ended_record(&token(1)).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_held_lease_counts_toward_the_cap() {
        let start = Instant::now();
        let mut book = LeaseBook::default();
        for n in 0..TOKENS_REMEMBERED - 1 {
            book.release(&token(1000 + n), start);
        }
        book.grant(token(1), 60, start).unwrap();
        // Full now: a release of a token never seen is not recorded...
        assert!(!book.release(&token(2), start));
        assert!(book.ended_record(&token(2)).is_none());
        // ...and the held lease's end moves it into the record, which
        // stays at the cap.
        assert!(book.end(LeaseEnd::Stopped, start));
        assert_eq!(book.ended.len(), TOKENS_REMEMBERED);
    }

    #[test]
    fn ending_with_no_lease_held_does_nothing() {
        let mut book = LeaseBook::default();
        assert!(!book.end(LeaseEnd::Disconnected, Instant::now()));
        assert!(book.ended.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_time_is_named_in_wall_clock_time() {
        let start = Instant::now();
        let now = start + Duration::from_secs(60);
        let later = when(now + Duration::from_secs(60), now);
        let earlier = when(start, now);
        assert!(later > earlier, "{earlier} !< {later}");
    }
}
