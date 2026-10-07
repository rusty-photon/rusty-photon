//! The per-layer verifier: memo lookup, a single-slot gate and the off-worker
//! KDF.
//!
//! The Argon2id verify exists to be expensive — ~41 ms on a Raspberry Pi 5 —
//! and HTTP Basic is stateless, so a middleware that runs it inline pays that
//! on every request and, worse, parks the tokio worker that holds the
//! runtime's I/O and timer driver for the duration. This module separates
//! *proving* a credential from *recognising* one already proved:
//!
//! - a hit in the [`Memo`] answers in microseconds and never touches the gate;
//! - one verification is in flight at a time, tracked by a single mutex-guarded
//!   slot: taking the gate and publishing the credential's tag are the *same*
//!   locked act, so there is no window in which a concurrent request can miss
//!   an in-flight verification;
//! - the KDF runs on the blocking pool, and a drop guard clears the slot on
//!   every exit (return or panic), so a client that disconnects mid-verify can
//!   neither free the gate early nor lose the warm-up, and a panicking KDF
//!   cannot wedge the gate;
//! - a request presenting the credential that is currently being verified
//!   waits for that verdict and is never refused (single-flight by tag); only
//!   a request for a different credential may be answered [`Verdict::Busy`],
//!   after a bounded wait.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, Salt};
use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use tracing::{debug, error, warn};
use zeroize::Zeroizing;

use crate::config::AuthConfig;
use crate::credentials;
use crate::memo::{Lookup, MacKey, Memo, Tag};

/// How long a request waits for the gate before it is judged busy. Sits under
/// sentinel's 2 s probe timeout. Only a request for a *different* credential
/// than the one in flight is refused when it expires: one whose own credential
/// is in flight waits for that verdict with no bound of its own.
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

/// The verification currently in flight, and where its verdict will land.
/// `None` in the channel means "pending"; a sender dropped without a value
/// means the KDF task died. `tag` is `None` only when the memo is disabled,
/// in which case no request can single-flight onto it.
struct InFlight {
    /// A monotonic id so a drop guard clears only its own registration, never
    /// a successor's.
    id: u64,
    tag: Option<Tag>,
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
    /// and concurrent requests for one credential serialise on the gate
    /// (so may be answered [`Verdict::Busy`]). Never a fixed key instead.
    mac_key: Option<MacKey>,
    memo: Mutex<Memo>,
    /// The gate: `Some` means a verification is in flight. Taking this slot is
    /// the single atomic act of claiming the gate and publishing the tag.
    slot: Mutex<Option<InFlight>>,
    /// Bumped whenever the slot clears, so a request waiting for a different
    /// credential to finish is woken without a lost-wakeup window.
    freed: watch::Sender<u64>,
    next_id: AtomicU64,
    kdf: Box<Kdf>,
    gate_wait: Duration,
    refusals: AtomicU64,
    #[cfg(test)]
    kdf_runs: std::sync::atomic::AtomicUsize,
}

/// Clears the gate slot (and wakes waiters) when a held verification ends,
/// whether its blocking task returns or panics. Clears only its own
/// registration, identified by `id`.
struct SlotGuard {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.shared.release_slot(self.id);
    }
}

/// What inspecting the gate slot under its lock decided this request should do
/// once the lock is released. Carrying the decision out of the locked section
/// keeps the `MutexGuard` from ever being held across an await.
enum Claim {
    /// The memo answered under the lock: let the request through.
    Allow,
    /// The memo answered under the lock: refuse with 401.
    Deny,
    /// This request claimed the free gate; run the KDF under `guard`,
    /// publishing the verdict through the sender.
    Hold(SlotGuard, watch::Sender<Option<bool>>),
    /// This request's credential is already in flight; wait for its verdict.
    Wait(watch::Receiver<Option<bool>>),
    /// A different credential holds the gate.
    Busy,
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
                slot: Mutex::new(None),
                freed: watch::channel(0u64).0,
                next_id: AtomicU64::new(0),
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
    /// leaves no lock held and nothing half-registered, and a KDF already
    /// spawned runs to completion, stores its verdict and frees the gate on
    /// its own — its [`SlotGuard`] moves into the blocking task.
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

