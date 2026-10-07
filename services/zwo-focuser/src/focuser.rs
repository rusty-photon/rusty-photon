//! `ZwoFocuser` — the ASCOM `Device` + `Focuser` implementation over the
//! [`FocuserHandle`](crate::backend::FocuserHandle) seam.
//!
//! Behaviour follows `docs/services/zwo-focuser.md`: `Absolute` is always
//! `true` and `move_` is absolute-only (no relative-move prior art exists
//! anywhere in this codebase); `TempComp`/`TempCompAvailable`/`SetTempComp`
//! stay stubbed, matching `qhy-focuser`/`pa-scops-oag`; `Temperature` returns
//! the live `EAFGetTemp` reading (the EAF has a sensor, unlike the Scops OAG).
//!
//! Every blocking SDK call runs on `spawn_blocking`, mirroring `zwo-camera`'s
//! `ZwoCamera` — the EAF SDK is blocking C FFI, so calling it directly from an
//! async handler could stall other Alpaca requests.

use std::sync::Arc;

use ascom_alpaca::api::{Device, Focuser};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use parking_lot::Mutex;
use tracing::debug;

use crate::backend::{BackendError, FocuserHandle, SessionState};
use crate::config::DeviceOverride;
use crate::config_actions::ZwoFocuserDriver;
use rusty_photon_driver::{connected_transition, ConfigActionCtx, ConnectedTransition};

/// What a failed SDK call answers a client: `NOT_CONNECTED` when the handle
/// judged the failure to mean the EAF has left the bus (C5), and the generic
/// `INVALID_OPERATION` with the SDK's message for any other failure. Decided
/// from the call's own error, never from the session's mark, which a
/// reconnect may have cleared by the time [`ZwoFocuser::on_handle`] looks.
fn sdk_err(e: BackendError) -> ASCOMError {
    if e.is_departed() {
        ASCOMError::NOT_CONNECTED
    } else {
        ASCOMError::invalid_operation(e.into_message())
    }
}

/// What a `Connected = requested` write has to do from `session`: the rule
/// zwo-camera and svbony-camera share (`rusty-photon-driver`), fed this
/// handle's session. A lost session (C5) always has something to do, its
/// release.
const fn transition_for(session: SessionState, requested: bool) -> ConnectedTransition {
    connected_transition(
        requested,
        !matches!(session, SessionState::Closed),
        matches!(session, SessionState::Lost),
    )
}

/// One ASCOM Focuser device per discovered EAF.
#[derive(Clone, derive_more::Debug)]
pub struct ZwoFocuser {
    #[debug(skip)]
    handle: Arc<dyn FocuserHandle>,
    /// The working travel limit (`EAFGetMaxStep`) — what `MaxStep`/
    /// `MaxIncrement` report and `Move` validates against. The firmware stops
    /// at this limit, so the `EAF_INFO::MaxStep` ceiling must not be used for
    /// range checks.
    max_step: u32,
    unique_id: String,
    name: String,
    description: String,
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<ZwoFocuserDriver>>,
    /// Held while a `Connected` write reads the session and acts on it, so
    /// connection changes run one at a time (C5). A release is not idempotent
    /// the way an open is: without this, two reconnects after a departure
    /// could each release and reopen.
    #[debug(skip)]
    lifecycle: Arc<Mutex<()>>,
}

