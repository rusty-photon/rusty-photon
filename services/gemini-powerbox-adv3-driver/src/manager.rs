//! Thin manager wrapping `SharedTransport<GeminiCodec>` plus the cached
//! protocol state both ASCOM devices read from.
//!
//! The refcount, slot, open/close transitions, command-lock arbitration,
//! and poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The handshake: `>H#` (with its one retry) → `>V#` → `>G#`, seed the
//!   cache.
//! * The poll loop body: discard the input, `>G#`, store the frame.
//! * The cached state both devices share (the last frame, the firmware,
//!   the three sensor sliding-window means).
//!
//! Both the handshake and the poll loop are **read-only by construction**:
//! the only [`GeminiCommand`] variants either one sends are the three
//! queries. That is what tenet 3 (no actuation on connect) demands of a box
//! where almost every write is a power or heater change — the handshake
//! re-runs on every reconnect after a serial glitch, so a set command placed
//! here would energise a rail behind the operator's back.
//!
//! The hub streams a frame every 3 s once asked. The poll does not consume
//! that stream; it discards it and asks for a fresh frame
//! ([`GeminiCodec::discard_before_send`]), so a frame that sat in the buffer
//! never answers a later request.

use std::sync::Arc;
use std::time::Duration;

use rusty_photon_rolling_stats::observing_conditions::{average_period_hours, effective_window};
use rusty_photon_rolling_stats::SensorMean;
use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SessionError, SharedTransport, StateAssertion, TransportError,
    TransportFactory, WhileOpen,
};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::debug;

use crate::codec::{GeminiCodec, GeminiCodecError, GeminiResponse};
use crate::config::Config;
use crate::error::{GeminiError, Result};
use crate::protocol::{
    check_firmware, check_identity, FirmwareVersion, GeminiCommand, SetCommand, StatusFrame,
    MIN_FIRMWARE, RECOMMENDED_FIRMWARE,
};

/// How long the handshake waits before it sends `>H#` a second time when
/// the first draws no `*H` reply: the vendor driver's flat post-open sleep,
/// spent only when a unit needs it.
pub const IDENTITY_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Cached device state shared between the switch device and the
/// observing-conditions device.
///
/// The frame stays `None` until the handshake seeds it, so a read that
/// arrives before the first successful poll can be answered `NOT_CONNECTED`
/// rather than with a fabricated zero.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    /// The last `*G` frame.
    pub status: Option<StatusFrame>,
    /// The firmware `>V#` reported, kept for `DriverInfo`.
    pub firmware: Option<u16>,
    pub temp_mean: SensorMean,
    pub humidity_mean: SensorMean,
    pub dewpoint_mean: SensorMean,
    /// `AveragePeriod` in hours, exactly as the client last set it.
    ///
    /// Stored rather than derived from the sensor window because the two are
    /// not the same number: 0 hours means "do not average", which this driver
    /// serves with a short window rather than no window at all (see
    /// [`effective_window`]). Reading the period back off the window would
    /// report that window's length, so a client that set 0 would be told
    /// something else — and a client that set exactly the instantaneous
    /// window's length would be told 0.
    pub average_period_hours: f64,
}

/// Manager that wraps the shared transport plus the hub's cached state.
/// One instance per process; both devices hold `Arc<GeminiManager>`.
pub struct GeminiManager {
    transport: Arc<SharedTransport<GeminiCodec>>,
    cached_state: Arc<RwLock<CachedState>>,
    /// Kept so [`GeminiManager::set_averaging_period`] can size the
    /// instantaneous window against the poll cadence.
    poll_interval: Duration,
}

impl GeminiManager {
    /// Build the manager: seed the sensor windows from config and install
    /// the handshake and poll-loop hooks on a fresh shared transport.
    #[must_use]
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        // Seed sensor windows from config, through the same mapping a
        // client's SetAveragePeriod takes, so a configured 0 behaves exactly
        // like one set over the wire.
        let poll_interval = config.serial.polling_interval;
        let mut state = CachedState::default();
        let period_hours = config.observingconditions.averaging_period.as_secs_f64() / 3600.0;
        let window = effective_window(period_hours, poll_interval);
        state.temp_mean.set_window(window);
        state.humidity_mean.set_window(window);
        state.dewpoint_mean.set_window(window);
        state.average_period_hours = period_hours;
        let cached_state = Arc::new(RwLock::new(state));

