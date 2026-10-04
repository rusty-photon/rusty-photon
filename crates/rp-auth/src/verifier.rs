//! The per-layer verifier: memo lookup, single-flight by tag, the one-permit
//! gate and the off-worker KDF.
//!
//! The Argon2id verify exists to be expensive — ~41 ms on a Raspberry Pi 5 —
//! and HTTP Basic is stateless, so a middleware that runs it inline pays that
//! on every request and, worse, parks the tokio worker that holds the
//! runtime's I/O and timer driver for the duration. This module separates
//! *proving* a credential from *recognising* one already proved:
//!
//! - a hit in the [`Memo`] answers in microseconds and never touches the gate;
//! - a miss runs the KDF on the blocking pool behind a one-permit
//!   [`Semaphore`], with the permit and the memo store owned by the blocking
//!   closure so a client that disconnects mid-verify can neither release the
//!   gate early nor lose the warm-up;
//! - a request presenting the credential that is currently being verified
//!   waits for that verdict and is never refused (single-flight by tag),
//!   whether it found the verification in flight on arrival or only after
//!   queueing for the permit; only a request for a different credential may
//!   be answered [`Verdict::Busy`], after a bounded wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{Output, PasswordHash, Salt, SaltString};
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use subtle::ConstantTimeEq;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, warn};
use zeroize::Zeroizing;

use crate::config::AuthConfig;
use crate::credentials;
use crate::memo::{Lookup, MacKey, Memo, Tag};

/// Concurrent KDF computations per layer instance. One bounds a
/// credential-guessing flood to one core and one 19 MiB buffer per service,
/// leaving the Pi's other cores to the imaging session; the price is that a
/// cold legitimate client queues behind the flood (ADR-003, accepted).
pub const GATE_PERMITS: usize = 1;

/// How long a request waits for the permit before the gate is judged busy.
/// Sits under sentinel's 2 s probe timeout. Only a request for a *different*
/// credential than the one in flight is refused when it expires: one whose
/// own credential is in flight by then switches to waiting for that verdict.
pub const GATE_WAIT: Duration = Duration::from_secs(1);

/// A miss slower than this is logged: the verify takes its parameters from
/// the stored PHC string, so a hand-written hash with heavier parameters, or
/// a throttled board, shows up here rather than as unexplained 503s.
const SLOW_KDF: Duration = Duration::from_millis(500);

/// The outcome of checking one presented credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The credential is proved: let the request through.
    Allow,
    /// The credential is wrong, or cannot be checked: answer 401.
    Deny,
    /// The gate stayed busy with other credentials for the whole wait:
    /// answer 503 with `Retry-After`.
    Busy,
}

/// The KDF comparison: `(password, phc) -> verified`. Production uses
/// [`credentials::verify_password`]; tests inject counting or blocking
/// stand-ins so the gate, the single-flight and the cancellation paths run
/// without argon2.
pub type Kdf = dyn Fn(&str, &str) -> bool + Send + Sync;

/// What a request found when its permit wait ran out.
enum Late {
    /// Its credential was proved, refuted or is in flight: no KDF of its own.
    Verdict(Verdict),
    /// The permit freed at the deadline: proceed as the holder.
    Permit(OwnedSemaphorePermit),
    /// The gate really is busy with another credential.
    Busy,
}

/// The credential currently being proved, and where its verdict will land.
/// `None` in the channel means "pending"; a sender dropped without a value
/// means the KDF task died.
struct InFlight {
    tag: Tag,
    outcome: watch::Receiver<Option<bool>>,
}