impl ZwoFocuser {
    /// Build a device from an SDK handle and an optional per-serial config
    /// override. The ASCOM `UniqueID` is the handle's serial-derived id;
    /// `name`/`description` fall back to SDK-derived defaults.
    pub fn new(handle: Arc<dyn FocuserHandle>, overrides: Option<&DeviceOverride>) -> Self {
        let info = handle.info();
        let max_step = handle.max_step();
        let unique_id = handle.unique_id();
        let name = overrides
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| info.name.clone());
        let description = overrides
            .and_then(|o| o.description.clone())
            .unwrap_or_else(|| format!("ZWO EAF focuser ({})", info.name));
        Self {
            handle,
            max_step,
            unique_id,
            name,
            description,
            config_ctx: None,
            lifecycle: Arc::new(Mutex::new(())),
        }
    }

    /// Attach config-action wiring (enables `config.get`/`apply`/`schema`).
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<ZwoFocuserDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    fn ensure_connected(&self) -> ASCOMResult<()> {
        if self.is_connected() {
            Ok(())
        } else {
            Err(ASCOMError::NOT_CONNECTED)
        }
    }

    /// Whether this device holds a session on an EAF still on the bus: the
    /// handle is open, and no failure on it has meant the EAF left (C5).
    /// Answered from the handle's own state, never from the SDK.
    fn is_connected(&self) -> bool {
        self.handle.session() == SessionState::Live
    }

    /// Bring the device to `connected`: read the handle's session under
    /// [`Self::lifecycle`] and act on it there, so concurrent requests run one
    /// after another, each from what the last left behind.
    ///
    /// An EAF that has left the bus is held but not connected (C5): its
    /// handle is still open, and only a disconnect lets it go. Either way a
    /// client asks, that release comes first. `Connected = false` ends there,
    /// and `Connected = true` goes on to a fresh connect rather than taking
    /// the lost session back.
    fn transition(&self, connected: bool) -> ASCOMResult<()> {
        let lifecycle = self.lifecycle.lock();
        let outcome = match transition_for(self.handle.session(), connected) {
            ConnectedTransition::Nothing => Ok(()),
            ConnectedTransition::Connect => self.connect(),
            ConnectedTransition::Disconnect => self.disconnect(),
            ConnectedTransition::Release => {
                debug!(focuser = %self.unique_id, "releasing a session whose focuser left the bus");
                self.disconnect()
            }
            ConnectedTransition::ReleaseThenConnect => {
                debug!(focuser = %self.unique_id, "releasing a session whose focuser left the bus");
                self.disconnect().and_then(|()| self.connect())
            }
        };
        drop(lifecycle);
        outcome
    }

    fn connect(&self) -> ASCOMResult<()> {
        // A failed open leaves the handle closed (C2). There is no post-open
        // handshake: the name, travel limit and serial were cached at
        // enumeration.
        self.handle.open().map_err(|e| {
            debug!(focuser = %self.unique_id, error = %e, "focuser open failed");
            ASCOMError::NOT_CONNECTED
        })?;
        debug!(focuser = %self.unique_id, "focuser connected");
        Ok(())
    }

    fn disconnect(&self) -> ASCOMResult<()> {
        self.handle.close().map_err(|_| ASCOMError::NOT_CONNECTED)?;
        debug!(focuser = %self.unique_id, "focuser disconnected");
        Ok(())
    }

    /// Run a blocking SDK-seam call off the async executor. The EAF FFI calls
    /// do USB I/O, so running them directly on a Tokio worker could stall
    /// other Alpaca requests; offload them like the connect path.
    ///
    /// A failure on a device that is no longer connected answers
    /// `NOT_CONNECTED`: a disconnect that landed during the call, or a
    /// departure (C5), which the call site's own mapping also answers from the
    /// failure itself.
    async fn on_handle<T, F>(&self, f: F) -> ASCOMResult<T>
    where
        F: FnOnce(&dyn FocuserHandle) -> ASCOMResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = Arc::clone(&self.handle);
        let outcome = tokio::task::spawn_blocking(move || f(handle.as_ref()))
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("SDK task failed: {e}")))?;
        match outcome {
            Err(e) if !self.is_connected() => {
                debug!(
                    focuser = %self.unique_id,
                    error = %e,
                    "SDK call failed on a focuser that is closed or has left the bus"
                );
                Err(ASCOMError::NOT_CONNECTED)
            }
            outcome => outcome,
        }
    }
}

#[async_trait::async_trait]
impl Device for ZwoFocuser {
    fn static_name(&self) -> &str {
        &self.name
    }

