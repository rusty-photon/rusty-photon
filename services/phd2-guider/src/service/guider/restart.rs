//! PHD2's restart (`docs/services/phd2-guider.md` § "`POST
//! /api/v1/phd2/restart`"): stop PHD2, let its keeper start it again, and
//! connect its equipment.
//!
//! The restart holds the single-flight mutex throughout, and everything
//! after the mutex is bounded by `phd2.restart_timeout`. Its steps carry
//! the design's names (`refusals`, `stop`, `start`, `answer`,
//! `equipment`), and a failure answers `restart_failed` naming the step it
//! failed in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::config::{is_loopback_host, Phd2Config};
use crate::error::Phd2Error;
use crate::events::AppState;

use super::super::error::ServiceError;
use super::super::keeper::{why_no_keeper, Keeper, ProcessId};
use super::GuiderOps;

/// How long PHD2 takes to start its event server. A PHD2 process that has
/// run longer without accepting a connection hung before its event server
/// started.
const PHD2_START: Duration = Duration::from_mins(1);

/// How long PHD2 has to exit after `shutdown` before it is ended.
const EXIT_WAIT: Duration = Duration::from_secs(30);

/// How often the keeper is asked about PHD2's process, and PHD2 about its
/// equipment.
const KEEPER_POLL: Duration = Duration::from_millis(500);

/// How often an exiting PHD2, and a reconnecting client, are looked at.
const QUICK_POLL: Duration = Duration::from_millis(100);

/// What a restart of PHD2 takes from the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartSettings {
    /// `phd2.managed`: the host is switched over.
    pub managed: bool,
    /// `phd2.host` is `localhost` or a loopback address.
    pub local_phd2: bool,
    /// `phd2.restart_timeout`.
    pub timeout: Duration,
}

impl RestartSettings {
    #[must_use]
    pub fn from_config(phd2: &Phd2Config) -> Self {
        Self {
            managed: phd2.managed,
            local_phd2: is_loopback_host(&phd2.host),
            timeout: phd2.restart_timeout,
        }
    }
}

impl Default for RestartSettings {
    fn default() -> Self {
        Self::from_config(&Phd2Config::default())
    }
}

/// A restart that succeeded.
#[derive(Debug, Clone, Copy)]
pub struct RestartDone {
    /// From when the restart held the single-flight mutex.
    pub elapsed: Duration,
}

/// The design's steps of a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Refusals,
    Stop,
    Start,
    Answer,
    Equipment,
}

impl Step {
    const fn name(self) -> &'static str {
        match self {
            Self::Refusals => "refusals",
            Self::Stop => "stop",
            Self::Start => "start",
            Self::Answer => "answer",
            Self::Equipment => "equipment",
        }
    }

    fn failed(self, detail: impl Into<String>) -> ServiceError {
        ServiceError::RestartFailed {
            step: self.name(),
            detail: detail.into(),
        }
    }
}

/// What PHD2 turned out to be once a restart looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// It answers: it is stopped through its RPC.
    Answers,
    /// It accepted the connection and did not answer: it is ended.
    NoAnswer,
    /// Its process has run longer than PHD2 takes to start and still
    /// accepts no connection: it is ended.
    NeverAccepted(ProcessId),
}

/// The requests that stop an answering PHD2, in order.
#[derive(Debug, Clone, Copy)]
enum StopCall {
    StopCapture,
    DisconnectEquipment,
    Shutdown,
}

impl StopCall {
    const ALL: [Self; 3] = [Self::StopCapture, Self::DisconnectEquipment, Self::Shutdown];

    const fn name(self) -> &'static str {
        match self {
            Self::StopCapture => "stop_capture",
            Self::DisconnectEquipment => "set_connected(false)",
            Self::Shutdown => "shutdown",
        }
    }
}

/// PHD2's states that guide: a restart ends none of them.
const fn guides(state: AppState) -> bool {
    matches!(
        state,
        AppState::Guiding | AppState::Calibrating | AppState::Paused | AppState::LostLock
    )
}

/// Marks a restart under way while it lives.
pub struct Restarting<'a>(&'a AtomicBool);