struct Shared {
    /// BLAKE2b-256 of the configured username: compared as fixed-length
    /// digests so the comparison is constant-time in the username's length
    /// as well as its bytes.
    username_digest: [u8; 32],
    /// The stored PHC string, or the decoy when the stored one can never
    /// verify (see [`Shared::hash_usable`]).
    phc: String,
    /// Whether `password_hash` is something Argon2 could ever accept a
    /// password against (see [`stored_hash_usable`]). Combined with AND into
    /// every verdict, so an unusable hash can never admit anyone; the decoy
    /// only keeps the timing of a miss indistinguishable from a wrong password.
    hash_usable: bool,
    /// `None` when the OS RNG was unavailable at construction: the memo and
    /// the single-flight are then disabled, every request runs the gated KDF,
    /// and concurrent requests for one credential serialise on the permit
    /// (so may be answered [`Verdict::Busy`]). Never a fixed key instead.
    mac_key: Option<MacKey>,
    memo: Mutex<Memo>,
    gate: Arc<Semaphore>,
    inflight: Mutex<Option<InFlight>>,
    kdf: Box<Kdf>,
    gate_wait: Duration,
    refusals: AtomicU64,
    #[cfg(test)]
    kdf_runs: std::sync::atomic::AtomicUsize,
}

/// One layer instance's verifier. Cheap to share behind an `Arc`.
pub struct Verifier {
    shared: Arc<Shared>,
}

impl Verifier {
    /// Build the production verifier for `config`.
    #[must_use]
    pub fn new(config: &AuthConfig) -> Self {
        Self::build(
            config,
            Box::new(credentials::verify_password),
            generate_key(),
            GATE_WAIT,
        )
    }

