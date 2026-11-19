//! The guide camera guard (rp.md § The guide camera guard).
//!
//! The guider and rp are both Alpaca clients of the guiding train's
//! camera, the guide camera, and Alpaca gives a camera one exposure in
//! flight, one settings cache, one image slot and one abort for all its
//! clients. rp keeps its own captures through that camera and the starts
//! of guiding it sends off each other here, in its memory: the guider
//! serves nothing for it.
//!
//! Captures and a guiding train's train lease are rp's own users of the
//! camera and hold the guard together; a start holds it alone. Whichever
//! finds it held by the other kind is refused at once, naming the
//! holder, and nobody waits on it. A user that has taken it reads the
//! guider's state and keeps it only with the guider `idle`, so no start
//! of rp's lands between that read and the exposure.
//!
//! The guard exists while a guider is configured and a train guides;
//! the handler decides that, and holds one [`GuideCameraGuard`] for all
//! its clones.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rp_guider::{GuiderClient, GuiderState};
use tracing::debug;

/// One of rp's own users of the guide camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum User<'a> {
    /// A capture through the guide camera, under a train lease or not.
    Capture,
    /// A guiding train's train lease, by its id.
    Lease(&'a str),
}

#[derive(Debug, Default)]
struct Holders {
    captures: usize,
    /// The ids of the guiding train's leases that hold it. The lease
    /// book lets one lease reserve the camera at a time, so this holds
    /// one id at most.
    leases: Vec<String>,
    /// The tool whose start of guiding holds it.
    start: Option<&'static str>,
}

impl Holders {
    /// Why a start cannot take the guard: the user that holds it, a
    /// lease named before a capture, since a capture under the lease
    /// runs inside it.
    fn user_in_the_way(&self) -> Option<String> {
        if let Some(lease_id) = self.leases.first() {
            return Some(format!("train lease '{lease_id}'"));
        }
        (self.captures > 0).then(|| "a capture".to_string())
    }
}

/// The guide camera guard. See the module docs.
#[derive(Debug, Default)]
pub struct GuideCameraGuard {
    holders: Mutex<Holders>,
}

impl GuideCameraGuard {
    fn holders(&self) -> MutexGuard<'_, Holders> {
        // A poisoned lock only means a holder panicked between two plain
        // field writes; the counts are still whole.
        self.holders.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take the guard for a start of guiding sent by `tool`
    /// (`start_guiding`, or `move_rotator` for the ladder's start).
    ///
    /// # Errors
    ///
    /// The guard is held: by a capture or a guiding train's lease, or by
    /// another start.
    pub fn take_for_start(
        self: &Arc<Self>,
        camera_id: &str,
        tool: &'static str,
    ) -> Result<GuardHold, String> {
        let mut holders = self.holders();
        if let Some(holder) = holders
            .user_in_the_way()
            .or_else(|| holders.start.map(str::to_string))
        {
            debug!(%camera_id, tool, %holder, "a start of guiding is refused: the guide camera guard is held");
            return Err(in_use(camera_id, &holder));
        }
        holders.start = Some(tool);
        drop(holders);
        debug!(%camera_id, tool, "a start of guiding holds the guide camera guard");
        Ok(GuardHold {
            guard: Arc::clone(self),
            held_as: HeldAs::Start,
        })
    }

    /// Take the guard for one of rp's own users, then read the guider's
    /// state, bounded by `bound`, and keep the guard only when the
    /// guider is `idle`. The read is the only request the guard sends
    /// the guider.
    ///
    /// # Errors
    ///
    /// A start of guiding holds the guard, the guider is not `idle`, or
    /// its state cannot be read. The guard is given back.
    pub async fn take_for_use(
        self: &Arc<Self>,
        camera_id: &str,
        user: User<'_>,
        guider: &dyn GuiderClient,
        bound: Duration,
    ) -> Result<GuardHold, String> {
        let hold = self.take_for_user(camera_id, user)?;
        let state = match tokio::time::timeout(bound, guider.guiding_stats()).await {
            Ok(Ok(stats)) => stats.state,
            Ok(Err(e)) => return Err(unread(camera_id, &e.to_string())),
            Err(_) => {
                let bound = humantime::format_duration(bound);
                return Err(unread(camera_id, &format!("no answer within {bound}")));
            }
        };
        if state != GuiderState::Idle {
            debug!(%camera_id, ?user, %state, "the guider is not idle; the guide camera guard goes back");
            return Err(format!(
                "the guider is {state}; rp uses the guide camera '{camera_id}' only while the guider is idle"
            ));
        }
        debug!(%camera_id, ?user, "the guider reads idle; the user keeps the guide camera guard");
        Ok(hold)
    }