impl Drop for Restarting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl GuiderOps {
    /// Claim the camera facade's camera for a connect. Refused while PHD2
    /// restarts: the restart ends PHD2's camera connection.
    ///
    /// Each side marks itself and then reads the other (sequentially
    /// consistent), so a connect and a restart that begin together cannot
    /// both miss each other.
    ///
    /// # Errors
    ///
    /// Returns why the camera cannot be connected now.
    pub(crate) fn claim_facade_camera(&self) -> Result<(), String> {
        self.facade_camera.store(true, Ordering::SeqCst);
        if self.restarting.load(Ordering::SeqCst) {
            self.facade_camera.store(false, Ordering::SeqCst);
            return Err("PHD2 is restarting; connect the camera once it is back".to_string());
        }
        Ok(())
    }

    /// The facade's camera is disconnected, or its connect failed.
    pub(crate) fn release_facade_camera(&self) {
        self.facade_camera.store(false, Ordering::SeqCst);
    }

    /// Whether the facade's camera is claimed.
    #[cfg(test)]
    pub(crate) fn facade_camera_claimed(&self) -> bool {
        self.facade_camera.load(Ordering::SeqCst)
    }

    /// Mark a restart under way, refused with `camera_connected` while the
    /// facade's camera is connected or connecting.
    pub(crate) fn begin_restart(&self) -> Result<Restarting<'_>, ServiceError> {
        self.restarting.store(true, Ordering::SeqCst);
        if self.facade_camera.load(Ordering::SeqCst) {
            self.restarting.store(false, Ordering::SeqCst);
            let message = "a client has the camera facade's camera connected, and the restart \
                           ends PHD2's camera connection under it; disconnect the camera first";
            debug!("{message}");
            return Err(ServiceError::CameraConnected(message.to_string()));
        }
        Ok(Restarting(&self.restarting))
    }

    /// Restart PHD2: stop it, let its keeper start it again, wait for it to
    /// answer, and connect its equipment.
    ///
    /// # Errors
    ///
    /// `restart_unsupported` where nothing would start PHD2 again,
    /// `camera_connected` and `camera_busy` while the camera facade holds
    /// the guide camera, `guiding_active` while PHD2 guides, and
    /// `restart_failed` naming the step that failed or ran out of
    /// `phd2.restart_timeout`.
    pub async fn restart_phd2(&self) -> Result<RestartDone, ServiceError> {
        let _op = self.op_lock.lock().await;
        let begun = tokio::time::Instant::now();
        let keeper = self.restart_keeper()?;
        let limit = self.restart.timeout;
        debug!(
            "restarting PHD2 (bound {})",
            humantime::format_duration(limit)
        );
        let mut step = Step::Refusals;
        let finished =
            tokio::time::timeout(limit, self.run_restart(keeper.as_ref(), &mut step)).await;
        match finished {
            Ok(Ok(())) => {
                let elapsed = begun.elapsed();
                info!(
                    "PHD2 restarted and its equipment connected in {}",
                    humantime::format_duration(elapsed)
                );
                Ok(RestartDone { elapsed })
            }
            Ok(Err(e)) => {
                debug!("restart of PHD2 not done: {e}");
                Err(e)
            }
            Err(_) => {
                let e = step.failed(format!(
                    "not done when phd2.restart_timeout ({}) ran out",
                    humantime::format_duration(limit)
                ));
                warn!("{e}");
                Err(e)
            }
        }
    }

    /// The refusals that need no keeper: `phd2.managed`, a local PHD2, and
    /// a keeper on this host.
    fn restart_keeper(&self) -> Result<Arc<dyn Keeper>, ServiceError> {
        let unsupported = |why: &str| {
            debug!("restart refused: {why}");
            Err(ServiceError::RestartUnsupported(why.to_string()))
        };
        if !self.restart.managed {
            return unsupported(
                "phd2.managed is false: this host is not switched over to PHD2 under the \
                 service manager, so nothing would start PHD2 again",
            );
        }
        if !self.restart.local_phd2 {
            return unsupported(
                "phd2.host is neither localhost nor a loopback address, so the PHD2 this \
                 service talks to is not the one this host keeps",
            );
        }
        self.keeper.as_ref().map_or_else(
            || unsupported(why_no_keeper()),
            |keeper| Ok(Arc::clone(keeper)),
        )
    }

    async fn run_restart(&self, keeper: &dyn Keeper, step: &mut Step) -> Result<(), ServiceError> {
        let kept = keeper.state().await.map_err(|e| Step::Refusals.failed(e))?;
        if !kept.restarts {
            let why = format!(
                "{} is {}, so nothing would start PHD2 again",
                keeper.name(),
                kept.state
            );
            debug!("restart refused: {why}");
            return Err(ServiceError::RestartUnsupported(why));
        }
        let _restarting = self.begin_restart()?;
        self.refuse_while_camera_held()?;
        let found = self.find_phd2(keeper, kept.main).await?;

        *step = Step::Stop;
        let ended = self.stop_phd2(keeper, found).await?;
        // Every connection from here on is to a PHD2 started after the old
        // one was gone.
        let connections = self.client.connection_count();

        *step = Step::Start;
        let started = self.wait_started(keeper, ended).await?;
        debug!(pid = started.pid, "{} started PHD2 again", keeper.name());

        *step = Step::Answer;
        self.wait_answer(connections).await;
        debug!("the new PHD2 answers");

        *step = Step::Equipment;
        self.connect_equipment().await
    }

    /// Ask PHD2 its state: refused while it guides, stopped through its RPC
    /// when it answers, ended when it accepts the connection and does not
    /// answer. With no connection at all, wait while its process is younger
    /// than PHD2's start, and end one older.
    async fn find_phd2(
        &self,
        keeper: &dyn Keeper,
        mut main: Option<ProcessId>,
    ) -> Result<Found, ServiceError> {
        loop {
            if self.client.is_connected().await {
                match self.client.get_app_state().await {
                    Ok(state) if guides(state) => {
                        let message = format!(
                            "PHD2 is {state}; a restart ends no guiding or calibration, so stop \
                             guiding first (POST /api/v1/guiding/stop)"
                        );
                        debug!("{message}");
                        return Err(ServiceError::GuidingActive(message));
                    }
                    Ok(state) => {
                        debug!(%state, "PHD2 answers");
                        return Ok(Found::Answers);
                    }
                    Err(e @ (Phd2Error::RpcError { .. } | Phd2Error::InvalidState(_))) => {
                        debug!("PHD2 answers get_app_state with an error: {e}");
                        return Ok(Found::Answers);
                    }
                    Err(Phd2Error::Timeout(_)) => {
                        debug!("PHD2 accepted the connection and does not answer");
                        return Ok(Found::NoAnswer);
                    }
                    Err(e) => debug!("PHD2's connection changed while it was asked: {e}"),
                }
            } else if let Some(process) = main {
                match keeper
                    .age(process)
                    .await
                    .map_err(|e| Step::Refusals.failed(e))?
                {
                    Some(age) if age >= PHD2_START => {
                        return Ok(Found::NeverAccepted(process));
                    }
                    Some(age) => debug!(
                        pid = process.pid,
                        "PHD2 accepts no connection after {}; waiting while it starts",
                        humantime::format_duration(age)
                    ),
                    None => main = None,
                }
            } else {
                main = keeper
                    .state()
                    .await
                    .map_err(|e| Step::Refusals.failed(e))?
                    .main;
            }
            tokio::time::sleep(KEEPER_POLL).await;
        }
    }

    /// Stop PHD2 as `found` calls for. The process the keeper knew as PHD2,
    /// which no longer runs once this returns.
    async fn stop_phd2(
        &self,
        keeper: &dyn Keeper,
        found: Found,
    ) -> Result<Option<ProcessId>, ServiceError> {
        let end = |process: ProcessId, why: &str| {
            warn!(pid = process.pid, "ending PHD2: {why}");
            keeper.end(process)
        };
        let remembered = match found {
            Found::NeverAccepted(process) => Some(process),
            Found::Answers | Found::NoAnswer => {
                keeper.state().await.map_err(|e| Step::Stop.failed(e))?.main
            }
        };
        match (found, remembered) {
            (Found::Answers, _) => {
                self.ask_phd2_to_exit().await;
                if let Some(process) = remembered {
                    if !self.wait_exit(keeper, process).await? {
                        end(process, "it has not exited 30 s after shutdown")
                            .await
                            .map_err(|e| Step::Stop.failed(e))?;
                    }
                }
            }
            (Found::NoAnswer, Some(process)) => {
                end(process, "it accepted the connection and did not answer")
                    .await
                    .map_err(|e| Step::Stop.failed(e))?;
            }
            (Found::NeverAccepted(_), Some(process)) => {
                end(process, "it has run 60 s and accepts no connection")
                    .await
                    .map_err(|e| Step::Stop.failed(e))?;
            }
            (Found::NoAnswer | Found::NeverAccepted(_), None) => {
                debug!("{} runs no PHD2 to end", keeper.name());
            }
        }
        Ok(remembered)
    }

    /// `stop_capture`, `set_connected(false)` and `shutdown`, each best
    /// effort: one that fails moves on to the next, and one that times out
    /// moves on to the exit wait, since PHD2's main thread then hangs and a
    /// further request would hang too.
    async fn ask_phd2_to_exit(&self) {
        for call in StopCall::ALL {
            let sent = match call {
                StopCall::StopCapture => self.client.stop_capture().await,
                StopCall::DisconnectEquipment => self.client.disconnect_equipment().await,
                StopCall::Shutdown => self.client.shutdown_phd2().await,
            };
            match sent {
                Ok(()) => debug!("PHD2 accepted {}", call.name()),
                Err(Phd2Error::Timeout(_)) => {
                    debug!(
                        "PHD2 did not answer {}: waiting for it to exit",
                        call.name()
                    );
                    return;
                }
                Err(e) => debug!("{} failed: {e}; going on", call.name()),
            }
        }
    }

    /// Wait up to [`EXIT_WAIT`] for `process` to exit. Whether it did.
    async fn wait_exit(
        &self,
        keeper: &dyn Keeper,
        process: ProcessId,
    ) -> Result<bool, ServiceError> {
        let deadline = tokio::time::Instant::now().checked_add(EXIT_WAIT);
        loop {
            if keeper
                .age(process)
                .await
                .map_err(|e| Step::Stop.failed(e))?
                .is_none()
            {
                debug!(pid = process.pid, "PHD2 exited");
                return Ok(true);
            }
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return Ok(false);
            }
            tokio::time::sleep(QUICK_POLL).await;
        }
    }

    /// Wait for the keeper to run a PHD2 process other than `ended`.
    async fn wait_started(
        &self,
        keeper: &dyn Keeper,
        ended: Option<ProcessId>,
    ) -> Result<ProcessId, ServiceError> {
        loop {
            let kept = keeper.state().await.map_err(|e| Step::Start.failed(e))?;
            if kept.stopped {
                return Err(Step::Start.failed(format!(
                    "{} is {}, so it starts no PHD2 again",
                    keeper.name(),
                    kept.state
                )));
            }
            if let Some(process) = kept.main.filter(|&process| Some(process) != ended) {
                return Ok(process);
            }
            tokio::time::sleep(KEEPER_POLL).await;
        }
    }

    /// Wait for a connection made after `connections` connections to carry
    /// PHD2's `Version` event: the new PHD2's event server answers.
    async fn wait_answer(&self, connections: u64) {
        loop {
            if self.client.connection_count() > connections
                && self.client.is_connected().await
                && self.client.get_phd2_version().await.is_some()
            {
                return;
            }
            tokio::time::sleep(QUICK_POLL).await;
        }
    }

    /// `set_connected(true)`, then `get_connected` until it reads true.
    async fn connect_equipment(&self) -> Result<(), ServiceError> {
        match self.client.connect_equipment().await {
            Ok(()) => debug!("PHD2 accepted set_connected(true)"),
            // PHD2 connects the equipment on its main thread, which may take
            // longer than one request; get_connected answers once it is done.
            Err(Phd2Error::Timeout(_)) => debug!("set_connected(true) still runs"),
            Err(e) => {
                return Err(Step::Equipment.failed(format!("set_connected(true) failed: {e}")));
            }
        }
        loop {
            match self.client.is_equipment_connected().await {
                Ok(true) => return Ok(()),
                Ok(false) | Err(Phd2Error::Timeout(_)) => {}
                Err(e) => {
                    return Err(Step::Equipment.failed(format!("get_connected failed: {e}")));
                }
            }
            tokio::time::sleep(KEEPER_POLL).await;
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::super::super::keeper::Kept;
    use super::*;
    use crate::client::Phd2Client;
    use crate::config::SettleParams;

    /// A keeper whose PHD2 never accepts a connection: it runs `main`,
    /// which started `age_at_start` before the test began, and starts
    /// `next` (when set) once `main` is ended.
    struct FakeKeeper {
        state: Mutex<FakeState>,
        begun: tokio::time::Instant,
    }

    struct FakeState {
        active_state: &'static str,
        main: Option<ProcessId>,
        age_at_start: Duration,
        next: Option<ProcessId>,
        /// When each `end` was asked, after the test began.
        ended: Vec<(ProcessId, Duration)>,
    }

    const OLD: ProcessId = ProcessId {
        pid: 100,
        started: 1,
    };
    const NEW: ProcessId = ProcessId {
        pid: 200,
        started: 2,
    };

    impl FakeKeeper {
        fn new(
            active_state: &'static str,
            age_at_start: Duration,
            next: Option<ProcessId>,
        ) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    active_state,
                    main: Some(OLD),
                    age_at_start,
                    next,
                    ended: Vec::new(),
                }),
                begun: tokio::time::Instant::now(),
            }
        }

        fn ended(&self) -> Vec<(ProcessId, Duration)> {
            self.state.lock().unwrap().ended.clone()
        }
    }

    #[async_trait]
    impl Keeper for FakeKeeper {
        fn name(&self) -> &'static str {
            "fake-phd2"
        }

        async fn state(&self) -> Result<Kept, String> {
            let state = self.state.lock().unwrap();
            Ok(Kept {
                state: state.active_state.to_string(),
                restarts: matches!(state.active_state, "active" | "activating"),
                stopped: matches!(state.active_state, "inactive" | "failed"),
                main: state.main,
            })
        }

        async fn age(&self, process: ProcessId) -> Result<Option<Duration>, String> {
            let state = self.state.lock().unwrap();
            Ok((state.main == Some(process))
                .then(|| state.age_at_start.saturating_add(self.begun.elapsed())))
        }

        async fn end(&self, process: ProcessId) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            let at = self.begun.elapsed();
            state.ended.push((process, at));
            if state.main == Some(process) {
                state.main = state.next;
            }
            Ok(())
        }
    }

    /// Guiding operations whose PHD2 client never connects, restarting
    /// through `keeper`.
    fn ops_with(keeper: Option<Arc<dyn Keeper>>, settings: RestartSettings) -> GuiderOps {
        let client = Arc::new(Phd2Client::new(Phd2Config::default()));
        GuiderOps::new(client, SettleParams::default(), Duration::from_secs(10))
            .with_restart(settings, keeper)
    }

    fn managed() -> RestartSettings {
        RestartSettings {
            managed: true,
            ..RestartSettings::default()
        }
    }

    fn unsupported_message(err: ServiceError) -> String {
        match err {
            ServiceError::RestartUnsupported(message) => message,
            other => panic!("expected restart_unsupported, got {other:?}"),
        }
    }

    #[test]
    fn the_settings_follow_the_config() {
        let settings = RestartSettings::default();
        assert!(!settings.managed);
        assert!(settings.local_phd2);
        assert_eq!(settings.timeout, Duration::from_mins(2));
        let remote = Phd2Config {
            host: "192.0.2.10".to_string(),
            managed: true,
            ..Phd2Config::default()
        };
        let settings = RestartSettings::from_config(&remote);
        assert!(settings.managed);
        assert!(!settings.local_phd2);
    }

    // The refusal tests run on paused time, so a refusal that regresses
    // into a whole restart fails at once rather than after the bound.
    #[tokio::test(start_paused = true)]
    async fn a_host_not_switched_over_refuses_a_restart() {
        let keeper = Arc::new(FakeKeeper::new("active", Duration::ZERO, None));
        let ops = ops_with(Some(keeper.clone()), RestartSettings::default());
        let message = unsupported_message(ops.restart_phd2().await.unwrap_err());
        assert!(message.contains("phd2.managed"), "{message}");
        assert_eq!(keeper.ended(), Vec::new(), "PHD2 was ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_phd2_on_another_host_refuses_a_restart() {
        let keeper = Arc::new(FakeKeeper::new("active", Duration::ZERO, None));
        let settings = RestartSettings {
            local_phd2: false,
            ..managed()
        };
        let ops = ops_with(Some(keeper), settings);
        let message = unsupported_message(ops.restart_phd2().await.unwrap_err());
        assert!(message.contains("loopback"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_without_a_keeper_refuses_a_restart() {
        let ops = ops_with(None, managed());
        let message = unsupported_message(ops.restart_phd2().await.unwrap_err());
        assert_eq!(message, why_no_keeper());
    }

    #[tokio::test(start_paused = true)]
    async fn a_keeper_that_does_not_run_phd2_refuses_a_restart() {
        for state in ["inactive", "failed", "deactivating"] {
            let keeper = Arc::new(FakeKeeper::new(state, Duration::ZERO, None));
            let ops = ops_with(Some(keeper), managed());
            let message = unsupported_message(ops.restart_phd2().await.unwrap_err());
            assert!(
                message.contains(&format!("fake-phd2 is {state}")),
                "{message}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_connected_facade_camera_refuses_a_restart() {
        let keeper = Arc::new(FakeKeeper::new("active", Duration::ZERO, None));
        let ops = ops_with(Some(keeper.clone()), managed());
        ops.claim_facade_camera().unwrap();
        let err = ops.restart_phd2().await.unwrap_err();
        assert!(matches!(err, ServiceError::CameraConnected(_)), "{err:?}");
        assert_eq!(keeper.ended(), Vec::new(), "PHD2 was ended");
        // The refusal left no restart marked: the camera can still be
        // disconnected and connected again.
        ops.release_facade_camera();
        ops.claim_facade_camera().unwrap();
    }

    #[test]
    fn a_camera_connect_during_a_restart_is_refused() {
        let ops = ops_with(None, managed());
        let restarting = ops.begin_restart().unwrap();
        let err = ops.claim_facade_camera().unwrap_err();
        assert!(err.contains("restarting"), "{err}");
        drop(restarting);
        ops.claim_facade_camera().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_facade_frame_in_flight_refuses_a_restart_with_camera_busy() {
        let keeper = Arc::new(FakeKeeper::new("active", Duration::ZERO, None));
        let ops = ops_with(Some(keeper.clone()), managed());
        {
            let op = ops.op_lock().lock().await;
            ops.claim_capture(
                &op,
                super::super::CaptureClaim {
                    path: "/frames/a.fits".into(),
                    expires: tokio::time::Instant::now() + Duration::from_mins(1),
                    deadline: std::time::SystemTime::now() + Duration::from_mins(1),
                    stop_requested: false,
                    recovered: false,
                },
            );
        }
        let err = ops.restart_phd2().await.unwrap_err();
        assert!(matches!(err, ServiceError::CameraBusy(_)), "{err:?}");
        assert_eq!(keeper.ended(), Vec::new(), "PHD2 was ended");
        // A refused restart leaves no restart marked.
        ops.claim_facade_camera().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_phd2_that_never_accepts_a_connection_is_ended_once_it_has_run_60_s() {
        // 10 s old when the restart begins: waited for until it has run 60 s.
        let keeper = Arc::new(FakeKeeper::new(
            "active",
            Duration::from_secs(10),
            Some(NEW),
        ));
        let ops = ops_with(Some(keeper.clone()), managed());
        let err = ops.restart_phd2().await.unwrap_err();

        let ended = keeper.ended();
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].0, OLD);
        let at = ended[0].1;
        assert!(
            at >= Duration::from_secs(50) && at < Duration::from_secs(51),
            "ended {at:?} into the restart, not when it had run 60 s"
        );
        // The keeper started PHD2 again, and nothing ever answered.
        match err {
            ServiceError::RestartFailed { step, detail } => {
                assert_eq!(step, "answer");
                assert!(detail.contains("phd2.restart_timeout (2m)"), "{detail}");
            }
            other => panic!("expected restart_failed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_phd2_that_has_run_60_s_without_accepting_is_ended_at_once() {
        let keeper = Arc::new(FakeKeeper::new(
            "active",
            Duration::from_secs(61),
            Some(NEW),
        ));
        let ops = ops_with(Some(keeper.clone()), managed());
        let _ = ops.restart_phd2().await.unwrap_err();
        let ended = keeper.ended();
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0], (OLD, Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn a_keeper_that_starts_no_new_phd2_fails_the_start_step() {
        // Ended at once, and nothing takes its place.
        let keeper = Arc::new(FakeKeeper::new("active", Duration::from_secs(61), None));
        let ops = ops_with(Some(keeper), managed());
        let err = ops.restart_phd2().await.unwrap_err();
        match err {
            ServiceError::RestartFailed { step, .. } => assert_eq!(step, "start"),
            other => panic!("expected restart_failed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_keeper_that_stops_during_the_restart_fails_the_start_step_at_once() {
        let keeper = Arc::new(FakeKeeper::new("active", Duration::from_secs(61), None));
        let ops = ops_with(Some(keeper.clone()), managed());
        // The operator stops the unit as PHD2 is ended.
        let stopper = {
            let keeper = Arc::clone(&keeper);
            tokio::spawn(async move {
                while keeper.ended().is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                keeper.state.lock().unwrap().active_state = "inactive";
            })
        };
        let begun = tokio::time::Instant::now();
        let err = ops.restart_phd2().await.unwrap_err();
        stopper.await.unwrap();
        match err {
            ServiceError::RestartFailed { step, detail } => {
                assert_eq!(step, "start");
                assert!(detail.contains("fake-phd2 is inactive"), "{detail}");
            }
            other => panic!("expected restart_failed, got {other:?}"),
        }
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "the start step waited out the bound for a unit that had stopped"
        );
    }

    /// Wait until `client` has PHD2's version, at most 5 s.
    async fn version_arrives(client: &Phd2Client) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while client.get_phd2_version().await.is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no Version event arrived"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // real sockets
    async fn the_answer_comes_only_on_a_connection_made_after_the_old_phd2_was_gone() {
        use tokio::io::AsyncWriteExt;

        const VERSION: &[u8] =
            b"{\"Event\":\"Version\",\"PHDVersion\":\"2.6.14\",\"PHDSubver\":\"\",\"MsgVersion\":1}\r\n";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (close_first, first_closed) = tokio::sync::oneshot::channel::<()>();
        // The old PHD2 says who it is, then goes when told; the new one says
        // who it is and stays.
        let server = tokio::spawn(async move {
            let (mut old, _) = listener.accept().await.unwrap();
            old.write_all(VERSION).await.unwrap();
            first_closed.await.unwrap();
            drop(old);
            let (mut new, _) = listener.accept().await.unwrap();
            new.write_all(VERSION).await.unwrap();
            new
        });
        let config = Phd2Config {
            host: "127.0.0.1".to_string(),
            port,
            reconnect: crate::config::ReconnectConfig {
                enabled: true,
                interval: Duration::from_millis(20),
                max_retries: None,
            },
            ..Phd2Config::default()
        };
        let client = Arc::new(Phd2Client::new(config));
        let ops = GuiderOps::new(
            Arc::clone(&client),
            SettleParams::default(),
            Duration::from_secs(10),
        );
        client.connect().await.unwrap();
        version_arrives(&client).await;
        let connections = client.connection_count();

        // Connected, and with a version, but on the old connection.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), ops.wait_answer(connections))
                .await
                .is_err(),
            "the old connection's Version was taken for the new PHD2's answer"
        );

        close_first.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), ops.wait_answer(connections))
            .await
            .expect("the new PHD2's answer was not seen");
        assert!(client.connection_count() > connections);
        drop(server.await.unwrap());
    }

    #[test]
    fn the_states_that_guide_are_refused() {
        for state in [
            AppState::Guiding,
            AppState::Calibrating,
            AppState::Paused,
            AppState::LostLock,
        ] {
            assert!(guides(state), "{state}");
        }
        for state in [AppState::Stopped, AppState::Selected, AppState::Looping] {
            assert!(!guides(state), "{state}");
        }
    }
}