        let port: Arc<str> = Arc::from(config.serial.port.as_str());
        let hooks = build_hooks(&cached_state, poll_interval, port);
        let transport = SharedTransport::new(factory, GeminiCodec, hooks);

        Arc::new(Self {
            transport,
            cached_state,
            poll_interval,
        })
    }

    /// Access the shared transport so devices can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<GeminiCodec>> {
        &self.transport
    }

    /// Cheap, non-blocking snapshot — true between handshake completion
    /// and the start of teardown.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.transport.is_available()
    }

    /// Clone the current cached state for read-only consumers.
    pub async fn get_cached_state(&self) -> CachedState {
        self.cached_state.read().await.clone()
    }

    /// Reconfigure the sliding-window length on all three sensor means.
    ///
    /// Takes the client's `AveragePeriod` in hours rather than a window so
    /// the requested value can be recorded verbatim for read-back; the window
    /// it maps to comes from [`effective_window`].
    ///
    /// A period outside [`average_period_hours`] — NaN included — is ignored
    /// entirely: windows and recorded period both keep their previous values.
    pub async fn set_averaging_period(&self, period_hours: f64) {
        if !average_period_hours().contains(&period_hours) {
            debug!(
                period_hours,
                "averaging period outside the supported range; state left unchanged"
            );
            return;
        }
        let window = effective_window(period_hours, self.poll_interval);
        let mut state = self.cached_state.write().await;
        state.temp_mean.set_window(window);
        state.humidity_mean.set_window(window);
        state.dewpoint_mean.set_window(window);
        state.average_period_hours = period_hours;
        drop(state);
        debug!(period_hours, ?window, "sensor averaging period updated");
    }

    /// Ask the hub for a fresh frame — the input discarded first, then
    /// `>G#` — store it, and return it.
    ///
    /// Devices use this on demand: the second look a write takes when its
    /// readback does not show it, the `can_write` of a dew row while the
    /// cache is still empty, `ObservingConditions.Refresh`.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as a [`GeminiError`] — a
    /// transport error, a frame the codec cannot parse, or an exhausted skip
    /// budget. The cache is left untouched on failure.
    pub async fn refresh_status(&self, session: &Session<GeminiCodec>) -> Result<StatusFrame> {
        let resp = session
            .request(GeminiCommand::Status)
            .await
            .map_err(GeminiError::from)?;
        let GeminiResponse::Status(frame) = resp else {
            return Err(GeminiError::InvalidResponse(
                ">G# was answered by something other than a status frame".to_string(),
            ));
        };
        self.store_status(&frame).await;
        Ok(frame)
    }

    /// Send a set command — the input discarded first, then the set and a
    /// `>G#` in one buffer — and return the frame the hub produced after
    /// acting on it. The frame is **not** stored: the caller compares the
    /// field the write addressed first, and stores what it believes.
    ///
    /// # Errors
    ///
    /// The same failure set as [`refresh_status`](Self::refresh_status).
    pub async fn send_set(
        &self,
        session: &Session<GeminiCodec>,
        set: SetCommand,
    ) -> Result<StatusFrame> {
        let resp = session
            .request(GeminiCommand::Set(set))
            .await
            .map_err(GeminiError::from)?;
        match resp {
            GeminiResponse::Status(frame) => Ok(frame),
            _ => Err(GeminiError::InvalidResponse(
                "a set was answered by something other than a status frame".to_string(),
            )),
        }
    }

    /// Store `frame` as the hub's current state and feed the sensor windows.
    pub async fn store_status(&self, frame: &StatusFrame) {
        let mut state = self.cached_state.write().await;
        apply_status(&mut state, frame);
    }
}

