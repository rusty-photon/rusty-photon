//! Thin manager wrapping `SharedTransport<Upbv2Codec>` plus the cached
//! protocol state both ASCOM devices read from.
//!
//! The refcount, slot, open/close transitions, command-lock arbitration,
//! and poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The `UPBv2` handshake (`P#` → `PV` → `PA` → `PC` → `PS`, seed the cache).
//! * The poll loop body that refreshes `PA` + `PC` + `PS` into the cache.
//! * The cached state both devices share (status, power counters, boot
//!   state, sensor sliding-window means).
//!
//! Both the handshake and the poll loop are **read-only by construction**:
//! the only [`Upbv2Command`] variants either one sends are the five query
//! commands. That is what tenet 3 (no actuation on connect) demands of a box
//! where nearly every write is a power toggle — the handshake re-runs on
//! every reconnect after a serial glitch, so a set command placed here would
//! energise a rail behind the operator's back.
//!
//! Unlike `ppba-driver` there is no USB-hub shadow state: the `UPBv2`'s `PA`
//! reply reports all six USB port states directly, so the cache is a pure
//! function of what the device last said.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SessionError, SharedTransport, TransportFactory, WhileOpen,
};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, warn};

use crate::codec::{Upbv2Codec, Upbv2CodecError, Upbv2Response};
use crate::config::Config;
use crate::error::{Result, Upbv2Error};
use crate::mean::SensorMean;
use crate::protocol::{
    validate_ping_response, Upbv2BootState, Upbv2Command, Upbv2PowerConsumption, Upbv2Status,
};

/// Cached device state shared between the switch device and the
/// observing-conditions device.
///
/// The three frames stay `None` until the handshake seeds them, so a read
/// that arrives before the first successful poll can be answered
/// `NOT_CONNECTED` rather than with a fabricated zero.
#[derive(Debug, Clone, Default)]
pub struct CachedState {
    /// Last `PA` frame: outputs, USB ports, dew duties, sensors, per-channel
    /// currents, overcurrent flags and the auto-dew mask.
    pub status: Option<Upbv2Status>,
    /// Last `PC` frame: the cumulative power counters.
    pub power: Option<Upbv2PowerConsumption>,
    /// Last `PS` frame. Read for one field only — the variable-output
    /// setpoint, which `PA` does not carry.
    pub boot_state: Option<Upbv2BootState>,
    pub last_update: Option<SystemTime>,
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

/// The sensor window used when a client asks for `AveragePeriod = 0`.
///
/// ASCOM reads 0 as "the device is not averaging — give me the most recent
/// value". `SensorMean` has no unaveraged mode, and giving it a literally
/// unbounded window would resurrect the staleness this driver windows
/// `get_mean` on read to avoid: a stalled poll loop would keep answering with
/// an hours-old sample.
///
/// So 0 becomes the shortest window that still always holds the newest
/// sample under healthy polling. Three intervals tolerates two missed polls
/// before readings degrade to `VALUE_NOT_SET`, which is the honest answer once
/// the device has gone quiet that long. The 10 s floor keeps a very fast poll
/// interval from making the window shorter than one client round trip.
fn instantaneous_window(poll_interval: Duration) -> Duration {
    poll_interval.saturating_mul(3).max(Duration::from_secs(10))
}

/// The sensor window that serves `period_hours`.
///
/// Anything above zero is that period exactly; zero routes to
/// [`instantaneous_window`]. `period_hours` is never negative — the device
/// rejects that before it reaches here — so `<= 0.0` reads as "is zero"
/// without tripping `clippy::float_cmp`.
fn effective_window(period_hours: f64, poll_interval: Duration) -> Duration {
    if period_hours <= 0.0 {
        instantaneous_window(poll_interval)
    } else {
        Duration::from_secs_f64(period_hours * 3600.0)
    }
}

/// Manager that wraps the shared transport plus `UPBv2`-specific cached
/// state. One instance per process; both devices hold `Arc<Upbv2Manager>`.
pub struct Upbv2Manager {
    transport: Arc<SharedTransport<Upbv2Codec>>,
    cached_state: Arc<RwLock<CachedState>>,
    /// Kept so [`Upbv2Manager::set_averaging_period`] can size the
    /// instantaneous window against the poll cadence.
    poll_interval: Duration,
}

impl Upbv2Manager {
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