        // Fast path: a memo hit answers in microseconds, never touching the gate.
        if let Some(tag) = &tag {
            match shared.lookup(tag) {
                Lookup::Hit => return Verdict::Allow,
                Lookup::NegativeHit => return Verdict::Deny,
                Lookup::Miss => {}
            }
        }

        // Subscribe before first inspecting the slot, so a free notification
        // that lands between an inspection and the wait is not lost.
        let mut freed = shared.freed.subscribe();
        // tokio's clock, the one the wait below runs on: the same instant as
        // std's in production, and a test on paused time can then drive the
        // gate wait instead of racing the host's scheduling.
        let started = tokio::time::Instant::now();
        loop {
            match shared.claim_slot(tag.as_ref()) {
                Claim::Allow => return Verdict::Allow,
                Claim::Deny => return Verdict::Deny,
                Claim::Hold(guard, tx) => {
                    return shared.run_held(guard, tx, username, password, tag).await;
                }
                Claim::Wait(rx) => return shared.await_outcome(rx).await,
                Claim::Busy => {
                    let remaining = shared
                        .gate_wait
                        .checked_sub(started.elapsed())
                        .unwrap_or(Duration::ZERO);
                    if remaining.is_zero() {
                        // The wait has elapsed and the fresh `claim_slot` above
                        // still reports a *different* credential in flight: only
                        // now, after re-inspecting the slot, is 503 honest.
                        shared.note_refusal();
                        return Verdict::Busy;
                    }
                    // Wait for the gate to free or the wait to elapse, then loop
                    // to re-inspect the slot. A timeout never refuses on its own:
                    // the next `claim_slot` may join a sibling that took our tag,
                    // read a verdict proved at the deadline, or claim the freed
                    // gate; it refuses only if a different credential is still in
                    // flight past the wait. A closed channel cannot happen while
                    // `self` lives, so its wake is treated like any other.
                    let _ = tokio::time::timeout(remaining, freed.changed()).await;
                }
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

    fn lock_slot(&self) -> std::sync::MutexGuard<'_, Option<InFlight>> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Free the gate at the end of a held verification, clearing the slot only
    /// if it still holds this registration (`id`) and waking every request
    /// blocked on a busy gate so one may claim it. A non-matching id means a
    /// successor already holds the gate and will wake the waiters itself.
    fn release_slot(&self, id: u64) {
        let cleared = {
            let mut slot = self.lock_slot();
            let is_ours = slot.as_ref().is_some_and(|inflight| inflight.id == id);
            if is_ours {
                *slot = None;
            }
            is_ours
        };
        if cleared {
            self.freed
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }

    fn lookup(&self, tag: &Tag) -> Lookup {
        self.lock_memo().lookup(tag, Instant::now())
    }

    /// Inspect the gate slot under its lock and decide this request's next
    /// move, releasing the lock before returning so no guard is held across an
    /// await. Claiming the free gate and publishing the credential's tag are
    /// one locked act, so a concurrent request for the same credential can
    /// never miss the in-flight verification and run a second KDF for it.
    fn claim_slot(self: &Arc<Self>, tag: Option<&Tag>) -> Claim {
        let mut slot = self.lock_slot();
        if let Some(inflight) = slot.as_ref() {
            return if tag_matches(inflight, tag) {
                Claim::Wait(inflight.outcome.clone())
            } else {
                Claim::Busy
            };
        }
        // The gate is free. Re-check the memo under the same lock: a verdict
        // proved while we queued is served now, not paid for a second time.
        // Bind the lookup so the memo guard drops before we build the entry.
        if let Some(tag) = tag {
            let verdict = self.lock_memo().lookup(tag, Instant::now());
            match verdict {
                Lookup::Hit => return Claim::Allow,
                Lookup::NegativeHit => return Claim::Deny,
                Lookup::Miss => {}
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = watch::channel(None);
        *slot = Some(InFlight {
            id,
            tag: tag.cloned(),
            outcome: rx,
        });
        // The tag is now published under the lock; release it before handing
        // back the guard so the gate is observable as held to the next request.
        drop(slot);
        Claim::Hold(
            SlotGuard {
                shared: Arc::clone(self),
                id,
            },
            tx,
        )
    }

    /// Run the KDF on the blocking pool with the gate held by `guard`, publish
    /// the verdict through `tx`, and await it. The guard, the password and the
    /// tag move into the task, so a dropped request future neither frees the
    /// gate early nor loses the warm-up, and a panicking KDF frees the gate
    /// during unwinding rather than wedging it.
    async fn run_held(
        self: &Arc<Self>,
        guard: SlotGuard,
        tx: watch::Sender<Option<bool>>,
        username: &str,
        password: Zeroizing<String>,
        tag: Option<Tag>,
    ) -> Verdict {
        let task_shared = Arc::clone(self);
        let presented_username = digest(username.as_bytes());
        let handle = tokio::task::spawn_blocking(move || {
            // The gate guard, the password and the tag end with this closure,
            // not with the request that spawned it.
            let _guard = guard;
            task_shared.run_kdf(presented_username, &password, tag, &tx)
        });
        match handle.await {
            Ok(true) => Verdict::Allow,
            Ok(false) => Verdict::Deny,
            Err(e) => {
                // The task panicked: its guard freed the gate during unwinding
                // and any same-tag waiter was woken by the dropped sender.
                error!(error = %e, "credential verification task failed; refusing the request");
                Verdict::Deny
            }
        }
    }

    /// Wait for the verdict of the KDF already in flight for this tag. A sender
    /// dropped without a verdict means that task died: fail closed. The slot is
    /// cleared by the dead task's [`SlotGuard`], not here.
    async fn await_outcome(&self, mut rx: watch::Receiver<Option<bool>>) -> Verdict {
        loop {
            // Bind the borrow so its guard drops before the await below.
            let current = *rx.borrow_and_update();
            if let Some(ok) = current {
                return if ok { Verdict::Allow } else { Verdict::Deny };
            }
            if rx.changed().await.is_err() {
                return Verdict::Deny;
            }
        }
    }

    /// The blocking half of a miss. Runs on the blocking pool with the gate
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
        // Publish the verdict through the retained channel value; the gate is
        // freed by the `SlotGuard` when the blocking task ends, just after this
        // returns, so a same-tag waiter reads the verdict before the slot
        // clears and the memo is already warm by the time it does.
        let _ = tx.send(Some(ok));
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

/// Whether an in-flight verification is for the presented credential's tag,
/// compared in constant time. A memo-disabled request presents `None` and an
/// in-flight entry then also carries `None`; two `None`s never match, so
/// single-flight is simply inactive without a memo key.
fn tag_matches(inflight: &InFlight, presented: Option<&Tag>) -> bool {
    match (inflight.tag.as_ref(), presented) {
        (Some(a), Some(b)) => bool::from(a.ct_eq(b)),
        _ => false,
    }
}

/// A fresh per-layer MAC key, or `None` when the OS RNG fails. Never a fixed
/// or time-derived key: a memo keyed predictably would be worse than none.
fn generate_key() -> Option<MacKey> {
    generate_key_from(&mut OsRng)
}

fn generate_key_from(rng: &mut impl RngCore) -> Option<MacKey> {
    let mut key = Zeroizing::new([0u8; 64]);
    match rng.try_fill_bytes(key.as_mut()) {
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
/// RNG fails, fixed bytes are used: the verdict is still forced.
fn decoy_phc() -> String {
    decoy_phc_from(&mut OsRng)
}

fn decoy_phc_from(rng: &mut impl RngCore) -> String {
    let mut salt = [0x42u8; 16];
    let mut hash = [0x24u8; 32];
    if rng.try_fill_bytes(&mut salt).is_err() || rng.try_fill_bytes(&mut hash).is_err() {
        salt = [0x42u8; 16];
        hash = [0x24u8; 32];
    }
    // PHC's B64 is the standard alphabet without padding.
    let salt = B64.encode(salt);
    let hash = B64.encode(hash);
    format!("$argon2id$v=19$m=19456,t=2,p=1${salt}${hash}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::future::Future;
    use std::pin::{pin, Pin};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};

    use tokio::task::JoinHandle;

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
    fn blocking_kdf() -> (Box<Kdf>, mpsc::Sender<()>, Arc<watch::Sender<usize>>) {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let started = Arc::new(watch::Sender::new(0usize));
        let started_in_kdf = Arc::clone(&started);
        let kdf: Box<Kdf> = Box::new(move |password, _phc| {
            started_in_kdf.send_modify(|n| *n = n.saturating_add(1));
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

    /// Wait until the KDF has started `value` times. The KDF wakes this wait
    /// itself: on paused time no timer fires while a KDF holds the blocking
    /// pool, so a polling sleep would never return. The timeout is a backstop
    /// for a KDF that never starts.
    async fn wait_until(started: &watch::Sender<usize>, value: usize) {
        let mut started = started.subscribe();
        tokio::time::timeout(Duration::from_secs(5), started.wait_for(|&n| n >= value))
            .await
            .unwrap_or_else(|_| panic!("the KDF never started {value} times"))
            .unwrap();
    }

    /// Spawn a request for `password` and yield until it is waiting at the
    /// held gate. A request subscribes to the gate's free notifications just
    /// before it first inspects the slot, with no await in between, so once
    /// the subscription count rises it has found the gate held.
    async fn spawn_queued(v: &Arc<Verifier>, password: &'static str) -> JoinHandle<Verdict> {
        let subscribed = v.shared.freed.receiver_count();
        let request = tokio::spawn({
            let v = Arc::clone(v);
            async move { check(&v, USER, password).await }
        });
        for _ in 0..100 {
            if v.shared.freed.receiver_count() > subscribed {
                return request;
            }
            tokio::task::yield_now().await;
        }
        panic!("the request never reached the gate");
    }

    /// Poll `request` once by hand, so the test alone decides when it runs:
    /// its wakeups go nowhere, and it moves only when polled again.
    fn poll_once<F: Future>(request: Pin<&mut F>) -> Poll<F::Output> {
        request.poll(&mut Context::from_waker(Waker::noop()))
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
            *started.borrow(),
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
    async fn a_cancelled_request_keeps_the_gate_and_still_warms_the_slot() {
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
        // The gate must still be held by the detached KDF: a different
        // credential cannot get in.
        assert_eq!(check(&v, USER, "another-guess").await, Verdict::Busy);
        assert_eq!(*started.borrow(), 1);
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
            v.shared.lock_slot().is_none(),
            "the panicking task's guard must have freed the gate"
        );
        // The next presentation runs the KDF again and succeeds.
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
    }

    #[tokio::test]
    async fn a_waiter_on_a_dying_kdf_is_denied_and_clears_the_entry() {
        // A KDF that blocks, then panics when released.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let started = Arc::new(watch::Sender::new(0usize));
        let kdf: Box<Kdf> = Box::new({
            let started = Arc::clone(&started);
            move |_password, _phc| {
                started.send_modify(|n| *n = n.saturating_add(1));
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
        assert!(v.shared.lock_slot().is_none());
        assert!(!v.shared.lock_memo().has_positive());
    }

    // The sibling tests run on paused time: the 200 ms gate wait passes only
    // when the test advances the clock, so a host that stalls the test cannot
    // let it expire while the gate is changing hands.
    #[tokio::test(start_paused = true)]
    async fn a_same_credential_request_that_queued_for_the_gate_is_never_refused() {
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        // X: a different credential holds the gate.
        let x = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, "stale-password").await }
        });
        wait_until(&started, 1).await;
        // A and B: the same correct credential, both cold, both queue for
        // the gate because X's tag is the one in flight.
        let a = spawn_queued(&v, PASSWORD).await;
        let b = spawn_queued(&v, PASSWORD).await;
        // X finishes; one of A/B claims the gate and starts the shared KDF.
        release.send(()).unwrap();
        wait_until(&started, 2).await;
        // Hold the shared KDF well past the loser's 200 ms gate wait:
        // it must have become a same-tag waiter, not a 503. The advance fires
        // any wait that came due, but the test body is polled again before
        // the request that wait woke; the yield lets that request run first.
        tokio::time::advance(Duration::from_millis(400)).await;
        tokio::task::yield_now().await;
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
    async fn claim_slot_holds_the_free_gate_then_turns_it_busy_or_joinable() {
        let v = verifier(instant_kdf());
        let tag = tag_for(&v, PASSWORD);
        // The gate is free: the first claim holds it.
        let Claim::Hold(held, _tx) = v.shared.claim_slot(Some(&tag)) else {
            panic!("the free gate must be claimable as Hold");
        };
        // While it is held, a different credential is turned away as Busy...
        let other = tag_for(&v, "another");
        assert!(matches!(v.shared.claim_slot(Some(&other)), Claim::Busy));
        // ...but the same credential joins the in-flight verification.
        assert!(matches!(v.shared.claim_slot(Some(&tag)), Claim::Wait(_)));
        // Dropping the guard frees the gate; it can be claimed afresh.
        drop(held);
        assert!(matches!(v.shared.claim_slot(Some(&tag)), Claim::Hold(..)));
    }

    #[tokio::test]
    async fn claim_slot_serves_a_memoised_verdict_without_holding_the_gate() {
        let v = verifier(instant_kdf());
        assert_eq!(check(&v, USER, PASSWORD).await, Verdict::Allow);
        // The credential is in the positive memo: claiming re-checks it under
        // the lock and answers Allow instead of holding the gate for a KDF.
        assert!(matches!(
            v.shared.claim_slot(Some(&tag_for(&v, PASSWORD))),
            Claim::Allow
        ));
        assert_eq!(check(&v, USER, "wrong").await, Verdict::Deny);
        assert!(matches!(
            v.shared.claim_slot(Some(&tag_for(&v, "wrong"))),
            Claim::Deny
        ));
    }

    #[tokio::test]
    async fn claim_slot_without_a_memo_always_holds_a_free_gate() {
        // With no key the tag is None; two None tags never match, so a second
        // request for a held gate is Busy, never a single-flight Wait.
        let v = Arc::new(Verifier::build(
            &config(STUB_PHC),
            instant_kdf(),
            None,
            Duration::from_millis(200),
        ));
        let Claim::Hold(held, _tx) = v.shared.claim_slot(None) else {
            panic!("a free gate must be claimable even without a memo");
        };
        assert!(matches!(v.shared.claim_slot(None), Claim::Busy));
        drop(held);
        assert!(matches!(v.shared.claim_slot(None), Claim::Hold(..)));
    }

    #[tokio::test]
    async fn release_slot_clears_only_its_own_registration() {
        // A guard for a stale registration (a KDF task that already died) must
        // not evict the successor that claimed the freed gate with a fresh id.
        let v = verifier(instant_kdf());
        let tag = tag_for(&v, PASSWORD);
        let Claim::Hold(successor, _tx) = v.shared.claim_slot(Some(&tag)) else {
            panic!("expected Hold");
        };
        let successor_id = v.shared.lock_slot().as_ref().unwrap().id;
        // A release for any other id is a no-op: the successor still holds it.
        v.shared.release_slot(successor_id.wrapping_sub(1));
        assert!(
            v.shared.lock_slot().is_some(),
            "a release for a stale id must not evict the live registration"
        );
        // The matching release — what the guard's Drop does — clears it.
        drop(successor);
        assert!(v.shared.lock_slot().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_sibling_that_claims_the_gate_after_the_shared_kdf_finished_hits_the_memo() {
        // Two cold requests for one credential queue behind a foreign KDF.
        // Siblings woken by the same free run back to back, and the second
        // joins the first one's KDF. So the test polls B by hand and holds it
        // back until the shared KDF has finished and freed the gate, an order
        // a busy multi-thread runtime can produce. B then claims the free gate
        // and must find the verdict in the memo under the claim's lock, with
        // no second KDF.
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        let x = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, "stale-password").await }
        });
        wait_until(&started, 1).await;
        let a = spawn_queued(&v, PASSWORD).await;
        let mut b = pin!(check(&v, USER, PASSWORD));
        assert!(poll_once(b.as_mut()).is_pending(), "B must queue behind X");
        release.send(()).unwrap(); // X done: A claims the gate
        wait_until(&started, 2).await;
        release.send(()).unwrap(); // the shared KDF
        assert_eq!(x.await.unwrap(), Verdict::Deny);
        assert_eq!(a.await.unwrap(), Verdict::Allow);
        assert!(
            v.shared.lock_slot().is_none(),
            "the shared KDF must have finished and freed the gate"
        );
        assert_eq!(poll_once(b.as_mut()), Poll::Ready(Verdict::Allow));
        assert_eq!(v.kdf_runs(), 2, "B must be answered from the memo");
        assert_eq!(v.refusals(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sibling_that_claims_the_gate_after_a_refuted_kdf_hits_the_negative_memo() {
        // As above, for a wrong password: B claims the free gate after the
        // shared KDF refuted it and must find the negative verdict in the memo.
        let (kdf, release, started) = blocking_kdf();
        let v = verifier(kdf);
        let x = tokio::spawn({
            let v = Arc::clone(&v);
            async move { check(&v, USER, "stale-password").await }
        });
        wait_until(&started, 1).await;
        let a = spawn_queued(&v, "another-wrong").await;
        let mut b = pin!(check(&v, USER, "another-wrong"));
        assert!(poll_once(b.as_mut()).is_pending(), "B must queue behind X");
        release.send(()).unwrap();
        wait_until(&started, 2).await;
        release.send(()).unwrap();
        assert_eq!(x.await.unwrap(), Verdict::Deny);
        assert_eq!(a.await.unwrap(), Verdict::Deny);
        assert!(
            v.shared.lock_slot().is_none(),
            "the shared KDF must have finished and freed the gate"
        );
        assert_eq!(poll_once(b.as_mut()), Poll::Ready(Verdict::Deny));
        assert_eq!(
            v.kdf_runs(),
            2,
            "the repeated wrong password is answered from the negative memo"
        );
        assert_eq!(v.refusals(), 0);
    }

    #[test]
    fn refusals_are_counted_past_the_first_warning() {
        let v = verifier(instant_kdf());
        for _ in 0..3 {
            v.shared.note_refusal();
        }
        assert_eq!(v.refusals(), 3);
    }

    /// An RNG whose every fill fails, standing in for a broken OS RNG.
    struct FailingRng;

    impl RngCore for FailingRng {
        fn next_u32(&mut self) -> u32 {
            0
        }
        fn next_u64(&mut self) -> u64 {
            0
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            dest.fill(0);
        }
        fn try_fill_bytes(
            &mut self,
            _dest: &mut [u8],
        ) -> Result<(), argon2::password_hash::rand_core::Error> {
            Err(argon2::password_hash::rand_core::Error::new(
                "simulated RNG failure",
            ))
        }
    }

    #[test]
    fn a_failing_rng_yields_no_key() {
        assert!(generate_key_from(&mut FailingRng).is_none());
        assert!(generate_key_from(&mut OsRng).is_some());
    }

    #[test]
    fn a_failing_rng_still_yields_a_parseable_decoy() {
        let decoy = decoy_phc_from(&mut FailingRng);
        PasswordHash::new(&decoy).unwrap();
        assert_eq!(
            decoy,
            decoy_phc_from(&mut FailingRng),
            "the fallback is the fixed bytes"
        );
    }

    #[test]
    fn a_hash_with_an_unknown_argon2_version_is_unusable() {
        let err = stored_hash_usable(
            "$argon2id$v=18$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG",
        )
        .unwrap_err();
        assert!(err.contains("unsupported Argon2 version"), "{err}");
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