    fn unique_id(&self) -> &str {
        &self.unique_id
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(self.is_connected())
    }

    async fn set_connected(&self, connected: bool) -> ASCOMResult<()> {
        // A lock-free fast path for the common no-op; `transition` decides
        // again under the lifecycle lock, since a request that held it first
        // may have released and reconnected since.
        if transition_for(self.handle.session(), connected) == ConnectedTransition::Nothing {
            return Ok(());
        }
        // `connect`/`disconnect` do blocking SDK I/O (`EAFGetNum`/`EAFOpen`/
        // `EAFClose`), so offload them off the executor (ZwoFocuser is cheap
        // to clone: it is `Arc`-backed).
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.transition(connected))
            .await
            .map_err(|e| ASCOMError::invalid_operation(format!("connect task failed: {e}")))?
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.description.clone())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok("rusty-photon zwo-focuser".to_string())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok(env!("CARGO_PKG_VERSION").to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<ZwoFocuserDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait::async_trait]
impl Focuser for ZwoFocuser {
    async fn absolute(&self) -> ASCOMResult<bool> {
        Ok(true)
    }

    async fn is_moving(&self) -> ASCOMResult<bool> {
        self.ensure_connected()?;
        self.on_handle(|h| h.is_moving().map_err(sdk_err)).await
    }

    async fn max_increment(&self) -> ASCOMResult<u32> {
        // Served from cache, but only while connected (M14, C5).
        self.ensure_connected()?;
        Ok(self.max_step)
    }

    async fn max_step(&self) -> ASCOMResult<u32> {
        self.ensure_connected()?;
        Ok(self.max_step)
    }

    async fn position(&self) -> ASCOMResult<i32> {
        self.ensure_connected()?;
        self.on_handle(|h| h.position().map_err(sdk_err)).await
    }

    async fn step_size(&self) -> ASCOMResult<f64> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn set_temp_comp(&self, _temp_comp: bool) -> ASCOMResult<()> {
        Err(ASCOMError::NOT_IMPLEMENTED)
    }

    async fn temp_comp_available(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn temperature(&self) -> ASCOMResult<f64> {
        self.ensure_connected()?;
        let temp = self.on_handle(|h| h.temperature().map_err(sdk_err)).await?;
        Ok(f64::from(temp))
    }

    async fn halt(&self) -> ASCOMResult<()> {
        self.ensure_connected()?;
        self.on_handle(|h| h.stop().map_err(sdk_err)).await
    }

    async fn move_(&self, position: i32) -> ASCOMResult<()> {
        self.ensure_connected()?;
        if position < 0 || u32::try_from(position).unwrap_or(u32::MAX) > self.max_step {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Position {} out of range [0, {}]", position, self.max_step),
            ));
        }
        self.on_handle(move |h| h.move_to(position).map_err(sdk_err))
            .await
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::backend::mock::MockFocuserHandle;
    use std::sync::atomic::Ordering;

    fn device(handle: MockFocuserHandle) -> ZwoFocuser {
        ZwoFocuser::new(Arc::new(handle), None)
    }