        let hooks = build_hooks(&cached_state, poll_interval);
        let transport = SharedTransport::new(factory, Upbv2Codec, hooks);

        Arc::new(Self {
            transport,
            cached_state,
            poll_interval,
        })
    }

    /// Access the shared transport so devices can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<Upbv2Codec>> {
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
    pub async fn set_averaging_period(&self, period_hours: f64) {
        let window = effective_window(period_hours, self.poll_interval);
        let mut state = self.cached_state.write().await;
        state.temp_mean.set_window(window);
        state.humidity_mean.set_window(window);
        state.dewpoint_mean.set_window(window);
        state.average_period_hours = period_hours;
        drop(state);
        debug!(period_hours, ?window, "sensor averaging period updated");
    }

    /// Issue a protocol command on the device's session and return the
    /// decoded response. Used by both devices for set-commands and the
    /// `UPBv2`-specific routing they do around them.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as an [`Upbv2Error`]: a
    /// transport error, a response the codec cannot decode, or an exhausted
    /// skip budget.
    pub async fn send_command(
        &self,
        session: &Session<Upbv2Codec>,
        cmd: Upbv2Command,
    ) -> Result<Upbv2Response> {
        session.request(cmd).await.map_err(Upbv2Error::from)
    }

    /// Refresh the status (`PA`) cache via the caller's session.
    ///
    /// Devices use this on demand — after a successful set command, so the
    /// very next read reflects the write instead of waiting out a poll
    /// interval. The poll loop does the same thing internally while a
    /// session is alive.
    ///
    /// # Errors
    ///
    /// Returns the [`Session::request`] failure as an [`Upbv2Error`] — a
    /// transport error, a response the codec cannot decode (a malformed or
    /// short `PA` frame included), or an exhausted skip budget; a decoded
    /// frame of the wrong variant is consumed by the skip budget rather than
    /// returned. The cache is left untouched on failure.
    pub async fn refresh_status(&self, session: &Session<Upbv2Codec>) -> Result<()> {
        let resp = session
            .request(Upbv2Command::Status)
            .await
            .map_err(Upbv2Error::from)?;
        let Upbv2Response::Status(status) = resp else {
            return Err(Upbv2Error::InvalidResponse(
                "PA command returned non-status frame".to_string(),
            ));
        };
        let mut state = self.cached_state.write().await;
        apply_status(&mut state, &status);
        drop(state);
        Ok(())
    }

    /// Refresh the boot-state (`PS`) cache via the caller's session.
    ///
    /// This exists because `PA` does **not** carry the variable-output
    /// voltage setpoint — `PS` is the only frame that reports it. The switch
    /// device calls this after a successful `P8:` write so the switch reads
    /// back the value the device actually stored rather than the value the
    /// client asked for.
    ///
    /// # Errors
    ///
    /// The same failure set as [`refresh_status`](Self::refresh_status),
    /// against the `PS` frame. The cache is left untouched on failure.
    pub async fn refresh_boot_state(&self, session: &Session<Upbv2Codec>) -> Result<()> {
        let resp = session
            .request(Upbv2Command::BootState)
            .await
            .map_err(Upbv2Error::from)?;
        let Upbv2Response::BootState(boot_state) = resp else {
            return Err(Upbv2Error::InvalidResponse(
                "PS command returned non-boot-state frame".to_string(),
            ));
        };
        let mut state = self.cached_state.write().await;
        state.boot_state = Some(boot_state);
        drop(state);
        Ok(())
    }
}

fn build_hooks(
    cached_state: &Arc<RwLock<CachedState>>,
    poll_interval: Duration,
) -> Hooks<Upbv2Codec> {
    let cs_handshake = Arc::clone(cached_state);
    let cs_poll = Arc::clone(cached_state);
    Hooks {
        handshake: Box::new(move |conn| {
            let cs = Arc::clone(&cs_handshake);
            Box::pin(handshake(conn, cs))
        }),
        on_last_disconnect: Box::new(|_| Box::pin(async {})),
        shutdown: Box::new(|_| Box::pin(async {})),
        while_open: Some(Box::new(move |ctx| {
            let cs = Arc::clone(&cs_poll);
            Box::pin(poll_loop(ctx, cs, poll_interval))
        })),
    }
}