    fn take_for_user(
        self: &Arc<Self>,
        camera_id: &str,
        user: User<'_>,
    ) -> Result<GuardHold, String> {
        let mut holders = self.holders();
        if let Some(tool) = holders.start {
            debug!(%camera_id, ?user, tool, "refused: a start of guiding holds the guide camera guard");
            return Err(in_use(camera_id, tool));
        }
        let held_as = match user {
            User::Capture => {
                holders.captures = holders.captures.saturating_add(1);
                HeldAs::Capture
            }
            User::Lease(lease_id) => {
                holders.leases.push(lease_id.to_string());
                HeldAs::Lease(lease_id.to_string())
            }
        };
        drop(holders);
        Ok(GuardHold {
            guard: Arc::clone(self),
            held_as,
        })
    }

    fn give_back(&self, held_as: &HeldAs) {
        let mut holders = self.holders();
        match held_as {
            HeldAs::Capture => holders.captures = holders.captures.saturating_sub(1),
            HeldAs::Lease(lease_id) => {
                if let Some(at) = holders.leases.iter().position(|id| id == lease_id) {
                    holders.leases.remove(at);
                }
            }
            HeldAs::Start => holders.start = None,
        }
    }
}

fn in_use(camera_id: &str, holder: &str) -> String {
    format!("the guide camera '{camera_id}' is in use by {holder}")
}

fn unread(camera_id: &str, error: &str) -> String {
    format!(
        "could not read the guider's state before using the guide camera '{camera_id}': {error}"
    )
}

#[derive(Debug)]
enum HeldAs {
    Capture,
    Lease(String),
    Start,
}

/// One hold of the guide camera guard; dropping it gives the guard back.
#[derive(Debug)]
#[must_use = "dropping it gives the guide camera guard back"]
pub struct GuardHold {
    guard: Arc<GuideCameraGuard>,
    held_as: HeldAs,
}

impl Drop for GuardHold {
    fn drop(&mut self) {
        self.guard.give_back(&self.held_as);
    }
}