    /// Build a verifier with an injected KDF, key and gate wait. `mac_key:
    /// None` runs with the memo disabled.
    pub fn build(
        config: &AuthConfig,
        kdf: Box<Kdf>,
        mac_key: Option<MacKey>,
        gate_wait: Duration,
    ) -> Self {
        let hash_usable = match stored_hash_usable(&config.password_hash) {
            Ok(()) => true,
            Err(reason) => {
                warn!(
                    reason,
                    "server.auth.password_hash cannot be verified by Argon2; \
                     every request will be refused"
                );
                false
            }
        };
        let phc = if hash_usable {
            config.password_hash.clone()
        } else {
            decoy_phc()
        };
        if mac_key.is_none() {
            error!(
                "OS RNG unavailable: the verification memo is disabled, every request runs the KDF"
            );
        }
        Self {
            shared: Arc::new(Shared {
                username_digest: digest(config.username.as_bytes()),
                phc,
                hash_usable,
                mac_key,
                memo: Mutex::new(Memo::new()),
                gate: Arc::new(Semaphore::new(GATE_PERMITS)),
                inflight: Mutex::new(None),
                kdf,
                gate_wait,
                refusals: AtomicU64::new(0),
                #[cfg(test)]
                kdf_runs: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// Check one presented credential.
    ///
    /// Cancellation-safe: dropping the returned future at any await point
    /// leaves no lock held, and a KDF already spawned runs to completion,
    /// stores its verdict and releases the permit on its own.
    pub async fn check(&self, username: &str, password: Zeroizing<String>) -> Verdict {
        let shared = &self.shared;
        let tag = shared.mac_key.as_ref().map(|key| {
            Tag::compute(
                key,
                username.as_bytes(),
                password.as_bytes(),
                shared.phc.as_bytes(),
            )
        });

        if let Some(tag) = &tag {
            match shared.lookup(tag) {
                Lookup::Hit => return Verdict::Allow,
                Lookup::NegativeHit => return Verdict::Deny,
                Lookup::Miss => {}
            }
            if let Some(rx) = shared.same_tag_in_flight(tag) {
                return shared.await_outcome(rx, tag).await;
            }
        }

        let permit =
            match tokio::time::timeout(shared.gate_wait, Arc::clone(&shared.gate).acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_closed)) => return Verdict::Deny,
                Err(_elapsed) => match shared.after_gate_timeout(tag.as_ref()).await {
                    Late::Verdict(verdict) => return verdict,
                    Late::Permit(permit) => permit,
                    Late::Busy => {
                        shared.note_refusal();
                        return Verdict::Busy;
                    }
                },
            };

        // Under the permit no KDF is running, so a credential proved while we
        // queued is in the memo now: re-check before paying for it again.
        if let Some(tag) = &tag {
            match shared.lookup(tag) {
                Lookup::Hit => return Verdict::Allow,
                Lookup::NegativeHit => return Verdict::Deny,
                Lookup::Miss => {}
            }
        }

        let (tx, rx) = watch::channel(None);
        if let Some(tag) = &tag {
            // With one permit the slot is ours; a stale entry left by a KDF
            // task that died is replaced here.
            *shared.lock_inflight() = Some(InFlight {
                tag: tag.clone(),
                outcome: rx,
            });
        }

        let task_shared = Arc::clone(shared);
        let presented_username = digest(username.as_bytes());
        let registered_tag = tag.clone();
        let handle = tokio::task::spawn_blocking(move || {
            // The permit, the password and the store all end with this
            // closure, not with the request that spawned it.
            let _permit = permit;
            task_shared.run_kdf(presented_username, &password, tag, &tx)
        });

        match handle.await {
            Ok(true) => Verdict::Allow,
            Ok(false) => Verdict::Deny,
            Err(e) => {
                error!(error = %e, "credential verification task failed; refusing the request");
                // A panic released the permit during unwinding, so a successor
                // may already have registered its own entry: clear only ours.
                if let Some(tag) = &registered_tag {
                    shared.clear_inflight_if_tag(tag);
                }
                Verdict::Deny
            }
        }
    }

    /// How often the gate refused a request with [`Verdict::Busy`] (test
    /// introspection; production reads the count from the warn log).
    #[cfg(test)]
    pub(crate) fn refusals(&self) -> u64 {
        self.shared.refusals.load(Ordering::Relaxed)
    }

    /// How many KDF computations ran (test introspection).
    #[cfg(test)]
    pub(crate) fn kdf_runs(&self) -> usize {
        self.shared.kdf_runs.load(Ordering::Relaxed)
    }
}

impl Shared {
    fn lock_memo(&self) -> std::sync::MutexGuard<'_, Memo> {
        self.memo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, Option<InFlight>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lookup(&self, tag: &Tag) -> Lookup {
        self.lock_memo().lookup(tag, Instant::now())
    }

    fn same_tag_in_flight(&self, tag: &Tag) -> Option<watch::Receiver<Option<bool>>> {
        let guard = self.lock_inflight();
        guard
            .as_ref()
            .filter(|entry| bool::from(entry.tag.ct_eq(tag)))
            .map(|entry| entry.outcome.clone())
    }

    fn clear_inflight(&self) {
        *self.lock_inflight() = None;
    }

    /// The permit wait ran out. That does not yet mean the gate is busy with
    /// a *different* credential: a sibling that queued for the permit before
    /// the winner registered its tag is now a same-tag waiter, the verdict
    /// may already be in the memo, or the permit may have been released at
    /// the deadline. Look again, yielding briefly to cover the winner's
    /// acquire-to-register window, before refusing.
    async fn after_gate_timeout(&self, tag: Option<&Tag>) -> Late {
        for _ in 0..3 {
            if let Some(tag) = tag {
                match self.lookup(tag) {
                    Lookup::Hit => return Late::Verdict(Verdict::Allow),
                    Lookup::NegativeHit => return Late::Verdict(Verdict::Deny),
                    Lookup::Miss => {}
                }
                if let Some(rx) = self.same_tag_in_flight(tag) {
                    return Late::Verdict(self.await_outcome(rx, tag).await);
                }
            }
            if let Ok(permit) = Arc::clone(&self.gate).try_acquire_owned() {
                return Late::Permit(permit);
            }
            tokio::task::yield_now().await;
        }
        Late::Busy
    }

    /// Clear the in-flight entry only if it is still the one for `tag`.
    fn clear_inflight_if_tag(&self, tag: &Tag) {
        let mut guard = self.lock_inflight();
        let is_ours = guard
            .as_ref()
            .is_some_and(|entry| bool::from(entry.tag.ct_eq(tag)));
        if is_ours {
            *guard = None;
        }
    }

    /// Wait for the verdict of the KDF already running for this tag. A
    /// sender dropped without a verdict means that task died: fail closed and
    /// clear the stale entry so the next presentation runs its own KDF.
    async fn await_outcome(&self, mut rx: watch::Receiver<Option<bool>>, tag: &Tag) -> Verdict {
        loop {
            let current = *rx.borrow_and_update();
            if let Some(ok) = current {
                return if ok { Verdict::Allow } else { Verdict::Deny };
            }
            if rx.changed().await.is_err() {
                self.clear_inflight_if_tag(tag);
                return Verdict::Deny;
            }
        }
    }

    /// The blocking half of a miss. Runs on the blocking pool with the permit
    /// held by the caller's closure. The verdict is the AND of the username
    /// comparison, the KDF result and the stored hash's validity, computed
    /// after the KDF so every miss costs the same whatever was wrong.
    fn run_kdf(
        &self,
        presented_username: [u8; 32],
        password: &str,
        tag: Option<Tag>,
        tx: &watch::Sender<Option<bool>>,
    ) -> bool {
        let started = Instant::now();
        let username_ok = presented_username.ct_eq(&self.username_digest);
        let kdf_ok = (self.kdf)(password, &self.phc);
        #[cfg(test)]
        self.kdf_runs.fetch_add(1, Ordering::Relaxed);
        let ok = bool::from(username_ok) & kdf_ok & self.hash_usable;
        let elapsed = started.elapsed();
        if elapsed > SLOW_KDF {
            warn!(
                ?elapsed,
                gate_wait = ?self.gate_wait,
                "credential verification is slow on this host; a request for another \
                 credential waits at most the gate wait behind it before a 503"
            );
        } else {
            debug!(?elapsed, verified = ok, "credential verified by KDF");
        }
        if let Some(tag) = tag {
            self.lock_memo().store(tag, ok, Instant::now());
        }
        // Publish before clearing: a same-tag waiter that subscribed while
        // the entry was present reads the value the channel retains.
        let _ = tx.send(Some(ok));
        self.clear_inflight();
        ok
    }

    fn note_refusal(&self) {
        let refusals = self
            .refusals
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if refusals.is_power_of_two() {
            warn!(
                refusals,
                gate_wait = ?self.gate_wait,
                "credential verification gate busy for the whole gate wait; answered 503 \
                 (another credential is being verified; a repeated pattern means a \
                 flood of distinct wrong passwords)"
            );
        } else {
            debug!(refusals, "credential verification gate busy; answered 503");
        }
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Blake2b::<U32>::digest(bytes).into()
}

/// A fresh per-layer MAC key, or `None` when the OS RNG fails. Never a fixed
/// or time-derived key: a memo keyed predictably would be worse than none.
fn generate_key() -> Option<MacKey> {
    let mut key = Zeroizing::new([0u8; 64]);
    match OsRng.try_fill_bytes(key.as_mut()) {
        Ok(()) => Some(key),
        Err(e) => {
            error!(error = %e, "OS RNG failed while keying the verification memo");
            None
        }
    }
}

/// Whether [`credentials::verify_password`] could ever accept a password
/// against `stored`: PHC syntax, an Argon2 algorithm, parameters argon2
/// accepts, a salt of a usable length and a hash field. These are the checks
/// argon2 runs before any work, so a stored hash that fails one would answer
/// 401 in nanoseconds; treating it like a malformed hash (decoy, forced deny)
/// keeps the timing honest and surfaces the misconfiguration once at startup.
fn stored_hash_usable(stored: &str) -> Result<(), String> {
    let parsed = PasswordHash::new(stored).map_err(|e| format!("not a PHC string: {e}"))?;
    argon2::Algorithm::try_from(parsed.algorithm)
        .map_err(|e| format!("not an Argon2 hash: {e}"))?;
    if let Some(version) = parsed.version {
        argon2::Version::try_from(version)
            .map_err(|e| format!("unsupported Argon2 version: {e}"))?;
    }
    argon2::Params::try_from(&parsed).map_err(|e| format!("Argon2 parameters rejected: {e}"))?;
    let salt = parsed.salt.ok_or_else(|| "no salt field".to_string())?;
    let mut salt_bytes = [0u8; Salt::MAX_LENGTH];
    let salt_len = salt
        .decode_b64(&mut salt_bytes)
        .map_err(|e| format!("salt is not B64: {e}"))?
        .len();
    if salt_len < argon2::MIN_SALT_LEN {
        return Err(format!(
            "salt is {salt_len} bytes, Argon2 needs at least {}",
            argon2::MIN_SALT_LEN
        ));
    }
    if parsed.hash.is_none() {
        return Err("no hash field".to_string());
    }
    Ok(())
}

/// A syntactically valid Argon2id PHC string whose hash field is random
/// bytes with no known preimage, used when the configured hash can never
/// verify so a miss still costs a full KDF. The verdict is forced to deny by
/// [`Shared::hash_usable`] regardless; this only shapes the timing. If the
/// RNG fails the fixed bytes below are used: the verdict is still forced.
fn decoy_phc() -> String {
    let mut salt = [0x42u8; 16];
    let mut hash = [0x24u8; 32];
    if OsRng.try_fill_bytes(&mut salt).is_err() || OsRng.try_fill_bytes(&mut hash).is_err() {
        salt = [0x42u8; 16];
        hash = [0x24u8; 32];
    }
    let salt = SaltString::encode_b64(&salt).map_or_else(|_| String::new(), |s| s.to_string());
    let hash = Output::new(&hash).map_or_else(|_| String::new(), |h| h.to_string());
    format!("$argon2id$v=19$m=19456,t=2,p=1${salt}${hash}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc;

    use super::*;

    const USER: &str = "observatory";
    const PASSWORD: &str = "correct horse battery staple";
    const STUB_PHC: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG";

    fn config(password_hash: &str) -> AuthConfig {
        AuthConfig {
            username: USER.to_string(),
            password_hash: password_hash.to_string(),
        }
    }

    fn test_key() -> MacKey {
        Zeroizing::new([9u8; 64])
    }

    /// A KDF that accepts exactly `PASSWORD`, instantly.
    fn instant_kdf() -> Box<Kdf> {
        Box::new(|password, _phc| password == PASSWORD)
    }

    /// A KDF that blocks until the test sends on the returned channel, and
    /// counts how many times it has started.
    fn blocking_kdf() -> (Box<Kdf>, mpsc::Sender<()>, Arc<AtomicUsize>) {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let started = Arc::new(AtomicUsize::new(0));
        let started_in_kdf = Arc::clone(&started);
        let kdf: Box<Kdf> = Box::new(move |password, _phc| {
            started_in_kdf.fetch_add(1, Ordering::SeqCst);
            let _ = release_rx.lock().unwrap().recv();
            password == PASSWORD
        });
        (kdf, release_tx, started)
    }

    fn verifier(kdf: Box<Kdf>) -> Arc<Verifier> {
        Arc::new(Verifier::build(
            &config(STUB_PHC),
            kdf,
            Some(test_key()),
            Duration::from_millis(200),
        ))
    }

    async fn check(v: &Verifier, username: &str, password: &str) -> Verdict {
        v.check(username, Zeroizing::new(password.to_string()))
            .await
    }

    async fn wait_until(counter: &AtomicUsize, value: usize) {
        for _ in 0..500 {
            if counter.load(Ordering::SeqCst) >= value {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("counter never reached {value}");
    }

    #[test]
    fn decoy_phc_parses_as_argon2id() {
        let decoy = decoy_phc();
        let parsed = PasswordHash::new(&decoy).unwrap();
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        assert!(parsed.hash.is_some() && parsed.salt.is_some(), "{decoy}");
    }

    #[test]
    fn decoy_phc_is_fresh_each_time() {
        assert_ne!(decoy_phc(), decoy_phc());
    }

    #[test]
    fn stub_phc_fixture_parses() {
        PasswordHash::new(STUB_PHC).unwrap();
    }

    #[tokio::test]
    async fn first_presentation_runs_the_kdf_and_allows() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(v.kdf_runs(), 1);
    }

    #[tokio::test]
    async fn second_presentation_is_a_hit_with_no_kdf() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(v.kdf_runs(), 1);
    }

    #[tokio::test]
    async fn wrong_password_is_denied_and_the_repeat_is_a_negative_hit() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, USER, "wrong").await, Verdict::Deny);
        assert_eq!(check(&v, USER, "wrong").await, Verdict::Deny);
        assert_eq!(
            v.kdf_runs(),
            1,
            "the repeated wrong password must not re-run the KDF"
        );
    }

    #[tokio::test]
    async fn wrong_username_costs_a_kdf_and_is_denied() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, "intruder", PASSWORD).await, Verdict::Deny);
        assert_eq!(
            v.kdf_runs(),
            1,
            "a wrong username must not short-circuit the KDF"
        );
    }

    #[tokio::test]
    async fn a_wrong_username_never_warms_the_positive_slot() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, "intruder", PASSWORD).await, Verdict::Deny);
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(v.kdf_runs(), 2);
    }

    #[tokio::test]
    async fn malformed_stored_hash_denies_after_a_full_kdf_and_caches_only_a_negative() {
        let kdf_accepts_everything: Box<Kdf> = Box::new(|_, _| true);
        let v = Arc::new(Verifier::build(
            &config("not-a-phc-string"),
            kdf_accepts_everything,
            Some(test_key()),
            Duration::from_millis(200),
        ));
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Deny);
        assert_eq!(
            v.kdf_runs(),
            1,
            "the decoy must be verified so the miss costs a KDF"
        );
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Deny);
        assert_eq!(v.kdf_runs(), 1, "the refusal is memoised as a negative");
        assert!(!v.shared.lock_memo().has_positive());
    }

    #[tokio::test]
    async fn without_a_key_every_presentation_runs_the_kdf() {
        let v = Arc::new(Verifier::build(
            &config(STUB_PHC),
            instant_kdf(),
            None,
            Duration::from_millis(200),
        ));
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(v.kdf_runs(), 2);
    }

    #[tokio::test]
    async fn concurrent_cold_requests_for_one_credential_share_one_kdf() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        let first = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        wait_until(&started, 1).await;
        let second = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        // Give the second request time to reach the single-flight wait.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "the second request must not start a KDF"
        );
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap(), Verdict::Allow);
        assert_eq!(second.await.unwrap(), Verdict::Allow);
        assert_eq!(v.kdf_runs(), 1);
    }

    #[tokio::test]
    async fn a_same_credential_waiter_is_never_refused_however_long_the_kdf_takes() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf); // gate wait is 200 ms; we hold the KDF for 600 ms
        let first = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        wait_until(&started, 1).await;
        let second = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !second.is_finished(),
            "the same-tag waiter must still be waiting, not refused"
        );
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap(), Verdict::Allow);
        assert_eq!(second.await.unwrap(), Verdict::Allow);
        assert_eq!(v.refusals(), 0);
    }

    #[tokio::test]
    async fn a_different_credential_behind_a_busy_gate_is_refused_after_the_wait() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        let first = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        wait_until(&started, 1).await;
        let t = Instant::now();
        assert_eq!(check(&v, USER, "another-guess").await, Verdict::Busy);
        assert!(
            t.elapsed() >= Duration::from_millis(150),
            "Busy must wait out the gate bound first"
        );
        assert_eq!(v.refusals(), 1);
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap(), Verdict::Allow);
    }

    #[tokio::test]
    async fn a_cancelled_request_keeps_the_permit_and_still_warms_the_slot() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        let first = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        wait_until(&started, 1).await;
        // The client goes away mid-verify.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        // The permit must still be held by the detached KDF: a different
        // credential cannot get in.
        assert_eq!(check(&v, USER, "another-guess").await, Verdict::Busy);
        assert_eq!(started.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        // The cancelled request's KDF completed and warmed the slot.
        for _ in 0..500 {
            if v.shared.lock_memo().has_positive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(
            v.kdf_runs(),
            1,
            "the warm slot must serve the next presentation"
        );
    }

    #[tokio::test]
    async fn a_panicking_kdf_denies_and_caches_nothing() {
        let first_call = Arc::new(AtomicBool::new(true));
        let kdf: Box<Kdf> = Box::new({
            let first_call = Arc::clone(&first_call);
            move |password, _phc| {
                assert!(
                    !first_call.swap(false, Ordering::SeqCst),
                    "simulated KDF panic"
                );
                password == PASSWORD
            }
        });
        let v = verifier(kdf);
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Deny);
        assert!(!v.shared.lock_memo().has_positive());
        assert!(
            v.shared.lock_inflight().is_none(),
            "the stale in-flight entry must be cleared"
        );
        // The next presentation runs the KDF again and succeeds.
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
    }

    #[tokio::test]
    async fn a_waiter_on_a_dying_kdf_is_denied_and_clears_the_entry() {
        // A KDF that blocks, then panics when released.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let started = Arc::new(AtomicUsize::new(0));
        let kdf: Box<Kdf> = Box::new({
            let started = Arc::clone(&started);
            move |_password, _phc| {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = release_rx.lock().unwrap().recv();
                panic!("simulated KDF panic after release");
            }
        });
        let v = verifier(kdf);
        let first = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        wait_until(&started, 1).await;
        let waiter = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap(), Verdict::Deny);
        assert_eq!(waiter.await.unwrap(), Verdict::Deny);
        assert!(v.shared.lock_inflight().is_none());
        assert!(!v.shared.lock_memo().has_positive());
    }

    #[tokio::test]
    async fn a_same_credential_request_that_queued_for_the_permit_is_never_refused() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf); // gate wait 200 ms
                               // X: a different credential holds the permit.
        let x = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, "stale-password").await }
        });
        wait_until(&started, 1).await;
        // A and B: the same correct credential, both cold, both queue for
        // the permit because X's tag is the one in flight.
        let a = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        let b = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, PASSWORD).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // X finishes; one of A/B wins the permit and starts the shared KDF.
        release.send(()).unwrap();
        wait_until(&started, 2).await;
        // Hold the shared KDF well past the loser's 200 ms permit deadline:
        // it must have become a same-tag waiter, not a 503.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !a.is_finished() && !b.is_finished(),
            "both must wait for the shared verdict"
        );
        assert_eq!(v.refusals(), 0, "a queued sibling must never be refused");
        release.send(()).unwrap();
        assert_eq!(x.await.unwrap(), Verdict::Deny);
        assert_eq!(a.await.unwrap(), Verdict::Allow);
        assert_eq!(b.await.unwrap(), Verdict::Allow);
        assert_eq!(v.kdf_runs(), 2, "X's KDF plus one shared KDF for A and B");
        assert_eq!(v.refusals(), 0);
    }

    fn tag_for(v: &Verifier, password: &str) -> Tag {
        let key = v.shared.mac_key.as_ref().unwrap();
        Tag::compute(
            key,
            USER.as_bytes(),
            password.as_bytes(),
            v.shared.phc.as_bytes(),
        )
    }

    #[tokio::test]
    async fn after_the_gate_timeout_a_freed_permit_is_taken() {
        // Nothing holds the permit by the time the wait expires: the request
        // proceeds as the new holder instead of being refused.
        let v = verifier(instant_kdf());
        let tag = tag_for(&v, PASSWORD);
        assert!(matches!(
            v.shared.after_gate_timeout(Some(&tag)).await,
            Late::Permit(_)
        ));
    }

    #[tokio::test]
    async fn after_the_gate_timeout_a_proved_credential_is_allowed() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        let tag = tag_for(&v, PASSWORD);
        let _held = Arc::clone(&v.shared.gate).acquire_owned().await.unwrap();
        assert!(matches!(
            v.shared.after_gate_timeout(Some(&tag)).await,
            Late::Verdict(Verdict::Allow)
        ));
    }

    #[tokio::test]
    async fn after_the_gate_timeout_a_held_permit_for_another_credential_is_busy() {
        let v = verifier(instant_kdf());
        let tag = tag_for(&v, PASSWORD);
        let _held = Arc::clone(&v.shared.gate).acquire_owned().await.unwrap();
        assert!(matches!(
            v.shared.after_gate_timeout(Some(&tag)).await,
            Late::Busy
        ));
    }

    #[tokio::test]
    async fn after_the_gate_timeout_without_a_memo_only_the_permit_counts() {
        let v = Arc::new(Verifier::build(
            &config(STUB_PHC),
            instant_kdf(),
            None,
            Duration::from_millis(200),
        ));
        assert!(matches!(
            v.shared.after_gate_timeout(None).await,
            Late::Permit(_)
        ));
        let _held = Arc::clone(&v.shared.gate).acquire_owned().await.unwrap();
        assert!(matches!(
            v.shared.after_gate_timeout(None).await,
            Late::Busy
        ));
    }

    #[test]
    fn a_valid_argon2id_hash_is_usable() {
        stored_hash_usable(STUB_PHC).unwrap();
    }

    #[test]
    fn a_hash_for_another_algorithm_is_unusable() {
        let err = stored_hash_usable(
            "$pbkdf2-sha256$i=1000$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG",
        )
        .unwrap_err();
        assert!(err.contains("not an Argon2 hash"), "{err}");
    }

    #[test]
    fn a_hash_with_rejected_parameters_is_unusable() {
        let err = stored_hash_usable(
            "$argon2id$v=19$m=1,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG",
        )
        .unwrap_err();
        assert!(err.contains("parameters rejected"), "{err}");
    }

    #[test]
    fn a_hash_without_a_hash_field_is_unusable() {
        let err = stored_hash_usable("$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ").unwrap_err();
        assert!(err.contains("no hash field"), "{err}");
    }

    #[test]
    fn a_hash_with_a_short_salt_is_unusable() {
        let err = stored_hash_usable(
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$RdescudvJCsgt3ub+b+dWRWJTmaaJObG",
        )
        .unwrap_err();
        assert!(err.contains("salt is 4 bytes"), "{err}");
    }

    #[test]
    fn garbage_is_unusable() {
        let err = stored_hash_usable("not-a-phc-string").unwrap_err();
        assert!(err.contains("not a PHC string"), "{err}");
    }

    #[tokio::test]
    async fn an_unusable_but_well_formed_hash_takes_the_decoy_path() {
        let kdf_accepts_everything: Box<Kdf> = Box::new(|_, _| true);
        let v = Arc::new(Verifier::build(
            &config("$pbkdf2-sha256$i=1000$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG"),
            kdf_accepts_everything,
            Some(test_key()),
            Duration::from_millis(200),
        ));
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Deny);
        assert_eq!(
            v.kdf_runs(),
            1,
            "the miss must still cost a KDF against the decoy"
        );
        assert!(!v.shared.lock_memo().has_positive());
        assert!(
            v.shared.phc.starts_with("$argon2id$"),
            "the decoy replaces the stored string"
        );
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing is too slow under Miri
    async fn the_production_kdf_verifies_a_real_argon2id_hash() {
        let hash = credentials::hash_password(PASSWORD).unwrap();
        let v = Verifier::new(&config(&hash));
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(check(&v, USER, "wrong").await, Verdict::Deny);
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        assert_eq!(v.kdf_runs(), 2);
    }
}