/// Read-only connect sequence: identify the box, log its firmware, and seed
/// the cache from the three query frames.
///
/// Runs on every transition into `Open`, reconnects included. Nothing here
/// may write device state.
async fn handshake(
    conn: &Connection<Upbv2Codec>,
    cached_state: Arc<RwLock<CachedState>>,
) -> std::result::Result<(), Upbv2CodecError> {
    // Ping first — fails fast on a wrong-protocol peer, and on the one
    // wrong-protocol peer that answers in a recognisable dialect: a PPBA
    // shares this box's USB id, baud rate and framing, but `P3:`/`P4:` set
    // dew heaters there and 12 V rails here.
    let ping = conn.request(Upbv2Command::Ping).await?;
    let Upbv2Response::PingReply(reply) = ping else {
        return Err(Upbv2CodecError::InvalidResponse(
            "handshake ping returned non-ping frame".to_string(),
        ));
    };
    validate_ping_response(&reply).map_err(ping_error_to_codec)?;

    let version_resp = conn.request(Upbv2Command::FirmwareVersion).await?;
    let Upbv2Response::Echo(firmware) = version_resp else {
        return Err(Upbv2CodecError::InvalidResponse(
            "handshake PV returned non-echo frame".to_string(),
        ));
    };
    debug!(firmware, "upbv2 firmware version");

    let status_resp = conn.request(Upbv2Command::Status).await?;
    let Upbv2Response::Status(status) = status_resp else {
        return Err(Upbv2CodecError::InvalidResponse(
            "handshake PA returned non-status frame".to_string(),
        ));
    };

    let power_resp = conn.request(Upbv2Command::PowerConsumption).await?;
    let Upbv2Response::PowerConsumption(power) = power_resp else {
        return Err(Upbv2CodecError::InvalidResponse(
            "handshake PC returned non-power-consumption frame".to_string(),
        ));
    };

    let boot_resp = conn.request(Upbv2Command::BootState).await?;
    let Upbv2Response::BootState(boot_state) = boot_resp else {
        return Err(Upbv2CodecError::InvalidResponse(
            "handshake PS returned non-boot-state frame".to_string(),
        ));
    };

    let mut state = cached_state.write().await;
    apply_status(&mut state, &status);
    state.power = Some(power);
    state.boot_state = Some(boot_state);
    drop(state);
    debug!("upbv2 handshake complete");
    Ok(())
}

/// Re-wrap a [`validate_ping_response`] failure as a codec-layer error.
///
/// The codec's own protocol-to-codec mapping is private to that module, and
/// only two shapes reach here: the wrong-model rejection, and a box that
/// answered something else entirely. The wrong-model message names
/// `ppba-driver`, which is the entire point of the check, so it has to travel
/// out of the handshake intact.
fn ping_error_to_codec(err: Upbv2Error) -> Upbv2CodecError {
    match err {
        wrong_model @ Upbv2Error::WrongModel { .. } => {
            Upbv2CodecError::WrongModel(wrong_model.to_string())
        }
        other => Upbv2CodecError::InvalidResponse(other.to_string()),
    }
}

async fn poll_loop(
    ctx: WhileOpen<Upbv2Codec>,
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
                debug!("upbv2 poll loop received cancellation");
                return;
            }
        }

        match ctx.request(Upbv2Command::Status).await {
            Ok(Upbv2Response::Status(status)) => {
                let mut state = cached_state.write().await;
                apply_status(&mut state, &status);
            }
            Ok(other) => warn!("upbv2 poll: PA returned unexpected frame variant: {other:?}"),
            Err(e) => session_err_to_warn("PA", &e),
        }

        match ctx.request(Upbv2Command::PowerConsumption).await {
            Ok(Upbv2Response::PowerConsumption(power)) => {
                let mut state = cached_state.write().await;
                state.power = Some(power);
            }
            Ok(other) => warn!("upbv2 poll: PC returned unexpected frame variant: {other:?}"),
            Err(e) => session_err_to_warn("PC", &e),
        }

        // PS rides the same interval as PA even though only one of its
        // fields is exposed: the variable-output setpoint is writable from
        // the Pegasus software too, so polling it is how the driver notices
        // a change it did not make.
        match ctx.request(Upbv2Command::BootState).await {
            Ok(Upbv2Response::BootState(boot_state)) => {
                let mut state = cached_state.write().await;
                state.boot_state = Some(boot_state);
            }
            Ok(other) => warn!("upbv2 poll: PS returned unexpected frame variant: {other:?}"),
            Err(e) => session_err_to_warn("PS", &e),
        }
    }
}