fn build_hooks(
    cached_state: &Arc<RwLock<CachedState>>,
    poll_interval: Duration,
    port: Arc<str>,
) -> Hooks<GeminiCodec> {
    let cs_handshake = Arc::clone(cached_state);
    let cs_poll = Arc::clone(cached_state);
    Hooks {
        handshake: Box::new(move |conn| {
            let cs = Arc::clone(&cs_handshake);
            let port = Arc::clone(&port);
            Box::pin(handshake(conn, cs, port))
        }),
        on_last_disconnect: Box::new(|_| {
            // Nothing to assert: this service holds no state that must
            // survive a no-client gap, so the answer is trivially yes.
            Box::pin(async { StateAssertion::Asserted })
        }),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(move |ctx| {
            let cs = Arc::clone(&cs_poll);
            Box::pin(poll_loop(ctx, cs, poll_interval))
        })),
    }
}

/// Whether a failed `>H#` is one the retry covers: no `*H` reply inside the
/// timeout, or only frames without the prefix. A port that errored or a
/// frame that will not decode is not.
const fn is_no_identity_reply(err: &SessionError<GeminiCodecError>) -> bool {
    matches!(
        err,
        SessionError::Transport(TransportError::Timeout(_)) | SessionError::SkipExhausted(_)
    )
}

/// `>H#`, and once more after [`IDENTITY_RETRY_DELAY`] if the first draws no
/// identity reply — the vendor driver's flat 2 s post-open sleep, spent only
/// when needed. The dev box's unit answers at once; the retry covers a unit
/// that does reset on the DTR edge.
async fn request_identity(
    conn: &Connection<GeminiCodec>,
    port: &str,
) -> std::result::Result<String, GeminiCodecError> {
    let first = conn.request(GeminiCommand::Identity).await;
    let reply = match first {
        Err(e) if is_no_identity_reply(&e) => {
            debug!(
                error = %e,
                delay = ?IDENTITY_RETRY_DELAY,
                "no identity reply; waiting before asking once more"
            );
            tokio::time::sleep(IDENTITY_RETRY_DELAY).await;
            match conn.request(GeminiCommand::Identity).await {
                Err(e) if is_no_identity_reply(&e) => {
                    return Err(GeminiCodecError::Handshake(GeminiError::NoIdentity {
                        port: port.to_string(),
                    }));
                }
                other => other?,
            }
        }
        other => other?,
    };
    match reply {
        GeminiResponse::Identity(identity) => Ok(identity),
        // `matches` only lets an identity frame answer `>H#`.
        _ => Err(GeminiCodecError::Handshake(GeminiError::NoIdentity {
            port: port.to_string(),
        })),
    }
}

/// Read-only connect sequence: identify the box, gate the firmware, and
/// seed the cache from one frame.
///
/// Runs on every transition into `Open`, reconnects included. Nothing here
/// may write device state.
async fn handshake(
    conn: &Connection<GeminiCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    port: Arc<str>,
) -> std::result::Result<(), GeminiCodecError> {
    let identity = request_identity(conn, &port).await?;
    check_identity(&identity).map_err(GeminiCodecError::Handshake)?;

    let firmware = match conn.request(GeminiCommand::Firmware).await? {
        GeminiResponse::Firmware(firmware) => firmware,
        other => {
            return Err(GeminiCodecError::Parse(format!(
                ">V# was answered by {other:?}"
            )))
        }
    };
    check_firmware(firmware).map_err(GeminiCodecError::Handshake)?;
    // Not a warning: the vendor publishes no image the operator could
    // move to (checked 2026-10-26), so there is nothing to act on; the
    // caveat is in the design doc and the version in `DriverInfo`.
    if firmware == MIN_FIRMWARE {
        debug!(
            firmware = %FirmwareVersion(firmware),
            recommended = %FirmwareVersion(RECOMMENDED_FIRMWARE),
            "the hub runs the oldest accepted firmware; the recommended one fixes DS18B20 \
             probe detection"
        );
    } else {
        debug!(firmware = %FirmwareVersion(firmware), "hub firmware");
    }

    let frame = match conn.request(GeminiCommand::Status).await? {
        GeminiResponse::Status(frame) => frame,
        other => {
            return Err(GeminiCodecError::Parse(format!(
                ">G# was answered by {other:?}"
            )))
        }
    };

    let mut state = cached_state.write().await;
    state.firmware = Some(firmware);
    apply_status(&mut state, &frame);
    drop(state);
    debug!("Gemini hub handshake complete");
    Ok(())
}

