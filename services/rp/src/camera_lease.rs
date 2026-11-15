//! The guider's camera lease, as rp holds it (rp.md § Train Lease,
//! step 2, and § Capture; guider.md § The camera lease).
//!
//! The guider exposes on the guiding train's camera whenever it is not
//! idle, and refuses to start while a camera lease is held. A train
//! lease on the guiding train holds it, and so does a capture through
//! that camera outside one. rp names the token, so a lease whose
//! acquire got no answer can still be released.

use std::sync::Arc;
use std::time::Duration;

use rp_guider::{GuiderClient, GuiderError, GuiderState};
use tokio::task::JoinHandle;
use tracing::debug;

/// The `ttl` rp asks for. Renewing every [`RENEW_INTERVAL`] leaves one
/// renewal free to fail: the next still lands about 20 s before the
/// lease would lapse.
pub const TTL: Duration = Duration::from_secs(60);

/// How often rp renews a camera lease it holds.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(20);

/// How long a renewal or the closing release may take.
///
/// A renewal that has not answered by then counts as one that did not
/// reach the guider, and is tried again at the next tick.
pub const CALL_BOUND: Duration = Duration::from_secs(10);

/// A fresh token: 32 lowercase hex digits.
#[must_use]
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Ask the guider for its camera lease under `token`.
///
/// # Errors
///
/// The guider's refusal, naming the guider's state when it is not
/// idle, or the request's failure, as text.
pub async fn acquire(guider: &dyn GuiderClient, token: &str) -> Result<(), String> {
    match guider.acquire_camera_lease(token, TTL).await {
        Ok(()) => {
            debug!("the guider granted its camera lease");
            Ok(())
        }
        Err(e @ GuiderError::Service { .. }) => {
            let state = if e.code() == Some("not_idle") {
                state_note(guider).await
            } else {
                String::new()
            };
            Err(format!("the guider refused the camera lease: {e}{state}"))
        }
        Err(e) => Err(format!("the guider's camera lease could not be taken: {e}")),
    }
}

/// The state of a guider that refused the lease for not being idle, as
/// a note for the refusal. Empty when the read fails or shows `idle`:
/// the guider's own words then stand alone.
async fn state_note(guider: &dyn GuiderClient) -> String {
    match tokio::time::timeout(CALL_BOUND, guider.guiding_stats()).await {
        Ok(Ok(stats)) if stats.state != GuiderState::Idle => {
            format!(" (the guider's state: {})", stats.state)
        }
        Ok(Ok(_)) => String::new(),
        Ok(Err(e)) => {
            debug!(error = %e, "the guider's state could not be read for the refusal");
            String::new()
        }
        Err(_) => {
            debug!(bound = ?CALL_BOUND, "the guider did not answer the state read for the refusal");
            String::new()
        }
    }
}

/// What one renewal came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Renewal {
    Renewed,
    /// The guider knows no lease under the token: it restarted, or the
    /// lease lapsed. Carries the guider's message.
    Lost(String),
    /// It did not reach the guider, or got no answer in time; the next
    /// tick tries again and the lease's `ttl` decides.
    Unreached(String),
}

/// Set the camera lease's `ttl` anew.
pub async fn renew(guider: &dyn GuiderClient, token: &str) -> Renewal {
    match tokio::time::timeout(CALL_BOUND, guider.renew_camera_lease(token, TTL)).await {
        Ok(Ok(())) => Renewal::Renewed,
        Ok(Err(GuiderError::Service { code, message, .. })) if code == "lease_not_found" => {
            Renewal::Lost(message)
        }
        Ok(Err(e)) => Renewal::Unreached(e.to_string()),
        Err(_) => Renewal::Unreached(format!("no answer within {CALL_BOUND:?}")),
    }
}

/// End the camera lease. Best effort: the guider never refuses a
/// release, and one that does not reach it leaves the lease to lapse at
/// its `ttl`.
pub async fn release(guider: &dyn GuiderClient, token: &str) {
    match tokio::time::timeout(CALL_BOUND, guider.release_camera_lease(token)).await {
        Ok(Ok(released)) => debug!(released, "released the guider's camera lease"),
        Ok(Err(e)) => {
            debug!(error = %e, "the guider's camera lease could not be released; it lapses at its ttl");
        }
        Err(_) => {
            debug!(bound = ?CALL_BOUND, "the guider did not answer the release; its camera lease lapses at its ttl");
        }
    }
}

/// The camera lease of a capture that runs under no train lease
/// (rp.md § Capture): asked for under a fresh token, renewed every
/// [`RENEW_INTERVAL`] once granted, and released by its holder.
///
/// The token exists before the request, so a request that failed or
/// whose answer never came is released like a granted one.
pub struct Held {
    guider: Arc<dyn GuiderClient>,
    token: String,
    renewals: Option<JoinHandle<()>>,
}