fn session_err_to_warn(op: &str, err: &SessionError<Upbv2CodecError>) {
    warn!(op, error = %err, "upbv2 poll request failed");
}

fn apply_status(state: &mut CachedState, status: &Upbv2Status) {
    state.status = Some(status.clone());
    state.last_update = Some(SystemTime::now());
    state.temp_mean.add_sample(status.temperature);
    state.humidity_mean.add_sample(status.humidity);
    state.dewpoint_mean.add_sample(status.dewpoint);
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock
    //! transport factory. Race / refcount / rollback invariants are
    //! tested once for everyone in `rusty-photon-shared-transport` —
    //! they don't get re-tested here per the migration plan.

    use super::*;
    use crate::mock::MockUpbv2TransportFactory;
    use crate::protocol::{DewChannel, OutputId, PwmDuty, UsbPortId, VariableVolts};
    use std::time::Instant;

    /// How long a test waits for the poll loop to write through to the
    /// cache before declaring the loop dead. Generous relative to the
    /// millisecond-scale poll intervals the tests configure — a failure
    /// deadline, not an expected duration.
    const CACHE_DEADLINE: Duration = Duration::from_secs(10);

    fn make_manager() -> Arc<Upbv2Manager> {
        let factory = Arc::new(MockUpbv2TransportFactory::default());
        Upbv2Manager::new(&Config::default(), factory)
    }

    fn make_manager_polling_every(poll_interval: Duration) -> Arc<Upbv2Manager> {
        let mut config = Config::default();
        config.serial.polling_interval = poll_interval;
        Upbv2Manager::new(&config, Arc::new(MockUpbv2TransportFactory::default()))
    }

    /// Poll the cache until `done` accepts it, or fail once `CACHE_DEADLINE`
    /// passes. Watches a window rather than sampling after a fixed nap, so a
    /// slow CI box cannot turn passing behaviour into a red test.
    async fn wait_for_cache(
        manager: &Upbv2Manager,
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

    #[tokio::test]
    async fn acquire_runs_handshake_and_seeds_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());

        let state = manager.get_cached_state().await;
        let status = state.status.expect("status seeded by handshake");
        assert!((status.temperature - 25.0).abs() < f64::EPSILON);
        let power = state.power.expect("power counters seeded by handshake");
        assert!((power.average_amps - 1.85).abs() < f64::EPSILON);
        let boot_state = state.boot_state.expect("boot state seeded by handshake");
        assert_eq!(boot_state.variable_volts, 12);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_scales_dew_c_current_by_its_own_divisor() {
        // Dew C runs through a different MOSFET: the same raw sense count
        // means a different current there than on A or B. The mock emits raw
        // counts, so this proves the parser applied 700 rather than 480.
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        let status = manager
            .get_cached_state()
            .await
            .status
            .expect("status seeded by handshake");
        assert!((status.dew_current[DewChannel::C.index()] - 0.5).abs() < f64::EPSILON);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_rejects_a_ppba_naming_the_right_service() {
        // A PPBA on the same USB id, baud rate and framing answers the ping
        // in its own dialect. Connect must fail, and the failure must tell
        // the operator which service to point at that box instead.
        let manager = Upbv2Manager::new(&Config::default(), Arc::new(PpbaPingFactory));

        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("a PPBA must not complete the UPBv2 handshake");
        let message = Upbv2Error::from(err).to_string();
        assert!(
            message.contains("ppba-driver"),
            "wrong-model error should name ppba-driver, got: {message}"
        );
        assert!(
            message.contains("PPBA_OK"),
            "wrong-model error should quote the wire reply, got: {message}"
        );
        assert!(!manager.is_available());
    }

    #[tokio::test]
    async fn poll_loop_refreshes_all_three_frames_into_the_cache() {
        let manager = make_manager_polling_every(Duration::from_millis(20));
        let session = manager.transport().acquire().await.unwrap();

        // Mutate device state behind the cache's back: `send_command`
        // refreshes nothing, so only the poll loop can make these show up.
        let output = OutputId::new(3).unwrap();
        manager
            .send_command(&session, Upbv2Command::SetOutput(output, true))
            .await
            .unwrap();
        manager
            .send_command(
                &session,
                Upbv2Command::SetVariableVoltage(VariableVolts::new(5).unwrap()),
            )
            .await
            .unwrap();

        let state = wait_for_cache(&manager, "the polled writes", |state| {
            let output_on = state
                .status
                .as_ref()
                .is_some_and(|status| status.outputs[output.index()]);
            let volts_seen = state
                .boot_state
                .as_ref()
                .is_some_and(|boot| boot.variable_volts == 5);
            output_on && volts_seen
        })
        .await;

        // The PC frame rides the same tick, and `last_update` proves the
        // status arm ran rather than the cache still holding the handshake's
        // seed.
        assert!(state.power.is_some(), "poll loop should refresh PC");
        assert!(state.last_update.is_some());

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn poll_loop_stops_when_the_session_closes() {
        let manager = make_manager_polling_every(Duration::from_millis(20));
        let session = manager.transport().acquire().await.unwrap();
        session.close().await.unwrap();
        assert!(!manager.is_available());
    }

    #[tokio::test]
    async fn refresh_status_updates_cache() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        // Mutate device state via a set command, then refresh and observe.
        let channel = DewChannel::B;
        manager
            .send_command(&session, Upbv2Command::SetDew(channel, PwmDuty(200)))
            .await
            .unwrap();
        manager.refresh_status(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.status.unwrap().dew_duty[channel.index()], 200);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_records_usb_state_without_shadow_tracking() {
        // The UPBv2 reports USB port states in PA, so a write plus a refresh
        // is enough — there is no locally tracked hub flag to keep in sync.
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        let port = UsbPortId::new(5).unwrap();
        manager
            .send_command(&session, Upbv2Command::SetUsb(port, true))
            .await
            .unwrap();
        manager.refresh_status(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert!(state.status.unwrap().usb_ports[port.index()]);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_boot_state_updates_variable_voltage() {
        // PA never carries the variable-output setpoint, so this is the only
        // path by which a P8: write becomes readable.
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();

        manager
            .send_command(
                &session,
                Upbv2Command::SetVariableVoltage(VariableVolts::new(9).unwrap()),
            )
            .await
            .unwrap();
        manager.refresh_boot_state(&session).await.unwrap();

        let state = manager.get_cached_state().await;
        assert_eq!(state.boot_state.unwrap().variable_volts, 9);

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn set_averaging_period_resizes_means() {
        let manager = make_manager();
        manager.set_averaging_period(2.0 / 60.0).await;
        let state = manager.get_cached_state().await;
        let new_window = Duration::from_mins(2);
        assert_eq!(state.temp_mean.window(), new_window);
        assert_eq!(state.humidity_mean.window(), new_window);
        assert_eq!(state.dewpoint_mean.window(), new_window);
    }

    #[tokio::test]
    async fn zero_average_period_windows_three_poll_intervals() {
        // The regression this guards: windowing `get_mean` on read means a
        // window shorter than the poll interval holds no sample for most of
        // each interval, so "no averaging" would answer VALUE_NOT_SET between
        // polls. The poll interval here is well above the 10 s floor.
        let manager = make_manager_polling_every(Duration::from_secs(60));
        manager.set_averaging_period(0.0).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), Duration::from_secs(180));
        assert!((state.average_period_hours - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn zero_average_period_never_windows_below_ten_seconds() {
        let manager = make_manager_polling_every(Duration::from_secs(1));
        manager.set_averaging_period(0.0).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn average_period_is_recorded_verbatim_not_inferred_from_the_window() {
        // A period whose window equals the instantaneous window must still
        // read back as itself, not as 0.
        let manager = make_manager_polling_every(Duration::from_secs(60));
        let three_minutes_in_hours = 180.0 / 3600.0;
        manager.set_averaging_period(three_minutes_in_hours).await;
        let state = manager.get_cached_state().await;
        assert_eq!(state.temp_mean.window(), Duration::from_secs(180));
        assert!((state.average_period_hours - three_minutes_in_hours).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn close_releases_transport() {
        let manager = make_manager();
        let session = manager.transport().acquire().await.unwrap();
        assert!(manager.is_available());
        session.close().await.unwrap();
        assert!(!manager.is_available());
    }

    #[test]
    fn session_err_to_warn_logs_without_panicking() {
        // `session_err_to_warn` is only invoked from the poll loop's Err
        // arms — which never fire in tests because the mock factory always
        // succeeds. The function emits a `warn!` and returns nothing
        // observable, so a direct call is the simplest way to keep the
        // log-only helper covered.
        session_err_to_warn(
            "PA",
            &SessionError::Transport(rusty_photon_shared_transport::TransportError::Eof),
        );
    }

    // ========================================================================
    // Test transports: a PPBA impostor for the model guard, and an
    // injectable wire failure for the error branches. Mirrors the
    // InjectableFactory pattern used in qhy-focuser; the canonical race /
    // refcount / rollback invariants remain tested once in
    // rusty-photon-shared-transport.
    // ========================================================================

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Answers every command with `PPBA_OK` — the reply a Pocket Powerbox
    /// Advance gives to `P#`. Only the ping is ever reached: the handshake
    /// stops there.
    struct PpbaPingFactory;

    #[async_trait]
    impl TransportFactory for PpbaPingFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            Ok(Box::new(PpbaPingTransport))
        }
    }

    struct PpbaPingTransport;

    #[async_trait]
    impl FrameTransport for PpbaPingTransport {
        async fn send_frame(&mut self, _bytes: &[u8]) -> std::result::Result<(), TransportError> {
            Ok(())
        }

        async fn recv_frame(
            &mut self,
            buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            buf.clear();
            buf.extend_from_slice(b"PPBA_OK\n");
            Ok(())
        }
    }

    /// Wraps the canonical mock factory but gates `send_frame` behind a
    /// shared atomic — flipping it makes the very next send return EOF.
    /// Used to inject a wire-level failure after handshake has succeeded
    /// (or, by arming the flag *before* acquire, during handshake).
    #[derive(Default, Clone)]
    struct InjectableFactory {
        inner: MockUpbv2TransportFactory,
        fail_next_send: Arc<AtomicBool>,
    }

    impl InjectableFactory {
        fn fail_next_send(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.fail_next_send)
        }
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
    }

    /// Manager built with the `InjectableFactory` and a long poll interval
    /// (5 minutes) so the poll loop can't consume the armed failure before
    /// the test's foreground request does.
    fn make_manager_with_factory(factory: Arc<InjectableFactory>) -> Arc<Upbv2Manager> {
        let mut config = Config::default();
        config.serial.polling_interval = Duration::from_mins(5);
        Upbv2Manager::new(&config, factory)
    }

    #[tokio::test]
    async fn send_command_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .send_command(
                &session,
                Upbv2Command::SetOutput(OutputId::new(1).unwrap(), false),
            )
            .await
            .expect_err("send_command should propagate the transport failure");
        // TransportError::Eof routes through the driver_error! From impl to
        // Upbv2Error::Communication("Connection closed").
        assert!(
            matches!(err, Upbv2Error::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_status_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .refresh_status(&session)
            .await
            .expect_err("refresh_status should propagate the transport failure");
        assert!(
            matches!(err, Upbv2Error::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn refresh_boot_state_propagates_transport_failure() {
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));
        let session = manager.transport().acquire().await.unwrap();

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .refresh_boot_state(&session)
            .await
            .expect_err("refresh_boot_state should propagate the transport failure");
        assert!(
            matches!(err, Upbv2Error::Communication(_)),
            "expected Communication, got {err:?}"
        );

        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn acquire_returns_err_and_keeps_cache_empty_when_handshake_send_fails() {
        // Failing the first handshake send (Ping) must:
        // - propagate Err from acquire(),
        // - leave is_available() == false (the RollbackGuard fired),
        // - leave the cache untouched (no PA / PC / PS seeded from a partial
        //   handshake).
        let factory = Arc::new(InjectableFactory::default());
        let fail_switch = factory.fail_next_send();
        let manager = make_manager_with_factory(Arc::clone(&factory));

        fail_switch.store(true, Ordering::SeqCst);
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake failure should propagate out of acquire");
        let mapped = Upbv2Error::from(err);
        assert!(
            matches!(mapped, Upbv2Error::Communication(_)),
            "expected Communication, got {mapped:?}"
        );

        assert!(
            !manager.is_available(),
            "RollbackGuard should have rolled the refcount back"
        );

        let state = manager.get_cached_state().await;
        assert!(
            state.status.is_none(),
            "handshake should not have seeded status"
        );
        assert!(
            state.power.is_none(),
            "handshake should not have seeded power counters"
        );
        assert!(
            state.boot_state.is_none(),
            "handshake should not have seeded boot state"
        );
        assert!(state.last_update.is_none());
    }
}