    #[tokio::test]
    async fn absolute_is_always_true() {
        assert!(device(MockFocuserHandle::default())
            .absolute()
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn max_step_and_max_increment_report_the_cached_info() {
        let d = device(MockFocuserHandle::default().with_max_step(1234));
        d.set_connected(true).await.unwrap();
        assert_eq!(d.max_step().await.unwrap(), 1234);
        assert_eq!(d.max_increment().await.unwrap(), 1234);
    }

    #[tokio::test]
    async fn max_step_and_max_increment_need_a_connection() {
        let d = device(MockFocuserHandle::default());
        assert_eq!(
            d.max_step().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(
            d.max_increment().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
    }

    // --- an EAF that leaves the bus (C5) ----------------------------------------

    /// A connected device over a mock whose knobs the test keeps.
    async fn connected() -> (ZwoFocuser, Arc<MockFocuserHandle>) {
        let handle = Arc::new(MockFocuserHandle::default());
        let d = ZwoFocuser::new(Arc::clone(&handle) as Arc<dyn FocuserHandle>, None);
        d.set_connected(true).await.unwrap();
        (d, handle)
    }

    #[tokio::test]
    async fn a_departed_focuser_reads_connected_until_a_call_reaches_it() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        assert!(d.connected().await.unwrap());
        assert_eq!(d.max_step().await.unwrap(), 60_000);
    }

    #[tokio::test]
    async fn the_call_that_finds_the_departure_answers_not_connected() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        assert_eq!(
            d.position().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert!(!d.connected().await.unwrap());
    }

    #[tokio::test]
    async fn once_lost_every_member_that_needs_a_session_answers_not_connected() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        d.temperature().await.unwrap_err();
        let not_connected = ASCOMErrorCode::NOT_CONNECTED;
        assert_eq!(d.position().await.unwrap_err().code, not_connected);
        assert_eq!(d.is_moving().await.unwrap_err().code, not_connected);
        assert_eq!(d.temperature().await.unwrap_err().code, not_connected);
        assert_eq!(d.max_step().await.unwrap_err().code, not_connected);
        assert_eq!(d.max_increment().await.unwrap_err().code, not_connected);
        assert_eq!(d.halt().await.unwrap_err().code, not_connected);
        assert_eq!(d.move_(100).await.unwrap_err().code, not_connected);
        // The members the driver fixes answer either way.
        assert!(d.absolute().await.unwrap());
        assert!(!d.temp_comp().await.unwrap());
    }

    #[tokio::test]
    async fn a_failure_on_a_focuser_still_there_keeps_its_code_and_the_session() {
        let (d, handle) = connected().await;
        handle.fail_temperature.store(true, Ordering::SeqCst);
        assert_eq!(
            d.temperature().await.unwrap_err().code,
            ASCOMErrorCode::INVALID_OPERATION
        );
        assert!(d.connected().await.unwrap());
    }

    #[tokio::test]
    async fn a_departure_answers_not_connected_even_when_the_mark_is_already_gone() {
        // A reconnect landing between the failing call and the device's look
        // at the session clears the mark; the answer comes from the failure.
        let (d, handle) = connected().await;
        handle.answer_one_departure_unmarked();
        assert_eq!(
            d.temperature().await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert!(d.connected().await.unwrap(), "the session itself is live");
    }

    #[tokio::test]
    async fn disconnecting_a_lost_session_releases_it() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        d.position().await.unwrap_err();
        d.set_connected(false).await.unwrap();
        assert_eq!(handle.session(), SessionState::Closed);
        assert!(!d.connected().await.unwrap());
    }

    #[tokio::test]
    async fn reconnecting_while_the_focuser_is_gone_fails_and_leaves_it_closed() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        d.position().await.unwrap_err();
        assert_eq!(
            d.set_connected(true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_CONNECTED
        );
        assert_eq!(handle.session(), SessionState::Closed);
    }

    #[tokio::test]
    async fn reconnecting_a_lost_session_releases_it_and_opens_afresh() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        d.position().await.unwrap_err();
        handle.return_to_bus();
        d.set_connected(true).await.unwrap();
        assert_eq!(handle.opens(), 2, "a fresh session, not the lost one back");
        assert!(d.connected().await.unwrap());
        d.position().await.unwrap();
    }

    #[tokio::test]
    async fn two_reconnects_after_a_departure_release_once_and_open_one_fresh_session() {
        let (d, handle) = connected().await;
        handle.leave_bus();
        d.position().await.unwrap_err();
        handle.return_to_bus();

        // Hold the first reconnect inside its release, so the second arrives
        // while the session still reads lost.
        handle.hold_closes();
        let first = tokio::spawn({
            let d = d.clone();
            async move { d.set_connected(true).await }
        });
        for _ in 0..5_000 {
            if handle.closes() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(
            handle.closes(),
            1,
            "the first reconnect never began its release"
        );
        let second = tokio::spawn({
            let d = d.clone();
            async move { d.set_connected(true).await }
        });
        // Without the lifecycle lock the second would read the lost session
        // and begin a release of its own; give it the chance to.
        for _ in 0..100 {
            if handle.closes() > 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        handle.release_closes();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(
            handle.closes(),
            1,
            "the second found the first's session live"
        );
        assert_eq!(handle.opens(), 2, "one fresh session between them");
        assert!(d.connected().await.unwrap());
    }

    #[tokio::test]
    async fn operations_while_disconnected_are_rejected() {
        let d = device(MockFocuserHandle::default());
        assert_eq!(
            d.position().await.unwrap_err().code,
            ASCOMError::NOT_CONNECTED.code
        );
        assert_eq!(
            d.is_moving().await.unwrap_err().code,
            ASCOMError::NOT_CONNECTED.code
        );
        assert_eq!(
            d.halt().await.unwrap_err().code,
            ASCOMError::NOT_CONNECTED.code
        );
        assert_eq!(
            d.move_(0).await.unwrap_err().code,
            ASCOMError::NOT_CONNECTED.code
        );
    }

    #[tokio::test]
    async fn connect_move_and_position_round_trip() {
        let d = device(MockFocuserHandle::default());
        d.set_connected(true).await.unwrap();
        assert!(d.connected().await.unwrap());
        d.move_(500).await.unwrap();
        assert!(d.is_moving().await.unwrap());
        assert!(!d.is_moving().await.unwrap());
        assert_eq!(d.position().await.unwrap(), 500);
        d.set_connected(false).await.unwrap();
        assert!(!d.connected().await.unwrap());
    }

    #[tokio::test]
    async fn move_out_of_range_is_rejected_without_calling_the_sdk() {
        let d = device(MockFocuserHandle::default().with_max_step(1000));
        d.set_connected(true).await.unwrap();
        assert_eq!(
            d.move_(-1).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        assert_eq!(
            d.move_(1001).await.unwrap_err().code,
            ASCOMErrorCode::INVALID_VALUE
        );
        // No move was actually issued to the handle.
        assert_eq!(d.position().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn halt_stops_an_in_progress_move() {
        let d = device(MockFocuserHandle::default());
        d.set_connected(true).await.unwrap();
        d.move_(200).await.unwrap();
        d.halt().await.unwrap();
        assert!(!d.is_moving().await.unwrap());
    }

    #[tokio::test]
    async fn temperature_returns_the_live_reading() {
        let d = device(MockFocuserHandle::default());
        d.set_connected(true).await.unwrap();
        assert_eq!(d.temperature().await.unwrap(), 20.0);
    }

    #[tokio::test]
    async fn temperature_failure_is_surfaced() {
        let handle = MockFocuserHandle::default();
        handle.fail_temperature.store(true, Ordering::SeqCst);
        let d = device(handle);
        d.set_connected(true).await.unwrap();
        assert!(d.temperature().await.is_err());
    }

    #[tokio::test]
    async fn temp_comp_and_step_size_are_stubbed() {
        let d = device(MockFocuserHandle::default());
        assert!(!d.temp_comp().await.unwrap());
        assert!(!d.temp_comp_available().await.unwrap());
        assert_eq!(
            d.set_temp_comp(true).await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            d.step_size().await.unwrap_err().code,
            ASCOMErrorCode::NOT_IMPLEMENTED
        );
    }

    #[tokio::test]
    async fn name_and_description_use_config_overrides() {
        let overrides = DeviceOverride {
            name: Some("Main Focuser".to_string()),
            description: Some("On the Askar 60F".to_string()),
        };
        let d = ZwoFocuser::new(Arc::new(MockFocuserHandle::default()), Some(&overrides));
        assert_eq!(d.static_name(), "Main Focuser");
        assert_eq!(d.description().await.unwrap(), "On the Askar 60F");
    }
}