/// A fake guider for the guard's tests and the handler's: it answers a
/// stats read in the state its field holds, and a devices read as its
/// field says, and counts the stats reads.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use rp_guider::{
        DitherRequest, GuiderClient, GuiderDevices, GuiderError, GuiderState, GuidingMetrics,
        GuidingStats, SettledOutcome, StartGuidingRequest,
    };

    /// How the fake answers a stats read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Stats {
        In(GuiderState),
        Unreachable,
        /// No answer, ever.
        Hang,
    }

    #[derive(Debug)]
    pub struct FakeGuider {
        pub stats: Mutex<Stats>,
        /// What `devices` reports; the read fails with none.
        pub devices: Mutex<Option<GuiderDevices>>,
        stats_reads: AtomicUsize,
    }

    impl Default for FakeGuider {
        fn default() -> Self {
            Self {
                stats: Mutex::new(Stats::In(GuiderState::Idle)),
                devices: Mutex::new(None),
                stats_reads: AtomicUsize::new(0),
            }
        }
    }

    impl FakeGuider {
        /// A guider whose stats read answers `stats`.
        pub fn answering(stats: Stats) -> Self {
            let guider = Self::default();
            *guider.stats.lock().unwrap() = stats;
            guider
        }

        /// How many stats reads have arrived.
        pub fn stats_reads(&self) -> usize {
            self.stats_reads.load(Ordering::SeqCst)
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
            self.stats_reads.fetch_add(1, Ordering::SeqCst);
            let stats = *self.stats.lock().unwrap();
            match stats {
                Stats::In(state) => Ok(GuidingStats::in_state(state)),
                Stats::Unreachable => Err(GuiderError::ServiceUnreachable(
                    "connection refused".to_string(),
                )),
                Stats::Hang => std::future::pending().await,
            }
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
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::test_support::{FakeGuider, Stats};
    use super::*;

    const CAM: &str = "guide-cam";
    const BOUND: Duration = Duration::from_secs(90);

    fn idle() -> FakeGuider {
        FakeGuider::answering(Stats::In(GuiderState::Idle))
    }

    #[tokio::test]
    async fn a_start_is_refused_at_once_while_a_capture_holds_the_guard() {
        let guard = Arc::new(GuideCameraGuard::default());
        let capture = guard
            .take_for_use(CAM, User::Capture, &idle(), BOUND)
            .await
            .unwrap();

        let err = guard.take_for_start(CAM, "start_guiding").unwrap_err();

        assert_eq!(err, "the guide camera 'guide-cam' is in use by a capture");
        drop(capture);
    }

    #[tokio::test]
    async fn a_start_is_refused_at_once_while_a_lease_holds_the_guard_naming_the_lease() {
        let guard = Arc::new(GuideCameraGuard::default());
        let lease = guard
            .take_for_use(CAM, User::Lease("lease-g"), &idle(), BOUND)
            .await
            .unwrap();
        let capture = guard
            .take_for_use(CAM, User::Capture, &idle(), BOUND)
            .await
            .unwrap();

        let err = guard.take_for_start(CAM, "move_rotator").unwrap_err();

        assert_eq!(
            err,
            "the guide camera 'guide-cam' is in use by train lease 'lease-g'"
        );
        drop((capture, lease));
    }

    #[tokio::test]
    async fn a_capture_and_a_lease_are_refused_at_once_while_a_start_holds_the_guard() {
        let guard = Arc::new(GuideCameraGuard::default());
        let guider = idle();
        let start = guard.take_for_start(CAM, "start_guiding").unwrap();

        for user in [User::Capture, User::Lease("lease-g")] {
            let err = guard
                .take_for_use(CAM, user, &guider, BOUND)
                .await
                .unwrap_err();
            assert_eq!(
                err,
                "the guide camera 'guide-cam' is in use by start_guiding"
            );
        }
        assert_eq!(guider.stats_reads(), 0, "a refused user read the guider");
        drop(start);
    }

    #[tokio::test]
    async fn a_second_start_is_refused_while_a_start_holds_the_guard() {
        let guard = Arc::new(GuideCameraGuard::default());
        let start = guard.take_for_start(CAM, "move_rotator").unwrap();

        let err = guard.take_for_start(CAM, "start_guiding").unwrap_err();

        assert_eq!(
            err,
            "the guide camera 'guide-cam' is in use by move_rotator"
        );
        drop(start);
    }

    #[tokio::test]
    async fn captures_and_a_lease_hold_the_guard_together() {
        let guard = Arc::new(GuideCameraGuard::default());
        let guider = idle();

        let lease = guard
            .take_for_use(CAM, User::Lease("lease-g"), &guider, BOUND)
            .await
            .unwrap();
        let first = guard
            .take_for_use(CAM, User::Capture, &guider, BOUND)
            .await
            .unwrap();
        let second = guard
            .take_for_use(CAM, User::Capture, &guider, BOUND)
            .await
            .unwrap();

        assert_eq!(guider.stats_reads(), 3, "every user reads the guider");
        drop((first, second, lease));
    }

    #[tokio::test]
    async fn every_holder_given_back_lets_a_start_through() {
        let guard = Arc::new(GuideCameraGuard::default());
        let lease = guard
            .take_for_use(CAM, User::Lease("lease-g"), &idle(), BOUND)
            .await
            .unwrap();
        let capture = guard
            .take_for_use(CAM, User::Capture, &idle(), BOUND)
            .await
            .unwrap();

        drop(capture);
        assert!(guard.take_for_start(CAM, "start_guiding").is_err());
        drop(lease);
        let start = guard.take_for_start(CAM, "start_guiding").unwrap();

        drop(start);
        drop(
            guard
                .take_for_use(CAM, User::Capture, &idle(), BOUND)
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn a_guider_that_is_not_idle_refuses_the_user_naming_its_state() {
        for state in [
            GuiderState::Looping,
            GuiderState::Calibrating,
            GuiderState::Guiding,
            GuiderState::Paused,
            GuiderState::StarLost,
        ] {
            let guard = Arc::new(GuideCameraGuard::default());
            let guider = FakeGuider::answering(Stats::In(state));

            let err = guard
                .take_for_use(CAM, User::Capture, &guider, BOUND)
                .await
                .unwrap_err();

            assert_eq!(
                err,
                format!(
                    "the guider is {state}; rp uses the guide camera 'guide-cam' only while the guider is idle"
                )
            );
            drop(
                guard
                    .take_for_start(CAM, "start_guiding")
                    .expect("a refused user kept the guard"),
            );
        }
    }

    #[tokio::test]
    async fn a_failed_read_refuses_the_user_naming_the_error() {
        let guard = Arc::new(GuideCameraGuard::default());
        let guider = FakeGuider::answering(Stats::Unreachable);

        let err = guard
            .take_for_use(CAM, User::Lease("lease-g"), &guider, BOUND)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            "could not read the guider's state before using the guide camera 'guide-cam': \
             service unreachable: connection refused"
        );
        drop(
            guard
                .take_for_start(CAM, "start_guiding")
                .expect("a refused user kept the guard"),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_with_no_answer_refuses_the_user_at_the_bound() {
        let guard = Arc::new(GuideCameraGuard::default());
        let guider = FakeGuider::answering(Stats::Hang);

        let err = guard
            .take_for_use(CAM, User::Capture, &guider, Duration::from_secs(2))
            .await
            .unwrap_err();

        assert_eq!(
            err,
            "could not read the guider's state before using the guide camera 'guide-cam': \
             no answer within 2s"
        );
        drop(
            guard
                .take_for_start(CAM, "start_guiding")
                .expect("a refused user kept the guard"),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_finds_the_guard_held_while_the_users_read_is_in_flight() {
        let guard = Arc::new(GuideCameraGuard::default());
        let guider = Arc::new(FakeGuider::answering(Stats::Hang));
        let reading = {
            let (guard, guider) = (Arc::clone(&guard), Arc::clone(&guider));
            tokio::spawn(async move {
                guard
                    .take_for_use(CAM, User::Capture, guider.as_ref(), BOUND)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;

        let err = guard.take_for_start(CAM, "start_guiding").unwrap_err();

        assert_eq!(err, "the guide camera 'guide-cam' is in use by a capture");
        reading.abort();
        let _ = reading.await;
        drop(
            guard
                .take_for_start(CAM, "start_guiding")
                .expect("a dropped read kept the guard"),
        );
    }
}