impl Held {
    /// A holder that has asked the guider nothing yet.
    #[must_use]
    pub fn new(guider: Arc<dyn GuiderClient>) -> Self {
        Self {
            guider,
            token: new_token(),
            renewals: None,
        }
    }

    /// Ask for the lease, and keep it renewed once it is granted.
    ///
    /// # Errors
    ///
    /// As [`acquire`]. The holder is still to be released.
    pub async fn acquire(&mut self) -> Result<(), String> {
        acquire(self.guider.as_ref(), &self.token).await?;
        self.renewals = Some(tokio::spawn(keep_renewed(
            Arc::clone(&self.guider),
            self.token.clone(),
        )));
        Ok(())
    }

    /// Stop renewing and end the lease.
    pub async fn release(mut self) {
        self.stop_renewing();
        release(self.guider.as_ref(), &self.token).await;
    }

    fn stop_renewing(&mut self) {
        if let Some(renewals) = self.renewals.take() {
            renewals.abort();
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.stop_renewing();
    }
}

/// Renew a capture's camera lease until the guider no longer knows it.
///
/// A lease the guider lost does not end the capture: a guider that
/// restarted is idle, and exposes only when asked.
async fn keep_renewed(guider: Arc<dyn GuiderClient>, token: String) {
    loop {
        tokio::time::sleep(RENEW_INTERVAL).await;
        match renew(guider.as_ref(), &token).await {
            Renewal::Renewed => debug!("renewed the capture's camera lease"),
            Renewal::Lost(reason) => {
                debug!(%reason, "the guider lost the capture's camera lease; the capture goes on");
                return;
            }
            Renewal::Unreached(reason) => {
                debug!(%reason, "the capture's camera lease renewal did not reach the guider");
            }
        }
    }
}

/// A fake guider for the tests of the camera lease's holders: it
/// answers the three lease calls as its fields say and records each.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_support {
    use std::sync::Mutex;
    use std::time::Duration;

    use rp_guider::{
        DitherRequest, GuiderClient, GuiderDevices, GuiderError, GuiderState, GuidingMetrics,
        GuidingStats, SettledOutcome, StartGuidingRequest,
    };