async fn poll_loop(
    ctx: WhileOpen<GeminiCodec>,
    cached_state: Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) {
    let mut ticker = interval(poll_interval);
    // Skip the immediate first tick — the handshake just populated the
    // cache; first poll should wait one interval.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("Gemini hub poll loop received cancellation");
                return;
            }
        }

        // The codec discards the input before `>G#` goes out, so the frame
        // read here is the one the hub produced for this request, not one it
        // streamed earlier.
        match ctx.request(GeminiCommand::Status).await {
            Ok(GeminiResponse::Status(frame)) => {
                let mut state = cached_state.write().await;
                apply_status(&mut state, &frame);
            }
            Ok(other) => debug!(?other, "poll: >G# answered by an unexpected frame"),
            // A frame that will not parse leaves the cache untouched; a
            // transport error has already woken the reconnect supervisor.
            Err(e) => debug!(error = %e, "poll request failed; cache left as it was"),
        }
    }
}

/// Store `frame` and feed the sensor windows — only while the AHT20 says it
/// is attached, and only from a field that carries a reading. The first
/// frame whose flag reads `0` clears the three windows, so the readings go
/// `VALUE_NOT_SET` at once rather than serving a mean of samples taken
/// before the sensor went away; a field with no reading while the flag is
/// `1` adds no sample and clears nothing, and the window ages out on its
/// own if that persists.
fn apply_status(state: &mut CachedState, frame: &StatusFrame) {
    state.status = Some(frame.clone());
    if frame.ambient_sensor_attached {
        for (mean, reading) in [
            (&mut state.temp_mean, frame.ambient_temperature),
            (&mut state.humidity_mean, frame.humidity),
            (&mut state.dewpoint_mean, frame.dew_point),
        ] {
            if let Some(value) = reading {
                mean.add_sample(value);
            }
        }
    } else {
        for mean in [
            &mut state.temp_mean,
            &mut state.humidity_mean,
            &mut state.dewpoint_mean,
        ] {
            *mean = SensorMean::new(mean.window());
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock
    //! transport factory. Race / refcount / rollback invariants are
    //! tested once for everyone in `rusty-photon-shared-transport` —
    //! they don't get re-tested here.

    use super::*;
    use crate::mock::MockGeminiTransportFactory;
    use crate::protocol::{DcOutput, DewChannel, DewMode};
    use std::time::Instant;

    /// How long a test waits for the poll loop to write through to the
    /// cache before declaring the loop dead. Generous relative to the
    /// millisecond-scale poll intervals the tests configure — a failure
    /// deadline, not an expected duration.
    const CACHE_DEADLINE: Duration = Duration::from_secs(10);

    fn make_manager() -> Arc<GeminiManager> {
        make_manager_with(Arc::new(MockGeminiTransportFactory::default()))
    }

    fn make_manager_with(factory: Arc<dyn TransportFactory>) -> Arc<GeminiManager> {
        GeminiManager::new(&Config::default(), factory)
    }

    /// The mock as the trait object the manager takes.
    fn dyn_factory(factory: &Arc<MockGeminiTransportFactory>) -> Arc<dyn TransportFactory> {
        let concrete: Arc<MockGeminiTransportFactory> = Arc::clone(factory);
        concrete
    }

    fn make_manager_polling_every(
        poll_interval: Duration,
        factory: Arc<dyn TransportFactory>,
    ) -> Arc<GeminiManager> {
        let mut config = Config::default();
        config.serial.polling_interval = poll_interval;
        GeminiManager::new(&config, factory)
    }

    /// Poll the cache until `done` accepts it, or fail once `CACHE_DEADLINE`
    /// passes. Watches a window rather than sampling after a fixed nap, so a
    /// slow CI box cannot turn passing behaviour into a red test.
    async fn wait_for_cache(
        manager: &GeminiManager,
        what: &str,
        mut done: impl FnMut(&CachedState) -> bool,
    ) -> CachedState {
        let deadline = Instant::now() + CACHE_DEADLINE;
        loop {
            let state = manager.get_cached_state().await;
            if done(&state) {
                return state;
            }
            assert!(
                Instant::now() < deadline,
                "cache never reflected {what} within {CACHE_DEADLINE:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // ---- handshake -------------------------------------------------------

    #[tokio::test]
    async fn acquire_runs_the_handshake_and_seeds_the_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());

        let state = manager.get_cached_state().await;
        let frame = state.status.expect("frame seeded by handshake");
        assert_eq!(frame.ambient_temperature, Some(24.47));
        assert_eq!(state.firmware, Some(308));
        assert_eq!(state.temp_mean.sample_count(), 1);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_handshake_sends_only_the_three_queries() {
        // Tenet 3: nothing on the connect path may actuate. The mock keeps
        // every command it was sent.
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let manager = make_manager_with(dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        assert_eq!(factory.sent_commands().await, [">H#", ">V#", ">G#"]);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_flat_panel_is_refused_by_name_and_nothing_else_is_sent() {
        let factory =
            Arc::new(MockGeminiTransportFactory::default().with_identity("GeminiFlatPanelV3"));
        let manager = make_manager_with(dyn_factory(&factory));

        let err = manager.transport().acquire().await.unwrap_err();
        let message = GeminiError::from(err).to_string();
        assert!(message.contains("flat panel"), "{message}");
        assert!(message.contains("GeminiFlatPanelV3"), "{message}");
        assert!(!manager.is_available());
        assert_eq!(factory.sent_commands().await, [">H#"]);
        assert!(manager.get_cached_state().await.status.is_none());
    }

    #[tokio::test]
    async fn another_gemini_power_box_is_refused_as_not_the_advanced_3() {
        let factory =
            Arc::new(MockGeminiTransportFactory::default().with_identity("GeminiPowerBoxMiniV2"));
        let manager = make_manager_with(factory);
        let err = GeminiError::from(manager.transport().acquire().await.unwrap_err());
        assert!(matches!(err, GeminiError::WrongModel(_)), "{err:?}");
        assert!(err.to_string().contains("not the Advanced 3"), "{err}");
    }

    #[tokio::test]
    async fn firmware_307_is_refused_naming_the_version_and_the_upgrade_tool() {
        let factory = Arc::new(MockGeminiTransportFactory::default().with_firmware("307"));
        let manager = make_manager_with(dyn_factory(&factory));
        let err = GeminiError::from(manager.transport().acquire().await.unwrap_err());
        assert!(matches!(err, GeminiError::FirmwareTooOld { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("3.0.7"), "{text}");
        assert!(text.contains("FirmwareUpgradeTool"), "{text}");
        // The gate ran before the frame was asked for.
        assert_eq!(factory.sent_commands().await, [">H#", ">V#"]);
    }

    #[tokio::test]
    async fn firmware_309_connects_and_is_kept_for_driver_info() {
        let factory = Arc::new(MockGeminiTransportFactory::default().with_firmware("309"));
        let manager = make_manager_with(factory);
        let session = manager.transport().acquire().await.unwrap();
        assert_eq!(manager.get_cached_state().await.firmware, Some(309));
        session.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_ignored_first_identity_request_is_asked_once_more_after_two_seconds() {
        let factory = Arc::new(MockGeminiTransportFactory::default().with_ignored_first_identity());
        let manager = make_manager_with(dyn_factory(&factory));
        let started = tokio::time::Instant::now();

        let session = manager.transport().acquire().await.unwrap();

        assert_eq!(
            factory.sent_commands().await,
            [">H#", ">H#", ">V#", ">G#"],
            "one retry, then the rest of the handshake"
        );
        assert!(
            started.elapsed() >= IDENTITY_RETRY_DELAY,
            "the retry waits the vendor's 2 s: elapsed {:?}",
            started.elapsed()
        );
        session.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_unit_that_never_identifies_fails_the_connect_naming_the_port() {
        let factory = Arc::new(MockGeminiTransportFactory::default().with_ignored_identity());
        let mut config = Config::default();
        config.serial.port = "/dev/ttyUSB7".to_string();
        let manager = GeminiManager::new(&config, dyn_factory(&factory));

        let err = GeminiError::from(manager.transport().acquire().await.unwrap_err());

        assert!(matches!(err, GeminiError::NoIdentity { .. }), "{err:?}");
        assert!(err.to_string().contains("/dev/ttyUSB7"), "{err}");
        assert_eq!(factory.sent_commands().await, [">H#", ">H#"]);
        assert!(!manager.is_available());
    }

    #[tokio::test]
    async fn a_hub_still_streaming_from_an_earlier_session_connects_past_the_frame_it_cuts_short() {
        // The dev box's unit at the first connect of the implementation: left
        // armed by the design session, it cut its periodic frame short to
        // answer `>H#`, and the reply arrived glued to the cut frame.
        let factory = Arc::new(
            MockGeminiTransportFactory::default()
                .with_stream_running_at_start()
                .with_cut_frames_before_reply(),
        );
        let manager = make_manager_polling_every(Duration::from_millis(20), dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();
        assert_eq!(manager.get_cached_state().await.firmware, Some(308));
        factory.set_input_voltage(12.9).await;
        wait_for_cache(&manager, "a poll past a cut frame", |state| {
            state
                .status
                .as_ref()
                .is_some_and(|f| (f.input_voltage - 12.9).abs() < f64::EPSILON)
        })
        .await;
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_malformed_first_frame_fails_the_connect() {
        let factory = Arc::new(MockGeminiTransportFactory::default().with_input_voltage(f64::NAN));
        let manager = make_manager_with(factory);
        let err = GeminiError::from(manager.transport().acquire().await.unwrap_err());
        assert!(matches!(err, GeminiError::ParseError(_)), "{err:?}");
        assert!(!manager.is_available());
    }

    // ---- poll loop -------------------------------------------------------

    #[tokio::test]
    async fn the_poll_loop_refreshes_the_frame_into_the_cache() {
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let manager = make_manager_polling_every(Duration::from_millis(20), dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        // Change the device behind the cache's back: only the poll loop can
        // make this show up.
        factory.set_input_voltage(13.1).await;

        let state = wait_for_cache(&manager, "the new input voltage", |state| {
            state
                .status
                .as_ref()
                .is_some_and(|frame| (frame.input_voltage - 13.1).abs() < f64::EPSILON)
        })
        .await;
        assert!(
            state.temp_mean.sample_count() >= 2,
            "the poll fed the window"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_poll_loop_discards_the_stream_before_it_asks() {
        // With the knob on, the mock queues two already-streamed frames
        // behind every reply. A poll that did not discard them would read
        // one as its answer and never see the device change.
        let factory =
            Arc::new(MockGeminiTransportFactory::default().with_stale_frames_before_reply());
        let manager = make_manager_polling_every(Duration::from_millis(20), dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        factory.set_input_voltage(13.4).await;

        wait_for_cache(
            &manager,
            "the input voltage past the stale frames",
            |state| {
                state
                    .status
                    .as_ref()
                    .is_some_and(|frame| (frame.input_voltage - 13.4).abs() < f64::EPSILON)
            },
        )
        .await;

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn an_unplugged_ambient_sensor_empties_the_windows_at_once() {
        // The mid-session unplug the design doc puts on the poll loop: a
        // full window, then a frame with the AHT20 flag at 0, then no mean.
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let manager = make_manager_polling_every(Duration::from_millis(20), dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();
        wait_for_cache(&manager, "a few samples", |state| {
            state.temp_mean.sample_count() >= 3
        })
        .await;

        factory.set_sensors(false, true).await;

        let state = wait_for_cache(&manager, "the AHT20 reading absent", |state| {
            state
                .status
                .as_ref()
                .is_some_and(|frame| !frame.ambient_sensor_attached)
        })
        .await;
        assert_eq!(
            state.temp_mean.get_mean(),
            None,
            "temperature window cleared"
        );
        assert_eq!(
            state.humidity_mean.get_mean(),
            None,
            "humidity window cleared"
        );
        assert_eq!(
            state.dewpoint_mean.get_mean(),
            None,
            "dewpoint window cleared"
        );
        assert!(
            state.status.as_ref().is_some_and(|f| f.lens_probe_attached),
            "the probe's flag is untouched"
        );

        // Plugging it back in starts fresh windows.
        factory.set_sensors(true, true).await;
        let state = wait_for_cache(&manager, "the AHT20 back", |state| {
            state.temp_mean.get_mean().is_some()
        })
        .await;
        assert!(state.temp_mean.sample_count() >= 1);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_poll_loop_stops_when_the_session_closes() {
        let manager = make_manager_polling_every(
            Duration::from_millis(20),
            Arc::new(MockGeminiTransportFactory::default()),
        );
        let session = manager.transport().acquire().await.unwrap();
        session.close().await.unwrap();
        assert!(!manager.is_available());
    }

    // ---- refresh and set -------------------------------------------------

    #[tokio::test]
    async fn refresh_status_stores_and_returns_the_fresh_frame() {
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let manager = make_manager_with(dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        factory.set_input_voltage(11.9).await;
        let frame = manager.refresh_status(&session).await.unwrap();
        assert_eq!(frame.input_voltage, 11.9);
        assert_eq!(
            manager
                .get_cached_state()
                .await
                .status
                .unwrap()
                .input_voltage,
            11.9
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_set_returns_the_post_write_frame_without_storing_it() {
        let factory = Arc::new(MockGeminiTransportFactory::default());
        let manager = make_manager_with(dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        let frame = manager
            .send_set(&session, SetCommand::Dc(DcOutput::Dc3, false))
            .await
            .unwrap();
        assert!(!frame.dc_on(DcOutput::Dc3), "the readback shows the write");
        assert!(
            manager
                .get_cached_state()
                .await
                .status
                .unwrap()
                .dc_on(DcOutput::Dc3),
            "the cache is the caller's to update"
        );
        assert_eq!(
            factory.sent_buffers().await.last().map(String::as_str),
            Some(">C3#\n>G#"),
            "the set and the status request travel in one buffer"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_set_reads_the_post_write_frame_past_stale_streamed_frames() {
        let factory =
            Arc::new(MockGeminiTransportFactory::default().with_stale_frames_before_reply());
        let manager = make_manager_with(dyn_factory(&factory));
        let session = manager.transport().acquire().await.unwrap();

        let frame = manager
            .send_set(
                &session,
                SetCommand::DewMode(DewChannel::Dew6, DewMode::Switch),
            )
            .await
            .unwrap();
        assert_eq!(frame.dew(DewChannel::Dew6).mode, DewMode::Switch);

        session.close().await.unwrap();
    }

    // ---- averaging -------------------------------------------------------

    #[tokio::test]
    async fn set_averaging_period_leaves_state_unchanged_for_an_unusable_period() {
        let manager = make_manager();
        let one_hour = 1.0;
        manager.set_averaging_period(one_hour).await;
        let before = manager.get_cached_state().await;

        for rejected in [f64::NAN, f64::INFINITY, -1.0, 25.0] {
            manager.set_averaging_period(rejected).await;
            let after = manager.get_cached_state().await;
            assert_eq!(
                after.temp_mean.window(),
                before.temp_mean.window(),
                "window moved for {rejected}"
            );
            assert!(
                (after.average_period_hours - one_hour).abs() < f64::EPSILON,
                "recorded period moved to {} for {rejected}",
                after.average_period_hours
            );
        }
    }

    #[tokio::test]
    async fn set_averaging_period_resizes_all_three_means() {
        let manager = make_manager();
        manager.set_averaging_period(2.0 / 60.0).await;
        let state = manager.get_cached_state().await;
        let new_window = Duration::from_mins(2);
        assert_eq!(state.temp_mean.window(), new_window);
        assert_eq!(state.humidity_mean.window(), new_window);
        assert_eq!(state.dewpoint_mean.window(), new_window);
    }

    #[tokio::test]
    async fn zero_average_period_windows_three_poll_intervals_with_a_ten_second_floor() {
        let manager = make_manager_polling_every(
            Duration::from_secs(60),
            Arc::new(MockGeminiTransportFactory::default()),
        );
        manager.set_averaging_period(0.0).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), Duration::from_secs(180));
        assert!((state.average_period_hours - 0.0).abs() < f64::EPSILON);

        let fast = make_manager_polling_every(
            Duration::from_secs(1),
            Arc::new(MockGeminiTransportFactory::default()),
        );
        fast.set_averaging_period(0.0).await;
        assert_eq!(
            fast.get_cached_state().await.temp_mean.window(),
            Duration::from_secs(10)
        );
    }

    #[tokio::test]
    async fn average_period_is_recorded_verbatim_not_inferred_from_the_window() {
        let manager = make_manager_polling_every(
            Duration::from_secs(60),
            Arc::new(MockGeminiTransportFactory::default()),
        );
        let three_minutes_in_hours = 180.0 / 3600.0;
        manager.set_averaging_period(three_minutes_in_hours).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), Duration::from_secs(180));
        assert!((state.average_period_hours - three_minutes_in_hours).abs() < f64::EPSILON);
    }

    // ---- wire failures ---------------------------------------------------

    use async_trait::async_trait;
    use rusty_photon_shared_transport::FrameTransport;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Wraps the canonical mock factory but gates `send_frame` behind a
    /// shared atomic — flipping it makes the very next send return EOF.
    #[derive(Default, Clone)]
    struct InjectableFactory {
        inner: MockGeminiTransportFactory,
        fail_next_send: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TransportFactory for InjectableFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            let inner = self.inner.open().await?;
            Ok(Box::new(InjectableTransport {
                inner,
                fail_next_send: Arc::clone(&self.fail_next_send),
            }))
        }
    }

    struct InjectableTransport {
        inner: Box<dyn FrameTransport>,
        fail_next_send: Arc<AtomicBool>,
    }

    #[async_trait]
    impl FrameTransport for InjectableTransport {
        async fn send_frame(&mut self, bytes: &[u8]) -> std::result::Result<(), TransportError> {
            if self.fail_next_send.swap(false, Ordering::SeqCst) {
                return Err(TransportError::Eof);
            }
            self.inner.send_frame(bytes).await
        }

        async fn recv_frame(
            &mut self,
            buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            self.inner.recv_frame(buf).await
        }

        async fn discard_input(&mut self) -> std::result::Result<(), TransportError> {
            self.inner.discard_input().await
        }
    }

    /// Manager built with the `InjectableFactory` and a long poll interval
    /// so the poll loop can't consume the armed failure before the test's
    /// foreground request does.
    fn make_manager_with_injectable(factory: Arc<InjectableFactory>) -> Arc<GeminiManager> {
        make_manager_polling_every(Duration::from_mins(5), factory)
    }

    #[tokio::test]
    async fn send_set_and_refresh_propagate_a_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let manager = make_manager_with_injectable(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        factory.fail_next_send.store(true, Ordering::SeqCst);
        let err = manager
            .send_set(&session, SetCommand::Dc(DcOutput::Dc2, false))
            .await
            .unwrap_err();
        assert!(matches!(err, GeminiError::Communication(_)), "{err:?}");

        factory.fail_next_send.store(true, Ordering::SeqCst);
        let err = manager.refresh_status(&session).await.unwrap_err();
        assert!(matches!(err, GeminiError::Communication(_)), "{err:?}");

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_cache_empty_when_the_handshake_send_fails() {
        let factory = Arc::new(InjectableFactory::default());
        let manager = make_manager_with_injectable(Arc::clone(&factory));

        factory.fail_next_send.store(true, Ordering::SeqCst);
        let err = GeminiError::from(manager.transport().acquire().await.unwrap_err());
        assert!(matches!(err, GeminiError::Communication(_)), "{err:?}");
        assert!(!manager.is_available());
        let state = manager.get_cached_state().await;
        assert!(state.status.is_none());
        assert!(state.firmware.is_none());
    }
}