    /// How the fake answers a lease call.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub enum Answer {
        #[default]
        Grant,
        /// The guider's error envelope, as `(code, message)`.
        Refuse(&'static str, &'static str),
        Unreachable,
        /// No answer, ever.
        Hang,
    }

    /// One recorded lease call.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Acquire { token: String, ttl: Duration },
        Renew { token: String, ttl: Duration },
        Release { token: String },
    }

    #[derive(Debug, Default)]
    pub struct FakeGuider {
        pub acquire: Mutex<Answer>,
        pub renewal: Mutex<Answer>,
        pub release: Mutex<Answer>,
        /// The state `guiding_stats` reports; the read fails with none.
        pub state: Mutex<Option<GuiderState>>,
        /// What `devices` reports; the read fails with none.
        pub devices: Mutex<Option<GuiderDevices>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeGuider {
        /// Every lease call, in arrival order.
        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// How many renewals have arrived.
        pub fn renewals(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| matches!(call, Call::Renew { .. }))
                .count()
        }

        /// The token of every release, in arrival order.
        pub fn released(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Release { token } => Some(token),
                    Call::Acquire { .. } | Call::Renew { .. } => None,
                })
                .collect()
        }

        async fn answer(&self, call: Call, answer: &Mutex<Answer>) -> Result<(), GuiderError> {
            self.calls.lock().unwrap().push(call);
            let answer = answer.lock().unwrap().clone();
            match answer {
                Answer::Grant => Ok(()),
                Answer::Refuse(code, message) => Err(GuiderError::Service {
                    code: code.to_string(),
                    message: message.to_string(),
                    details: serde_json::Value::Null,
                }),
                Answer::Unreachable => Err(GuiderError::ServiceUnreachable(
                    "connection refused".to_string(),
                )),
                Answer::Hang => std::future::pending().await,
            }
        }
    }

    fn unscripted<T>(what: &str) -> Result<T, GuiderError> {
        Err(GuiderError::Internal(format!(
            "the fake guider answers no {what}"
        )))
    }

    #[async_trait::async_trait]
    impl GuiderClient for FakeGuider {
        async fn start_guiding(
            &self,
            _: StartGuidingRequest,
        ) -> Result<SettledOutcome, GuiderError> {
            unscripted("start")
        }
        async fn stop_guiding(&self) -> Result<GuiderState, GuiderError> {
            unscripted("stop")
        }
        async fn pause_guiding(&self, _: bool) -> Result<GuiderState, GuiderError> {
            unscripted("pause")
        }
        async fn resume_guiding(&self) -> Result<GuiderState, GuiderError> {
            unscripted("resume")
        }
        async fn dither(&self, _: DitherRequest) -> Result<SettledOutcome, GuiderError> {
            unscripted("dither")
        }
        async fn abort_dither(&self) -> Result<bool, GuiderError> {
            unscripted("dither abort")
        }
        async fn guiding_stats(&self) -> Result<GuidingStats, GuiderError> {
            let state = *self.state.lock().unwrap();
            state.map_or_else(
                || {
                    Err(GuiderError::ServiceUnreachable(
                        "connection refused".to_string(),
                    ))
                },
                |state| Ok(GuidingStats::in_state(state)),
            )
        }
        async fn guiding_metrics(&self) -> Result<GuidingMetrics, GuiderError> {
            unscripted("metrics read")
        }
        async fn devices(&self) -> Result<GuiderDevices, GuiderError> {
            let devices = self.devices.lock().unwrap().clone();
            devices.map_or_else(|| unscripted("devices read"), Ok)
        }
        async fn clear_calibration(&self) -> Result<GuiderState, GuiderError> {
            unscripted("calibration clear")
        }
        async fn reselect_star(&self) -> Result<GuiderState, GuiderError> {
            unscripted("star re-selection")
        }
        async fn acquire_camera_lease(
            &self,
            token: &str,
            ttl: Duration,
        ) -> Result<(), GuiderError> {
            let call = Call::Acquire {
                token: token.to_string(),
                ttl,
            };
            self.answer(call, &self.acquire).await
        }
        async fn renew_camera_lease(&self, token: &str, ttl: Duration) -> Result<(), GuiderError> {
            let call = Call::Renew {
                token: token.to_string(),
                ttl,
            };
            self.answer(call, &self.renewal).await
        }
        async fn release_camera_lease(&self, token: &str) -> Result<bool, GuiderError> {
            let call = Call::Release {
                token: token.to_string(),
            };
            self.answer(call, &self.release).await.map(|()| true)
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::test_support::{Answer, Call, FakeGuider};
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn refusing(code: &'static str, message: &'static str) -> FakeGuider {
        let guider = FakeGuider::default();
        *guider.acquire.lock().unwrap() = Answer::Refuse(code, message);
        guider
    }

    #[test]
    fn a_token_is_32_lowercase_hex_digits_and_fresh_each_time() {
        let token = new_token();

        assert_eq!(token.len(), 32);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_ne!(token, new_token());
    }

    #[tokio::test]
    async fn an_acquire_asks_under_the_token_for_a_ttl_of_60_s() {
        let guider = FakeGuider::default();

        acquire(&guider, TOKEN).await.unwrap();

        assert_eq!(
            guider.calls(),
            vec![Call::Acquire {
                token: TOKEN.to_string(),
                ttl: Duration::from_secs(60),
            }]
        );
    }

    #[tokio::test]
    async fn a_refusal_for_not_being_idle_names_the_guiders_state() {
        let guider = refusing("not_idle", "a camera lease needs an idle guider");
        *guider.state.lock().unwrap() = Some(GuiderState::StarLost);

        let error = acquire(&guider, TOKEN).await.unwrap_err();

        assert_eq!(
            error,
            "the guider refused the camera lease: not_idle: a camera lease needs an idle guider \
             (the guider's state: star_lost)"
        );
    }

    #[tokio::test]
    async fn a_refusal_whose_state_read_fails_relays_the_guiders_words_alone() {
        let guider = refusing("not_idle", "a camera lease needs an idle guider");

        let error = acquire(&guider, TOKEN).await.unwrap_err();

        assert_eq!(
            error,
            "the guider refused the camera lease: not_idle: a camera lease needs an idle guider"
        );
    }

    #[tokio::test]
    async fn a_refusal_from_a_guider_that_reads_idle_names_no_state() {
        let guider = refusing("not_idle", "a request is queued");
        *guider.state.lock().unwrap() = Some(GuiderState::Idle);

        let error = acquire(&guider, TOKEN).await.unwrap_err();

        assert_eq!(
            error,
            "the guider refused the camera lease: not_idle: a request is queued"
        );
    }

    #[tokio::test]
    async fn a_refusal_for_another_holder_is_relayed_as_it_came() {
        let guider = refusing("camera_leased", "another client holds the camera lease");
        *guider.state.lock().unwrap() = Some(GuiderState::Guiding);

        let error = acquire(&guider, TOKEN).await.unwrap_err();

        assert_eq!(
            error,
            "the guider refused the camera lease: camera_leased: another client holds the camera lease"
        );
    }

    #[tokio::test]
    async fn a_guider_that_cannot_be_reached_fails_the_acquire() {
        let guider = FakeGuider::default();
        *guider.acquire.lock().unwrap() = Answer::Unreachable;

        let error = acquire(&guider, TOKEN).await.unwrap_err();

        assert_eq!(
            error,
            "the guider's camera lease could not be taken: service unreachable: connection refused"
        );
    }

    #[tokio::test]
    async fn a_renewal_sets_the_ttl_anew() {
        let guider = FakeGuider::default();

        assert_eq!(renew(&guider, TOKEN).await, Renewal::Renewed);
        assert_eq!(
            guider.calls(),
            vec![Call::Renew {
                token: TOKEN.to_string(),
                ttl: Duration::from_secs(60),
            }]
        );
    }

    #[tokio::test]
    async fn a_renewal_the_guider_knows_no_lease_for_is_lost() {
        let guider = FakeGuider::default();
        *guider.renewal.lock().unwrap() = Answer::Refuse("lease_not_found", "no such lease");

        assert_eq!(
            renew(&guider, TOKEN).await,
            Renewal::Lost("no such lease".to_string())
        );
    }

    #[tokio::test]
    async fn a_renewal_that_fails_otherwise_did_not_reach_the_guider() {
        let guider = FakeGuider::default();
        *guider.renewal.lock().unwrap() = Answer::Unreachable;

        assert_eq!(
            renew(&guider, TOKEN).await,
            Renewal::Unreached("service unreachable: connection refused".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_with_no_answer_in_10_s_did_not_reach_the_guider() {
        let guider = FakeGuider::default();
        *guider.renewal.lock().unwrap() = Answer::Hang;

        assert_eq!(
            renew(&guider, TOKEN).await,
            Renewal::Unreached("no answer within 10s".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_is_best_effort_whatever_the_guider_answers() {
        for answer in [Answer::Grant, Answer::Unreachable, Answer::Hang] {
            let guider = FakeGuider::default();
            *guider.release.lock().unwrap() = answer;

            release(&guider, TOKEN).await;

            assert_eq!(guider.released(), vec![TOKEN.to_string()]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lease_is_renewed_every_20_s_and_released_under_its_token() {
        let guider = Arc::new(FakeGuider::default());
        let mut held = Held::new(Arc::clone(&guider) as Arc<dyn GuiderClient>);

        held.acquire().await.unwrap();
        tokio::time::sleep(Duration::from_secs(45)).await;
        assert_eq!(guider.renewals(), 2);
        held.release().await;

        let calls = guider.calls();
        let Some(Call::Acquire { token, .. }) = calls.first() else {
            panic!("the first call is the acquire: {calls:?}");
        };
        assert_eq!(guider.released(), vec![token.clone()]);

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(guider.renewals(), 2, "a released lease is renewed no more");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lease_the_guider_lost_is_renewed_no_more() {
        let guider = Arc::new(FakeGuider::default());
        *guider.renewal.lock().unwrap() = Answer::Refuse("lease_not_found", "no such lease");
        let mut held = Held::new(Arc::clone(&guider) as Arc<dyn GuiderClient>);

        held.acquire().await.unwrap();
        tokio::time::sleep(Duration::from_secs(90)).await;

        assert_eq!(guider.renewals(), 1);
        held.release().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lease_whose_renewal_did_not_arrive_is_renewed_again() {
        let guider = Arc::new(FakeGuider::default());
        *guider.renewal.lock().unwrap() = Answer::Unreachable;
        let mut held = Held::new(Arc::clone(&guider) as Arc<dyn GuiderClient>);

        held.acquire().await.unwrap();
        tokio::time::sleep(Duration::from_secs(45)).await;

        assert_eq!(guider.renewals(), 2);
        held.release().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_holder_is_still_released_and_never_renewed() {
        let guider = Arc::new(refusing("camera_leased", "another client holds it"));
        let mut held = Held::new(Arc::clone(&guider) as Arc<dyn GuiderClient>);

        held.acquire().await.unwrap_err();
        tokio::time::sleep(Duration::from_secs(45)).await;
        held.release().await;

        assert_eq!(guider.renewals(), 0);
        assert_eq!(guider.released().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_holder_stops_renewing() {
        let guider = Arc::new(FakeGuider::default());
        let mut held = Held::new(Arc::clone(&guider) as Arc<dyn GuiderClient>);

        held.acquire().await.unwrap();
        drop(held);
        tokio::time::sleep(Duration::from_secs(45)).await;

        assert_eq!(guider.renewals(), 0);
    }
}
