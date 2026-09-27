//! Thin manager wrapping `SharedTransport<SkywatcherCodec>` plus the
//! mount-specific cached state (parameters from the handshake, the
//! background poll snapshot).
//!
//! The refcount, slot, open/close transitions, command-lock arbitration,
//! and poll-task lifetime all live in
//! [`rusty_photon_shared_transport::SharedTransport`]. What stays here:
//!
//! * The Sky-Watcher handshake (`:F` × 2, `:a` × 2, `:b`, `:g` × 2,
//!   `:e`, `:j` × 2) and the parameter cache it populates.
//! * The background poll loop body that refreshes `:f` + `:j` on both
//!   axes into the snapshot.
//! * The [`PollPauseGuard`] mechanism the slew/park watchers use to
//!   pause the polling task while they own the wire.
//! * `seed_*_position` mutators that publish `:E`-written encoder
//!   values into the snapshot immediately (so reads landing right
//!   after `Sync` don't see the pre-sync position).
//! * The per-axis record of the last `:I` step period the driver sent,
//!   and [`MountManager::snapshot_now`], which projects the poll sample
//!   forward to the read instant at that commanded rate (issue #1334).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusty_photon_shared_transport::{
    Connection, Hooks, Session, SharedTransport, StateAssertion, TransportFactory, WhileOpen,
};
use skywatcher_motor_protocol::{
    Axis, AxisStatus, Command, Direction, ModeKind, MountType, Response, Speed,
};
use tokio::sync::RwLock;
use tokio::time::{interval, Instant};
use tracing::{debug, error, info, warn};

use crate::codec::{decode_frame_for, SkywatcherCodec, SkywatcherCodecError};
use crate::config::{Config, TransportConfig};
use crate::coordinates::sidereal_step_period;
use crate::error::{Result, StarAdvError};
use crate::units::{sat_round_i32, Cpr};

/// Snapshot of the values the mount reports during the init handshake.
/// Meaningful units are in the design doc.
#[derive(Debug, Clone, Copy, Default)]
pub struct MountParameters {
    pub cpr_ra: u32,
    pub cpr_dec: u32,
    pub tmr_freq: u32,
    pub high_speed_ratio_ra: u32,
    pub high_speed_ratio_dec: u32,
    pub motor_board_version: u32,
    pub ra_at_handshake_ticks: i32,
    pub dec_at_handshake_ticks: i32,
}

impl MountParameters {
    /// Step period that turns the RA axis at the sidereal rate.
    ///
    /// Per-axis methods (instead of taking a CPR, or an `Axis`) keep a
    /// caller from pairing one axis with the other's CPR: `:I` sets the
    /// time between motor steps, the axes' CPRs differ, so a period
    /// derived from the wrong CPR turns the axis at the wrong angular
    /// rate by exactly the CPR ratio.
    #[must_use]
    pub fn sidereal_step_period_ra(&self) -> u32 {
        sidereal_step_period(self.tmr_freq, Cpr::new(self.cpr_ra))
    }

    /// Step period that turns the Dec axis at the sidereal rate. See
    /// [`Self::sidereal_step_period_ra`] for why this is per-axis.
    #[must_use]
    pub fn sidereal_step_period_dec(&self) -> u32 {
        sidereal_step_period(self.tmr_freq, Cpr::new(self.cpr_dec))
    }
}

/// Latest poll-loop snapshot. Updated by the background task at
/// `polling_interval`.
///
/// A sample is a *past* state of the axis: it is up to one
/// `polling_interval` (plus the poll cycle's own wire time) old by the
/// time a reader sees it. Readers that combine the encoder with the
/// sidereal time taken *now* must first bring the sample to the read
/// instant with [`Self::projected_to`] (via
/// [`MountManager::snapshot_now`]); pairing a stale encoder with a live
/// LST reads a tracking mount off by the sample's age (issue #1334).
#[derive(Debug, Clone, Copy, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag mirrors one independent bit of the `:f` status reply"
)]
pub struct AxisSnapshot {
    pub position_ticks: i32,
    /// When the `:j` position reply that produced `position_ticks`
    /// arrived (or when a `seed_*_position` published a just-written
    /// `:E` value). `None` until the first sample.
    pub sampled_at: Option<Instant>,
    pub running: bool,
    pub goto: bool,
    /// `:f` direction bit: `true` = CCW, i.e. decreasing encoder counts
    /// on the `GTi`.
    pub ccw: bool,
    /// `:f` speed bit: `true` = the high-speed regime, whose step rate is
    /// the `:I` rate times the axis' high-speed ratio.
    pub fast: bool,
    /// The `:I` step period the driver last sent this axis, as of the
    /// sample (`0` = none since the handshake, so the rate is unknown).
    /// Together with `running`, `goto`, `ccw` and `fast` this gives the
    /// rate the axis was turning at — whatever rate the driver
    /// commanded (sidereal, a guide-shifted pulse rate, or any future
    /// tracking rate), not an assumed sidereal.
    pub step_period: u32,
    /// Sky-Watcher spec §5 (Response E nibble-1 bit-1): the firmware
    /// reports `Blocked` when the motor is stepping but the encoder
    /// isn't advancing — typically because the axis is against a
    /// mechanical stop or stalled. The slew watcher uses this to
    /// abort a runaway goto before the gearbox is damaged.
    pub blocked: bool,
}

impl AxisSnapshot {
    /// Signed encoder rate at the sample, in ticks per second.
    ///
    /// Non-zero only while the axis was running in **tracking** mode
    /// with a known `:I` period: the firmware then steps at
    /// `tmr_freq / step_period` steps per second (times the high-speed
    /// ratio in the fast regime), CW counting up. A stopped axis is
    /// `0`, which is exact. A goto is also `0` — its accelerating
    /// profile has no single rate, and nothing precision-critical reads
    /// the encoder mid-goto (the pickup loop reads after both axes stop).
    #[must_use]
    pub fn rate_ticks_per_sec(&self, tmr_freq: u32, high_speed_ratio: u32) -> f64 {
        if !self.running || self.goto || self.step_period == 0 {
            return 0.0;
        }
        let gearing = if self.fast {
            f64::from(high_speed_ratio)
        } else {
            1.0
        };
        let rate = f64::from(tmr_freq) / f64::from(self.step_period) * gearing;
        if self.ccw {
            -rate
        } else {
            rate
        }
    }

    /// This sample carried forward to `now` at the rate the axis was
    /// turning when it was taken (see [`Self::rate_ticks_per_sec`]).
    ///
    /// The projection spans at most `max_age`: past that the sample is
    /// too old to trust that the axis kept its state (the poll loop is
    /// failing, or paused with nobody refreshing it), so the estimate
    /// stops advancing rather than extrapolating without bound. An
    /// unsampled axis, a stopped or goto axis, and a `now` earlier than
    /// the sample all return the sample unchanged.
    #[must_use]
    pub fn projected_to(
        &self,
        now: Instant,
        tmr_freq: u32,
        high_speed_ratio: u32,
        max_age: Duration,
    ) -> Self {
        let Some(sampled_at) = self.sampled_at else {
            return *self;
        };
        let rate = self.rate_ticks_per_sec(tmr_freq, high_speed_ratio);
        if rate == 0.0 {
            return *self;
        }
        let age = now.saturating_duration_since(sampled_at).min(max_age);
        let ticks = rate.mul_add(age.as_secs_f64(), f64::from(self.position_ticks));
        Self {
            position_ticks: sat_round_i32(ticks),
            sampled_at: sampled_at.checked_add(age).or(Some(sampled_at)),
            ..*self
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MountSnapshot {
    pub ra: AxisSnapshot,
    pub dec: AxisSnapshot,
}

impl MountSnapshot {
    /// Take each axis of `other` that was sampled no earlier than ours.
    ///
    /// The background poll builds its sample over four round trips and
    /// publishes it at the end; a motion command's re-read or a sync's
    /// seed can land in between with a newer sample of one axis, which
    /// the older poll value must not overwrite.
    pub fn merge_newer(&mut self, other: &Self) {
        if other.ra.sampled_at >= self.ra.sampled_at {
            self.ra = other.ra;
        }
        if other.dec.sampled_at >= self.dec.sampled_at {
            self.dec = other.dec;
        }
    }

    /// Both axes carried forward to `now`; see
    /// [`AxisSnapshot::projected_to`].
    #[must_use]
    pub fn projected_to(&self, now: Instant, params: &MountParameters, max_age: Duration) -> Self {
        Self {
            ra: self
                .ra
                .projected_to(now, params.tmr_freq, params.high_speed_ratio_ra, max_age),
            dec: self
                .dec
                .projected_to(now, params.tmr_freq, params.high_speed_ratio_dec, max_age),
        }
    }
}

/// How many polling intervals a sample may be carried forward by
/// [`MountManager::snapshot_now`]. A healthy poll loop keeps samples
/// within about one interval plus the cycle's wire time; four leaves
/// room for a slow Wi-Fi cycle or a skipped tick without letting a
/// stalled loop extrapolate for ever.
const MAX_PROJECTION_POLLS: u32 = 4;

/// The last `:I` step period the driver sent, per axis. Recorded by
/// [`MountManager::send`] (every driver `:I` goes through it) and
/// copied into each poll sample, so the projection uses the commanded
/// rate rather than an assumed one. `0` = nothing sent since the
/// handshake — the firmware's period is then unknown and samples are
/// not projected.
#[derive(Debug, Default)]
struct CommandedStepPeriods {
    ra: AtomicU32,
    dec: AtomicU32,
}

impl CommandedStepPeriods {
    fn record(&self, axis: Axis, period: u32) {
        if matches!(axis, Axis::Ra | Axis::Both) {
            self.ra.store(period, Ordering::SeqCst);
        }
        if matches!(axis, Axis::Dec | Axis::Both) {
            self.dec.store(period, Ordering::SeqCst);
        }
    }

    fn get(&self, axis: Axis) -> u32 {
        match axis {
            Axis::Ra => self.ra.load(Ordering::SeqCst),
            Axis::Dec => self.dec.load(Ordering::SeqCst),
            // A poll sample is per-axis; `Both` never reaches here.
            Axis::Both => 0,
        }
    }

    fn clear(&self) {
        self.ra.store(0, Ordering::SeqCst);
        self.dec.store(0, Ordering::SeqCst);
    }
}

/// RAII guard that pauses background polling while held.
///
/// Drop decrements a depth counter; the polling task resumes only when
/// the counter reaches zero. Returned by
/// [`MountManager::pause_background_polling`].
///
/// Ref-counted (not a plain bool) so overlapping guards from
/// different paths (e.g. an `AbortSlew` racing a still-running slew
/// watcher) can't prematurely resume polling while another guard
/// is still active.
///
/// While paused, the watcher is the *only* writer on the wire
/// (modulo concurrent Alpaca-client driven `MountDevice` ops, which
/// are rare during a slew). The watcher must poll `:f` / `:j`
/// directly via the watcher's own session to keep the cached snapshot
/// fresh for any ASCOM reader that happens to fire during the slew.
pub struct PollPauseGuard {
    depth: Arc<AtomicU32>,
}

impl Drop for PollPauseGuard {
    fn drop(&mut self) {
        // We only ever increment-and-then-decrement-on-drop, so the
        // counter can't go negative under normal control flow. If a
        // future refactor somehow drops a guard twice, fetch_sub at 0
        // would underflow to `u32::MAX` — switch to a CAS-based
        // saturating decrement here if that risk becomes real.
        self.depth.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Manager that wraps the shared transport plus mount-specific
/// state. One instance per process; the `MountDevice` and every
/// spawned watcher hold `Arc<MountManager>`.
pub struct MountManager {
    transport: Arc<SharedTransport<SkywatcherCodec>>,
    parameters: Arc<RwLock<Option<MountParameters>>>,
    snapshot: Arc<RwLock<MountSnapshot>>,
    step_periods: Arc<CommandedStepPeriods>,
    poll_pause_depth: Arc<AtomicU32>,
    polling_interval: Duration,
    command_timeout: Duration,
}

impl MountManager {
    pub fn new(config: &Config, factory: Arc<dyn TransportFactory>) -> Arc<Self> {
        let parameters = Arc::new(RwLock::new(None));
        let snapshot = Arc::new(RwLock::new(MountSnapshot::default()));
        let step_periods = Arc::new(CommandedStepPeriods::default());
        let poll_pause_depth = Arc::new(AtomicU32::new(0));
        let (polling_interval, command_timeout) = match &config.transport {
            TransportConfig::Usb(usb) => (usb.polling_interval, usb.command_timeout),
            TransportConfig::Udp(udp) => (udp.polling_interval, udp.command_timeout),
        };
        // Capture once; the handshake hook quotes this in the
        // [`StarAdvError::WrongDevice`] diagnostic when the `:e1` identity
        // probe rejects the device on the other end of the wire (issue #254).
        let port_label: Arc<str> = Arc::from(config.transport.port_label());

        let hooks = build_hooks(
            &parameters,
            &snapshot,
            &step_periods,
            &poll_pause_depth,
            polling_interval,
            &port_label,
        );
        let transport = SharedTransport::new(factory, SkywatcherCodec, hooks);

        Arc::new(Self {
            transport,
            parameters,
            snapshot,
            step_periods,
            poll_pause_depth,
            polling_interval,
            command_timeout,
        })
    }

    /// Access the shared transport so devices can acquire sessions.
    #[must_use]
    pub const fn transport(&self) -> &Arc<SharedTransport<SkywatcherCodec>> {
        &self.transport
    }

    /// Cheap, non-blocking snapshot — true between handshake completion
    /// and the start of teardown.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.transport.is_available()
    }

    /// Latest cached parameters. `None` until handshake completes.
    pub async fn parameters(&self) -> Option<MountParameters> {
        *self.parameters.read().await
    }

    /// Latest poll-loop snapshot, exactly as sampled.
    ///
    /// The positions are up to a poll old. Anything that pairs them
    /// with the current sidereal time wants [`Self::snapshot_now`].
    pub async fn snapshot(&self) -> MountSnapshot {
        *self.snapshot.read().await
    }

    /// Latest poll-loop snapshot with both axes carried forward to the
    /// current instant at their commanded rates.
    ///
    /// This is the encoder estimate to pair with an LST taken *now*:
    /// while tracking, the RA encoder keeps moving after the poll
    /// sampled it, and `LST(now) − HA(sample)` reads the sky off by the
    /// sample's age — up to a poll interval of RA, with a random sign
    /// between reads (issue #1334). A stopped axis is returned as
    /// sampled. Before the handshake has cached the parameters the raw
    /// snapshot is returned.
    pub async fn snapshot_now(&self) -> MountSnapshot {
        let snap = self.snapshot().await;
        self.parameters()
            .await
            .map_or(snap, |params| self.project_to_now(&snap, &params))
    }

    /// Carry `snap` forward to the current instant; see
    /// [`Self::snapshot_now`]. For callers holding a snapshot of their
    /// own, such as the slew watcher's `poll_axes_now` result.
    #[must_use]
    pub fn project_to_now(&self, snap: &MountSnapshot, params: &MountParameters) -> MountSnapshot {
        snap.projected_to(Instant::now(), params, self.max_projection_age())
    }

    const fn max_projection_age(&self) -> Duration {
        self.polling_interval.saturating_mul(MAX_PROJECTION_POLLS)
    }

    /// Wire-protocol polling interval taken from the config block. Exposed
    /// so the slew-completion watcher can match the background poller's
    /// cadence.
    #[must_use]
    pub const fn polling_interval_for_watcher(&self) -> Duration {
        self.polling_interval
    }

    /// Pause the background polling task and return a guard that
    /// resumes it on drop. Used by the slew/park watchers to free the
    /// wire during a slew — pickup-loop commands run without
    /// contending with `:j` / `:f` polls, and the watcher's own
    /// [`poll_axes_now`](Self::poll_axes_now) drives snapshot
    /// freshness while paused.
    ///
    /// Ref-counted: each call increments a depth counter; polling
    /// resumes only when the last guard drops (counter back to 0).
    /// Safe to nest or overlap across paths.
    ///
    /// Advisory, not synchronous: `poll_loop` reads the counter once
    /// per iteration, at the top, so a poll cycle already past that
    /// read runs to completion and can still put its remaining `:j` /
    /// `:f` frames on the wire after this returns. Callers get reduced
    /// wire load, not exclusive wire access — mutual exclusion against
    /// the poll loop comes from the shared transport's command lock.
    #[must_use]
    pub fn pause_background_polling(&self) -> PollPauseGuard {
        self.poll_pause_depth.fetch_add(1, Ordering::SeqCst);
        PollPauseGuard {
            depth: Arc::clone(&self.poll_pause_depth),
        }
    }

    /// Send one command on the caller's session, return one typed
    /// response. Does *not* update the snapshot — the background
    /// poller (and `poll_axes_now`) own that responsibility.
    ///
    /// Pre-validates command variants whose
    /// [`skywatcher_motor_protocol::Command::encode`] is fallible
    /// (currently [`Command::SetPosition`] / [`Command::SetGotoTarget`],
    /// both of which call `encode_position` on an `i32` tick value that
    /// must fit in signed-24-bit range). The validation lives here so
    /// every send path on the service shares one check and the codec's
    /// `encode` is reached only with already-valid inputs. A
    /// validation failure returns [`StarAdvError::InvalidValue`]
    /// without touching the wire.
    ///
    /// # Errors
    ///
    /// Returns [`StarAdvError::InvalidValue`] for an out-of-range tick
    /// value as above; the [`Session::request`] failure as a
    /// [`StarAdvError`] — a transport error, the codec's protocol error, or
    /// an exhausted skip budget; or
    /// [`StarAdvError::Protocol`] if the reply does not decode against
    /// `command`, a `!` error reply included.
    ///
    /// Commands that change how an axis moves also keep the cached
    /// sample's rate state honest, so [`Self::snapshot_now`] never
    /// projects a sample at a rate the axis has already left:
    ///
    /// * `:I` records the step period now in force for the axis;
    /// * `:J` re-reads the axis (`:j` + `:f`), stamping a sample that
    ///   carries the motion just started;
    /// * `:K` / `:L` re-read the axis position and mark its rate
    ///   unknown — the axis is halting, and where it stops is for the
    ///   next poll to say.
    ///
    /// Without the last two, a sample taken just before a start or stop
    /// would be projected at the old rate until the next poll replaced
    /// it — up to a poll interval of error in exactly the window a
    /// client reads in after starting tracking or finishing a pulse. A
    /// failed re-read is logged and leaves the sample to the next poll;
    /// the command itself has succeeded.
    pub async fn send(
        &self,
        session: &Session<SkywatcherCodec>,
        command: Command,
    ) -> Result<Response> {
        let response = self.request(session, command.clone()).await?;
        match command {
            Command::SetStepPeriod { axis, period } => self.step_periods.record(axis, period),
            Command::StartMotion(axis) => self.refresh_axis_samples(session, axis, false).await,
            Command::StopMotion(axis) | Command::InstantStop(axis) => {
                self.refresh_axis_samples(session, axis, true).await;
            }
            _ => {}
        }
        Ok(response)
    }

    /// One validated round trip, with none of [`Self::send`]'s snapshot
    /// bookkeeping. The poll paths use it directly.
    async fn request(
        &self,
        session: &Session<SkywatcherCodec>,
        command: Command,
    ) -> Result<Response> {
        validate_command_args(&command)?;
        let bytes = session
            .request(command.clone())
            .await
            .map_err(StarAdvError::from)?;
        decode_frame_for(&command, &bytes).map_err(StarAdvError::from)
    }

    /// Re-read `axis` (both, for [`Axis::Both`]) into the cached
    /// snapshot after a start or stop; see [`Self::send`].
    async fn refresh_axis_samples(
        &self,
        session: &Session<SkywatcherCodec>,
        axis: Axis,
        stopping: bool,
    ) {
        let to_refresh: &[Axis] = match axis {
            Axis::Ra => &[Axis::Ra],
            Axis::Dec => &[Axis::Dec],
            Axis::Both => &[Axis::Ra, Axis::Dec],
        };
        for &axis in to_refresh {
            let mut sample = AxisSnapshot::default();
            if let Err(e) = poll_axis_via_session(self, session, axis, &mut sample).await {
                debug!("re-reading {axis:?} after a motion command failed: {e}");
                continue;
            }
            if stopping {
                sample.step_period = 0;
            }
            let mut snap = self.snapshot.write().await;
            match axis {
                Axis::Dec => snap.dec = sample,
                _ => snap.ra = sample,
            }
        }
    }

    /// Synchronously round-trip `:f` + `:j` on both axes via the
    /// caller's session, update the cached snapshot, and return the
    /// fresh snapshot.
    ///
    /// Used by the slew/park watcher loops *instead of* reading the
    /// background polling task's cached snapshot. The caller is
    /// responsible for ensuring the background polling task is paused
    /// via [`pause_background_polling`](Self::pause_background_polling)
    /// during a sequence of `poll_axes_now` calls — otherwise the two
    /// paths contend for the connection's command lock and the wire
    /// load doubles.
    ///
    /// # Errors
    ///
    /// Returns [`Self::send`]'s failure for any of the four requests, or
    /// [`StarAdvError::Transport`] if a reply is not the position or status
    /// shape; the cached snapshot is left untouched in either case.
    pub async fn poll_axes_now(&self, session: &Session<SkywatcherCodec>) -> Result<MountSnapshot> {
        let mut snap = MountSnapshot::default();
        poll_axis_via_session(self, session, Axis::Ra, &mut snap.ra).await?;
        poll_axis_via_session(self, session, Axis::Dec, &mut snap.dec).await?;
        *self.snapshot.write().await = snap;
        Ok(snap)
    }

    /// Update the cached snapshot's RA position.
    ///
    /// Used by `SyncToCoordinates` to publish the just-written encoder
    /// position immediately rather than waiting up to
    /// `polling_interval` for the background task to refresh.
    ///
    /// Per-axis methods (instead of taking an `Axis`) eliminate the
    /// `Axis::Both` case at the type level — `:E3` (both axes) isn't
    /// part of the MVP wire surface, sync writes per-axis values that
    /// can differ, and there's no sensible single-tick interpretation
    /// of "seed both".
    ///
    /// The seeded value is stamped as sampled now — the `:E` has just
    /// set the encoder to it — so [`Self::snapshot_now`] carries it
    /// forward from this instant. The axis' running state and rate stay
    /// those of the previous sample: `:E` does not change them.
    pub async fn seed_ra_position(&self, ticks: i32) {
        let mut snap = self.snapshot.write().await;
        snap.ra.position_ticks = ticks;
        snap.ra.sampled_at = Some(Instant::now());
    }

    /// Update the cached snapshot's Dec position. See
    /// [`seed_ra_position`](Self::seed_ra_position) for rationale.
    pub async fn seed_dec_position(&self, ticks: i32) {
        let mut snap = self.snapshot.write().await;
        snap.dec.position_ticks = ticks;
        snap.dec.sampled_at = Some(Instant::now());
    }

    /// Per-call command timeout from the active transport config block.
    /// Exposed so handshake and poll loops can match the configured
    /// expectation; the shared-transport `FrameTransport`s already
    /// enforce the same value at the wire layer.
    #[must_use]
    pub const fn command_timeout(&self) -> Duration {
        self.command_timeout
    }
}

fn build_hooks(
    parameters: &Arc<RwLock<Option<MountParameters>>>,
    snapshot: &Arc<RwLock<MountSnapshot>>,
    step_periods: &Arc<CommandedStepPeriods>,
    poll_pause_depth: &Arc<AtomicU32>,
    polling_interval: Duration,
    port_label: &Arc<str>,
) -> Hooks<SkywatcherCodec> {
    let p_hs = Arc::clone(parameters);
    let s_hs = Arc::clone(snapshot);
    let s_poll = Arc::clone(snapshot);
    let periods_hs = Arc::clone(step_periods);
    let periods_poll = Arc::clone(step_periods);
    let depth_poll = Arc::clone(poll_pause_depth);
    let p_sd = Arc::clone(parameters);
    let port_hs = Arc::clone(port_label);
    Hooks {
        handshake: Box::new(move |conn| {
            let parameters = Arc::clone(&p_hs);
            let snapshot = Arc::clone(&s_hs);
            let step_periods = Arc::clone(&periods_hs);
            let port_label = Arc::clone(&port_hs);
            Box::pin(handshake(
                conn,
                parameters,
                snapshot,
                step_periods,
                port_label,
            ))
        }),
        // Safety stop only — do NOT clear the parameter cache. In
        // `ServiceLifetime` mode the transport stays open and the next
        // `acquire()` reuses the cache populated by the startup
        // handshake; clearing here would make the next post-acquire
        // hook (`seed_after_connect`, `load_park_target_after_connect`,
        // any motion command) see `parameters() == None` and fail with
        // `NOT_CONNECTED`. The cached values are immutable hardware
        // facts about the connected device and remain valid until the
        // device itself changes, which is handled by the reconnect
        // supervisor's handshake against the new connection.
        on_last_disconnect: Box::new(move |conn| Box::pin(safety_stop(conn))),
        // Phase 1: run the same `:L1, :L2, :K1` safety sequence at
        // service shutdown that runs at every last-client-disconnect.
        // The mount may be in any state (e.g. the supervisor was
        // mid-reconnect, a client crashed and never disconnected, the
        // last client did disconnect but a stray request landed in
        // between) — stopping tracking + halting both axes is
        // idempotent and the safest default for an unattended dome.
        // Clear the parameter cache as part of final teardown so a
        // future `start()` on the same manager (cold restart) does
        // not see stale handshake data.
        shutdown: Box::new(move |conn| {
            let parameters = Arc::clone(&p_sd);
            Box::pin(shutdown_teardown(conn, parameters))
        }),
        while_open: Some(Box::new(move |ctx| {
            let snapshot = Arc::clone(&s_poll);
            let step_periods = Arc::clone(&periods_poll);
            let depth = Arc::clone(&depth_poll);
            Box::pin(poll_loop(
                ctx,
                snapshot,
                step_periods,
                depth,
                polling_interval,
            ))
        })),
    }
}

/// `0→1` handshake.
///
/// **`:e1` first.** The motor-board-version inquiry runs as the very first
/// wire command, with the reply validated against the
/// [`MountType`] whitelist *before* any mount-specific
/// initialisation (`:F1` / `:F2`) goes on the wire. If the device on the
/// other end isn't a Sky-Watcher motor controller (wrong serial port,
/// foreign USB-CDC peripheral, wrong UDP host, etc.), the handshake
/// aborts after sending exactly one frame (`:e1\r`) and surfaces
/// [`SkywatcherCodecError::WrongDevice`] carrying the port label and a
/// human-readable reason. See [issue #254][issue] for the hardware session
/// that motivated this and the operator-facing diagnostic shape.
///
/// On success, the rest of the handshake follows the documented order:
/// initialise both axes, query parameters, seed the snapshot with the
/// initial encoder positions.
///
/// [issue]: https://github.com/rusty-photon/rusty-photon/issues/254
async fn handshake(
    conn: &Connection<SkywatcherCodec>,
    parameters: Arc<RwLock<Option<MountParameters>>>,
    snapshot: Arc<RwLock<MountSnapshot>>,
    step_periods: Arc<CommandedStepPeriods>,
    port_label: Arc<str>,
) -> std::result::Result<(), SkywatcherCodecError> {
    // Step 1: identify the device. `:e1` is the first (and, on a
    // wrong-device handshake, the *only*) frame on the wire.
    //
    // Error routing:
    //
    // - `SkywatcherCodecError::Transport(TransportError::*)` —
    //   *transport-layer* failures (`Timeout`, `Eof`, `Io`, `Open`,
    //   `Framing` — the last being e.g. UDP datagram-bounds /
    //   max-frame violations). Propagated unchanged so the existing
    //   `Timeout` / `ConnectionFailed` / `Transport` classifications
    //   survive into the operator-facing ASCOM error.
    //
    // - `SkywatcherCodecError::Protocol(ProtocolError::*)` —
    //   *codec-layer* response failures (`FrameError`, `PayloadError`,
    //   `HexError`, plus `MountError` for a structurally-valid `!X\r`
    //   reply). All converted to `WrongDevice`.
    //
    // The handshake uses the same `expect_u24` shape as the U24
    // inquiries below (`:a`, `:b`, `:g`) — `Response::decode` for
    // `Command::InquireMotorBoardVersion` only constructs
    // `Response::U24` on success, so the non-U24 case `expect_u24`
    // would catch is structurally unreachable; mapping a Protocol error
    // once on the outer call covers every failure mode.
    let board = expect_u24(
        request_typed(conn, Command::InquireMotorBoardVersion(Axis::Ra))
            .await
            .map_err(|e| wrong_device_for_e1(e, &port_label))?,
    )
    .map_err(|e| wrong_device_for_e1(e, &port_label))?;
    let mount_type = MountType::from_motor_board_version(board).map_err(|byte| {
        SkywatcherCodecError::wrong_device(
            port_label.as_ref(),
            format!(
                "`:e1` reply mount-type byte {byte:#04X} is not a known Sky-Watcher \
                 mount-controller ID (reply: {board:#08X})"
            ),
        )
    })?;
    let motor_board = format!("{board:#08X}");
    debug!(
        motor_board,
        mount_type = ?mount_type,
        "motor-board version validated"
    );

    // Step 2–3: now safe to issue mount-specific init. Initialise both
    // axes.
    for axis in [Axis::Ra, Axis::Dec] {
        expect_ack(request_typed(conn, Command::Initialize(axis)).await?)?;
    }

    // Step 4–5: per-axis CPR.
    let cpr_ra = expect_u24(request_typed(conn, Command::InquireCpr(Axis::Ra)).await?)?;
    let cpr_dec = expect_u24(request_typed(conn, Command::InquireCpr(Axis::Dec)).await?)?;
    // Step 6: TMR_Freq.
    let tmr_freq = expect_u24(request_typed(conn, Command::InquireTmrFreq).await?)?;
    // Step 7–8: high-speed ratio per axis.
    let hsr_ra = expect_u24(request_typed(conn, Command::InquireHighSpeedRatio(Axis::Ra)).await?)?;
    let hsr_dec =
        expect_u24(request_typed(conn, Command::InquireHighSpeedRatio(Axis::Dec)).await?)?;

    // Step 9–10: initial encoder positions seed the snapshot.
    let pos_ra = expect_position(request_typed(conn, Command::InquirePosition(Axis::Ra)).await?)?;
    let ra_sampled_at = Instant::now();
    let pos_dec = expect_position(request_typed(conn, Command::InquirePosition(Axis::Dec)).await?)?;
    let dec_sampled_at = Instant::now();

    *parameters.write().await = Some(MountParameters {
        cpr_ra,
        cpr_dec,
        tmr_freq,
        high_speed_ratio_ra: hsr_ra,
        high_speed_ratio_dec: hsr_dec,
        motor_board_version: board,
        ra_at_handshake_ticks: pos_ra,
        dec_at_handshake_ticks: pos_dec,
    });
    // A fresh conduit may be a power-cycled mount: whatever `:I` the
    // driver sent the old one says nothing about this firmware's period,
    // so forget it until the driver commands a rate again.
    step_periods.clear();
    let mut snap = snapshot.write().await;
    snap.ra.position_ticks = pos_ra;
    snap.ra.sampled_at = Some(ra_sampled_at);
    snap.dec.position_ticks = pos_dec;
    snap.dec.sampled_at = Some(dec_sampled_at);
    drop(snap);
    Ok(())
}

/// Halt on both axes (`:L1`, `:L2`, `:K1`), and say whether it took.
/// Used by both the `on_last_disconnect` hook (every `1→0` transition,
/// and every fresh conduit) and by `shutdown_teardown`.
///
/// Still log-and-continue per the hooks' infallible contract — a failed
/// command never propagates and never stops the rest of the sequence —
/// but the outcome is no longer thrown away. The shared crate watches
/// [`Connection::request`] for commands that never reached the wire,
/// which is blind to the case this function is the only witness to: the
/// mount answered `!XX` and refused. `SkywatcherCodec` returns raw
/// frames, so an error reply decodes as a perfectly good `Ok` response
/// one layer down, and only the typed decode in [`request_typed`] sees
/// it. Returning the verdict is what carries that out to the caller —
/// see [#1250](https://github.com/rusty-photon/rusty-photon/issues/1250).
///
/// Any failure means [`StateAssertion::NotAsserted`]: a stop that the
/// mount refused, or that never reached it, leaves axes that may still
/// be turning, and there is no partial credit for stopping one of two.
/// A [`StateAssertion::NotAsserted`] says only that the state could
/// not be asserted, never why. Every failure mode folds into it: an
/// `!XX` refusal, a reply that would not decode, a frame from some
/// other device. The specific error is logged at the point it is seen,
/// one `warn!` per command, and callers upstream read the verdict
/// alone — so nothing above this has to guess at a cause, and nothing
/// below it has to invent a taxonomy.
///
/// It also does not try to read the mount's error code and decide
/// which failures are benign. By the time this runs the handshake has
/// just had ten commands answered, so a mount that now cannot be
/// halted is a mount that is broken or is not the device we think it
/// is — the two cases where guessing "that one's probably fine" is
/// exactly wrong.
async fn safety_stop(conn: &Connection<SkywatcherCodec>) -> StateAssertion {
    let mut verdict = StateAssertion::Asserted;
    // Order matters: `:L` is the hammer (instant stop), `:K` is
    // graceful — issue the hammer first to guarantee motion stops
    // even if the graceful stop fails.
    for cmd in [
        Command::InstantStop(Axis::Ra),
        Command::InstantStop(Axis::Dec),
        Command::StopMotion(Axis::Ra),
    ] {
        if let Err(e) = request_typed(conn, cmd.clone()).await {
            warn!(
                command = ?cmd,
                error = %e,
                "safety stop wire command failed (continuing)"
            );
            verdict = verdict.and(StateAssertion::NotAsserted);
        }
    }
    // Report the verdict on the way out, including when nothing went
    // wrong. The per-command `warn!` above only fires on failure, so
    // without this line a halt that worked is invisible: on healthy
    // hardware the whole sequence leaves no trace at any default log
    // level, and confirming it ran at all means enabling wire tracing
    // on the transport crate and reading raw frames. An operator asking
    // "was the mount actually stopped?" should not have to do that.
    //
    // `info!` rather than `debug!`, against the usual preference for
    // `debug!`: the packaged units ship `RUST_LOG=info`, so `debug!`
    // would leave this invisible on every stock installation and the
    // operator no better off than before. This is the record that a
    // safety action affecting a moving telescope either did or did not
    // reach the hardware — the one class of event where "you had to
    // know to turn it on" is the wrong default. It fires once per start
    // and once per last-client disconnect, so it is bounded by session
    // count rather than by traffic.
    info!(verdict = ?verdict, "safety stop complete");
    verdict
}

/// Final shutdown teardown: safety-stop both axes, then clear the
/// parameter cache so a future `start()` on the same manager (cold
/// restart) does not see stale handshake data. All errors are
/// log-and-continue per the `Hooks::shutdown` infallible contract.
async fn shutdown_teardown(
    conn: &Connection<SkywatcherCodec>,
    parameters: Arc<RwLock<Option<MountParameters>>>,
) {
    if !safety_stop(conn).await.is_asserted() {
        // Nothing downstream can act on this — the lifecycle is ending
        // and `Hooks::shutdown` has no verdict to return — so the log
        // is the whole of the report, and it is worth an `error!`. The
        // mount may be left moving with no driver attached; the next
        // cold start asserts the state again and refuses to serve if
        // it cannot (see #1251), but nothing happens in between.
        error!(
            "the shutdown safety stop was not asserted \
             (see the warning above for the failing command); \
             the mount may still be moving"
        );
    }
    *parameters.write().await = None;
}

/// Background poll loop. Refreshes `:j` + `:f` on both axes at
/// `polling_interval` while not paused via [`PollPauseGuard`].
async fn poll_loop(
    ctx: WhileOpen<SkywatcherCodec>,
    snapshot: Arc<RwLock<MountSnapshot>>,
    step_periods: Arc<CommandedStepPeriods>,
    poll_pause_depth: Arc<AtomicU32>,
    polling_interval: Duration,
) {
    let mut ticker = interval(polling_interval);
    // Skip the immediate first tick — the handshake just populated the
    // snapshot; first poll should wait one interval.
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = ctx.cancelled() => {
                debug!("sky-watcher poll loop received cancellation");
                return;
            }
        }
        if poll_pause_depth.load(Ordering::SeqCst) > 0 {
            // Watcher owns the wire — skip this tick. The watcher's
            // own `poll_axes_now` updates the snapshot during the
            // slew so ASCOM readers still see fresh data.
            continue;
        }
        let mut snap = MountSnapshot::default();
        if let Err(e) = poll_axis_via_ctx(&ctx, &step_periods, Axis::Ra, &mut snap.ra).await {
            debug!("polling RA failed: {e}");
            continue;
        }
        if let Err(e) = poll_axis_via_ctx(&ctx, &step_periods, Axis::Dec, &mut snap.dec).await {
            debug!("polling Dec failed: {e}");
            continue;
        }
        snapshot.write().await.merge_newer(&snap);
    }
}

/// Request a command via the handshake-time `Connection` and decode it
/// to the protocol crate's typed [`Response`]. Used inside `handshake`
/// and `teardown` (which receive `&Connection<C>` per the `Hooks`
/// contract, not a `Session`).
///
/// Transport-side failures flow through the dedicated
/// [`SkywatcherCodecError::Transport`] variant rather than being
/// stringified into [`ProtocolError::FrameError`]; the device layer's
/// `From<SessionError<SkywatcherCodecError>> for StarAdvError` then
/// routes the inner [`TransportError`] through the canonical
/// `From<TransportError> for StarAdvError` impl a top-level
/// `SessionError::Transport(_)` uses, so a connect-time timeout gets
/// classified as `StarAdvError::Timeout` instead of collapsing to
/// `Protocol(FrameError("transport timeout after 5s"))` and surfacing
/// as the generic `INVALID_OPERATION`.
async fn request_typed(
    conn: &Connection<SkywatcherCodec>,
    cmd: Command,
) -> std::result::Result<Response, SkywatcherCodecError> {
    let bytes = conn
        .request(cmd.clone())
        .await
        .map_err(SkywatcherCodecError::from)?;
    decode_frame_for(&cmd, &bytes).map_err(SkywatcherCodecError::Protocol)
}

async fn poll_axis_via_ctx(
    ctx: &WhileOpen<SkywatcherCodec>,
    step_periods: &CommandedStepPeriods,
    axis: Axis,
    out: &mut AxisSnapshot,
) -> Result<()> {
    let pos_bytes = ctx
        .request(Command::InquirePosition(axis))
        .await
        .map_err(StarAdvError::from)?;
    let sampled_at = Instant::now();
    let pos = decode_frame_for(&Command::InquirePosition(axis), &pos_bytes)
        .map_err(StarAdvError::from)?;
    record_position(out, expect_position_runtime(pos)?, sampled_at);
    let status_bytes = ctx
        .request(Command::InquireStatus(axis))
        .await
        .map_err(StarAdvError::from)?;
    let status = decode_frame_for(&Command::InquireStatus(axis), &status_bytes)
        .map_err(StarAdvError::from)?;
    record_status(out, expect_status_runtime(status)?, step_periods.get(axis));
    Ok(())
}

async fn poll_axis_via_session(
    manager: &MountManager,
    session: &Session<SkywatcherCodec>,
    axis: Axis,
    out: &mut AxisSnapshot,
) -> Result<()> {
    let pos = manager
        .request(session, Command::InquirePosition(axis))
        .await?;
    let sampled_at = Instant::now();
    record_position(out, expect_position_runtime(pos)?, sampled_at);
    let status = manager
        .request(session, Command::InquireStatus(axis))
        .await?;
    record_status(
        out,
        expect_status_runtime(status)?,
        manager.step_periods.get(axis),
    );
    Ok(())
}

/// Store a `:j` reply and the instant it arrived. The stamp is taken
/// on receipt: the firmware latched the count somewhere inside the
/// round trip, so receipt is late by at most one round trip — a few
/// milliseconds of motion, against the poll interval it replaces.
const fn record_position(out: &mut AxisSnapshot, ticks: i32, sampled_at: Instant) {
    out.position_ticks = ticks;
    out.sampled_at = Some(sampled_at);
}

/// Store a `:f` reply, with the `:I` period in force, as the rate state
/// of the sample.
fn record_status(out: &mut AxisSnapshot, status: AxisStatus, step_period: u32) {
    out.running = status.motion.running;
    out.goto = status.mode == ModeKind::Goto;
    out.ccw = status.direction == Direction::Ccw;
    out.fast = status.speed == Speed::Fast;
    out.step_period = step_period;
    out.blocked = status.motion.blocked;
}

/// Convert any [`SkywatcherCodecError::Protocol`] arising from the
/// `:e1` round-trip into a [`SkywatcherCodecError::WrongDevice`]
/// carrying the configured port label and a human-readable reason.
/// Transport / `SkipExhausted` / pre-existing `WrongDevice` variants
/// pass through unchanged — those classifications are already correct
/// for their respective failure modes.
///
/// Used twice in the handshake — once on the `request_typed` result
/// (catches `FrameError` / `PayloadError` / `HexError` from the
/// codec's response decode, plus `MountError` from a `!X\r` reply)
/// and once on the `expect_u24` result (catches the
/// structurally-unreachable-in-practice non-U24 response case, where
/// `expect_u24` synthesises a `FrameError("expected U24, got X")`).
fn wrong_device_for_e1(err: SkywatcherCodecError, port_label: &str) -> SkywatcherCodecError {
    match err {
        SkywatcherCodecError::Protocol(pe) => {
            SkywatcherCodecError::wrong_device(port_label, format!("unexpected `:e1` reply: {pe}"))
        }
        other => other,
    }
}

fn expect_ack(r: Response) -> std::result::Result<(), SkywatcherCodecError> {
    match r {
        Response::Ack => Ok(()),
        other => Err(SkywatcherCodecError::Protocol(
            skywatcher_motor_protocol::ProtocolError::FrameError(format!(
                "expected Ack, got {other:?}"
            )),
        )),
    }
}

fn expect_u24(r: Response) -> std::result::Result<u32, SkywatcherCodecError> {
    match r {
        Response::U24(v) => Ok(v),
        other => Err(SkywatcherCodecError::Protocol(
            skywatcher_motor_protocol::ProtocolError::FrameError(format!(
                "expected U24, got {other:?}"
            )),
        )),
    }
}

fn expect_position(r: Response) -> std::result::Result<i32, SkywatcherCodecError> {
    match r {
        Response::Position(v) => Ok(v),
        other => Err(SkywatcherCodecError::Protocol(
            skywatcher_motor_protocol::ProtocolError::FrameError(format!(
                "expected Position, got {other:?}"
            )),
        )),
    }
}

fn expect_position_runtime(r: Response) -> Result<i32> {
    match r {
        Response::Position(v) => Ok(v),
        other => Err(StarAdvError::Transport(format!(
            "expected Position, got {other:?}"
        ))),
    }
}

fn expect_status_runtime(r: Response) -> Result<AxisStatus> {
    match r {
        Response::Status(s) => Ok(s),
        other => Err(StarAdvError::Transport(format!(
            "expected Status, got {other:?}"
        ))),
    }
}

/// Pre-validate any [`Command`] variant whose
/// [`Command::encode`](skywatcher_motor_protocol::Command::encode) is
/// fallible, so the codec layer never reaches the encode-error path.
///
/// Today the only fallible variants are [`Command::SetPosition`] and
/// [`Command::SetGotoTarget`], both of which call
/// [`encode_position`](skywatcher_motor_protocol::codec::encode_position)
/// on an `i32` tick value that must fit in signed-24-bit range
/// (`[POSITION_MIN, POSITION_MAX]` ≈ ±2²³ ≈ ±8.4M). For the `GTi`'s CPR
/// of ~3.6M, any in-range RA/Dec produces ticks well inside that
/// envelope; this check is the safety net for misconfigured park
/// targets, a future bug in coordinate-conversion math, or a different
/// CPR firmware variant.
fn validate_command_args(cmd: &Command) -> Result<()> {
    use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};
    let (axis, ticks, kind) = match cmd {
        Command::SetPosition { axis, ticks } => (*axis, *ticks, "SetPosition"),
        Command::SetGotoTarget { axis, ticks } => (*axis, *ticks, "SetGotoTarget"),
        _ => return Ok(()),
    };
    if (POSITION_MIN..=POSITION_MAX).contains(&ticks) {
        Ok(())
    } else {
        Err(StarAdvError::InvalidValue(format!(
            "{kind} {{ axis: {axis:?}, ticks: {ticks} }} is outside the signed-24-bit \
             encoder range [{POSITION_MIN}, {POSITION_MAX}]"
        )))
    }
}

#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Behaviour-level tests for the manager, driven through the mock
    //! transport factory. Race / refcount / rollback invariants are
    //! tested once for everyone in `rusty-photon-shared-transport` —
    //! they don't get re-tested here per the migration plan.

    use super::*;
    use tokio::sync::Mutex;
    use tokio::time::Instant;

    use crate::transport::mock::{CapturingMockFactory, MockMountState, MockTransportFactory};

    fn manager() -> Arc<MountManager> {
        MountManager::new(&Config::default(), Arc::new(MockTransportFactory))
    }

    /// Frames a single background poll cycle puts on the wire:
    /// `:j` + `:f` per axis, both axes.
    const FRAMES_PER_POLL_CYCLE: usize = 4;

    /// Upper bound on `:j`/`:f` frames that may still land *after*
    /// [`MountManager::pause_background_polling`] returns.
    ///
    /// The guard bumps a depth counter that `poll_loop` reads once, at
    /// the top of each iteration. A cycle already past that read runs
    /// to completion, so it can still emit whichever of its frames had
    /// not reached the wire yet. The pause can interleave anywhere
    /// after the read — including before the cycle's *first* frame,
    /// since acquiring the transport's command lock is itself an await
    /// point — so a whole cycle is the honest bound, not a partial
    /// one. Only one cycle is ever in flight (the poll loop is a
    /// single sequential task) and the next iteration re-reads the
    /// counter, so the residue cannot exceed one cycle on any machine
    /// at any load.
    const MAX_FRAMES_IN_FLIGHT_AT_PAUSE: usize = FRAMES_PER_POLL_CYCLE;

    /// Count `:j<axis>` / `:f<axis>` background-poll frames in a
    /// captured mock command log.
    fn poll_frames(log: &[Vec<u8>]) -> usize {
        log.iter()
            .filter(|f| {
                f.len() >= 3
                    && f[0] == b':'
                    && matches!(f[1], b'j' | b'f')
                    && matches!(f[2], b'1' | b'2')
            })
            .count()
    }

    /// Poll the captured command log until `want` holds for the
    /// `:j`/`:f` frame count, and return that count.
    ///
    /// Waiting for the condition rather than napping a fixed span and
    /// sampling once keeps the assertion load-independent: a slower
    /// machine takes longer to satisfy `want`, it does not turn a
    /// healthy run red. `what` names the condition for the panic
    /// message on timeout.
    async fn wait_for_poll_frames(
        state: &Arc<Mutex<MockMountState>>,
        what: &str,
        want: impl Fn(usize) -> bool,
    ) -> usize {
        // Generous relative to `TEST_POLL_INTERVAL`: a bound this
        // loose only ever trips on a genuinely
        // stalled poll loop, never on a busy runner. Measured on
        // tokio's clock, the same one `sleep` below advances — under a
        // paused/auto-advancing runtime `std::time::Instant` would
        // barely move while virtual time raced ahead, so the deadline
        // would never trip and a stalled poll loop would hang here
        // instead of failing.
        const DEADLINE: Duration = Duration::from_secs(10);
        const STEP: Duration = Duration::from_millis(5);

        let started = Instant::now();
        loop {
            let count = poll_frames(&state.lock().await.command_log);
            if want(count) {
                return count;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "timed out after {DEADLINE:?} waiting for {what}; \
                 :j/:f frame count stuck at {count}"
            );
            tokio::time::sleep(STEP).await;
        }
    }

    /// Assert the background poll loop stays off the wire for a whole
    /// window, tolerating only the bounded residue of a cycle that was
    /// already in flight when the guard was taken.
    ///
    /// `count_at_pause` is the frame count when the pause *began* — not
    /// what a previous window ended at. Across one continuously-held
    /// pause the allowance is one cycle in total, however many windows
    /// observe it, so successive calls covering the same pause must
    /// pass the same anchor.
    ///
    /// The window is the detector, so a longer one can only make a
    /// live poll loop easier to catch. Sampling once after a nap would
    /// do the opposite — on a loaded runner the sample can land
    /// before the loop next ticks, passing whether or not polling
    /// actually stopped.
    async fn assert_polling_stays_paused(
        state: &Arc<Mutex<MockMountState>>,
        count_at_pause: usize,
        context: &str,
    ) -> usize {
        // A fixed sample count, deliberately, rather than looping to a
        // wall-clock deadline: what detects a live poll loop is the
        // number of observations, and under load a deadline loop makes
        // *fewer* of them — weakening the detector exactly when the
        // flake this guards against is most likely. A fixed count
        // holds the observations constant and just takes longer.
        // Spans many `TEST_POLL_INTERVAL`s; a live poll loop blows the
        // budget within one.
        const SAMPLES: u32 = 100;
        const STEP: Duration = Duration::from_millis(5);

        let ceiling = count_at_pause + MAX_FRAMES_IN_FLIGHT_AT_PAUSE;
        let mut observed = count_at_pause;
        for _ in 0..SAMPLES {
            tokio::time::sleep(STEP).await;
            observed = poll_frames(&state.lock().await.command_log);
            assert!(
                observed <= ceiling,
                "{context}: background polling kept issuing :j/:f frames while the \
                 pause guard was held — {observed} frames seen, at most {ceiling} \
                 expected ({count_at_pause} at pause, plus at most \
                 {MAX_FRAMES_IN_FLIGHT_AT_PAUSE} from a cycle already in flight)"
            );
        }
        observed
    }

    /// Background poll interval these tests configure. Every window
    /// and deadline below is sized against it.
    const TEST_POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// A manager whose background poll loop ticks every
    /// [`TEST_POLL_INTERVAL`], plus a handle on the mock's captured
    /// command log.
    fn fast_polling_manager() -> (Arc<MountManager>, Arc<Mutex<MockMountState>>) {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let mut cfg = Config::default();
        // Exhaustive on purpose, rather than an `if let` on the
        // variant `Config::default()` happens to pick today: every
        // timing assumption in these helpers rests on this interval,
        // so a change of default transport must not be able to leave
        // it silently unset. A new variant becomes a compile error
        // here instead of a quietly weakened test.
        match &mut cfg.transport {
            TransportConfig::Usb(usb) => usb.polling_interval = TEST_POLL_INTERVAL,
            TransportConfig::Udp(udp) => udp.polling_interval = TEST_POLL_INTERVAL,
        }
        (MountManager::new(&cfg, Arc::new(factory)), state)
    }

    #[test]
    fn new_starts_unavailable() {
        let m = manager();
        assert!(!m.is_available());
    }

    #[tokio::test]
    async fn parameters_are_none_before_handshake() {
        let m = manager();
        assert!(m.parameters().await.is_none());
    }

    #[tokio::test]
    async fn snapshot_is_default_before_polling() {
        let m = manager();
        let snap = m.snapshot().await;
        assert_eq!(snap.ra.position_ticks, 0);
        assert_eq!(snap.dec.position_ticks, 0);
        assert!(!snap.ra.running);
        assert!(!snap.dec.running);
    }

    #[tokio::test]
    async fn acquire_runs_handshake_and_seeds_parameter_cache() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        assert!(m.is_available());
        let params = m.parameters().await.expect("handshake populates cache");
        assert_eq!(params.cpr_ra, 0x0037_5F00);
        assert_eq!(params.cpr_dec, 0x002C_4C00);
        assert_eq!(params.tmr_freq, 0x00F4_2400);
        assert_eq!(params.motor_board_version, 0x000C_3003);
        session.close().await.unwrap();
    }

    fn gti_parameters() -> MountParameters {
        MountParameters {
            cpr_ra: 0x0037_5F00,
            cpr_dec: 0x002C_4C00,
            tmr_freq: 0x00F4_2400,
            ..Default::default()
        }
    }

    #[test]
    fn sidereal_step_period_ra_uses_the_ra_cpr() {
        assert_eq!(gti_parameters().sidereal_step_period_ra(), 379_912);
    }

    #[test]
    fn sidereal_step_period_dec_uses_the_dec_cpr() {
        assert_eq!(gti_parameters().sidereal_step_period_dec(), 474_890);
    }

    #[tokio::test]
    async fn close_marks_unavailable_but_keeps_parameter_cache() {
        // In `LazyAcquire` mode (the default this test exercises),
        // `Session::close` closes the underlying port and the next
        // `acquire()` re-runs the handshake to repopulate the cache —
        // so leaving the cache populated across close is harmless. In
        // `ServiceLifetime` mode the cache MUST survive last-disconnect
        // because the next acquire is a refcount-bump with no
        // handshake; clearing here would break post-acquire hooks like
        // `seed_after_connect` that read `parameters()`. The on-the-wire
        // safety stop (`:L1`, `:L2`, `:K1`) is exercised by
        // [`teardown_sends_halt_sequence`]; only `shutdown_teardown`
        // (run by `Hooks::shutdown` on a final `SharedTransport::shutdown`)
        // clears the cache.
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        assert!(m.is_available());
        session.close().await.unwrap();
        assert!(!m.is_available());
        assert!(
            m.parameters().await.is_some(),
            "parameter cache must survive `on_last_disconnect` so a \
             ServiceLifetime re-acquire still sees CPR / TMR_Freq"
        );
    }

    #[tokio::test]
    async fn send_round_trips_typed_response() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        let r = m
            .send(&session, Command::InquireCpr(Axis::Ra))
            .await
            .unwrap();
        assert_eq!(r, Response::U24(0x0037_5F00));
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn poll_axes_now_returns_fresh_snapshot_and_updates_cache() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        let polled = m.poll_axes_now(&session).await.unwrap();
        let cached = m.snapshot().await;
        assert_eq!(polled.ra.position_ticks, cached.ra.position_ticks);
        assert_eq!(polled.dec.position_ticks, cached.dec.position_ticks);
        assert_eq!(polled.ra.running, cached.ra.running);
        assert_eq!(polled.dec.running, cached.dec.running);
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn pause_background_polling_stops_wire_traffic_and_resumes_on_drop() {
        // Hold a guard, watch for `:j`/`:f` traffic across a window
        // during the hold, resume after drop. Mirrors the legacy
        // `transport_manager.rs` test.
        let (m, state) = fast_polling_manager();
        let session = m.transport().acquire().await.unwrap();

        wait_for_poll_frames(&state, "background polling to issue a full cycle", |c| {
            c >= FRAMES_PER_POLL_CYCLE
        })
        .await;

        let guard = m.pause_background_polling();
        let count_at_pause = poll_frames(&state.lock().await.command_log);
        let count_during_pause =
            assert_polling_stays_paused(&state, count_at_pause, "guard held").await;

        drop(guard);
        wait_for_poll_frames(&state, "polling to resume after guard drop", |c| {
            c > count_during_pause
        })
        .await;
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn pause_background_polling_is_refcounted_across_overlapping_guards() {
        let (m, state) = fast_polling_manager();
        let session = m.transport().acquire().await.unwrap();

        let outer = m.pause_background_polling();
        let inner = m.pause_background_polling();
        let count_at_pause = poll_frames(&state.lock().await.command_log);
        assert_polling_stays_paused(&state, count_at_pause, "depth=2").await;

        drop(inner);
        // Still anchored on `count_at_pause`, not on what the previous
        // window ended at: `outer` has held the pause continuously
        // since then, so the whole stretch gets *one* in-flight
        // cycle's allowance between it. Re-anchoring per window would
        // ratchet the ceiling up by another cycle each time and let a
        // one-cycle leak after the inner drop pass unnoticed.
        let count_at_depth_1 = assert_polling_stays_paused(
            &state,
            count_at_pause,
            "depth=1 after inner drop, outer still alive",
        )
        .await;

        drop(outer);
        wait_for_poll_frames(&state, "polling to resume once depth returns to 0", |c| {
            c > count_at_depth_1
        })
        .await;
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn polling_interval_for_watcher_matches_usb_config() {
        let mut cfg = Config::default();
        if let TransportConfig::Usb(usb) = &mut cfg.transport {
            usb.polling_interval = Duration::from_millis(123);
        }
        let m = MountManager::new(&cfg, Arc::new(MockTransportFactory));
        assert_eq!(m.polling_interval_for_watcher(), Duration::from_millis(123));
    }

    #[tokio::test]
    async fn polling_interval_for_watcher_matches_udp_config() {
        let cfg = Config {
            transport: TransportConfig::Udp(crate::config::UdpConfig {
                polling_interval: Duration::from_millis(77),
                ..crate::config::UdpConfig::default()
            }),
            ..Config::default()
        };
        let m = MountManager::new(&cfg, Arc::new(MockTransportFactory));
        assert_eq!(m.polling_interval_for_watcher(), Duration::from_millis(77));
    }

    #[tokio::test]
    async fn seed_positions_update_snapshot() {
        let m = manager();
        m.seed_ra_position(12_345).await;
        m.seed_dec_position(-6_789).await;
        let snap = m.snapshot().await;
        assert_eq!(snap.ra.position_ticks, 12_345);
        assert_eq!(snap.dec.position_ticks, -6_789);
    }

    #[tokio::test]
    async fn command_timeout_returns_configured_value() {
        // `command_timeout` is a public accessor used by hand-rolled
        // callers (the BDD harness sometimes reaches for it). Verify
        // it round-trips the configured value across both transports.
        let mut cfg = Config::default();
        if let TransportConfig::Usb(usb) = &mut cfg.transport {
            usb.command_timeout = Duration::from_millis(123);
        }
        let m = MountManager::new(&cfg, Arc::new(MockTransportFactory));
        assert_eq!(m.command_timeout(), Duration::from_millis(123));

        let cfg = Config {
            transport: TransportConfig::Udp(crate::config::UdpConfig {
                command_timeout: Duration::from_millis(456),
                ..crate::config::UdpConfig::default()
            }),
            ..Config::default()
        };
        let m = MountManager::new(&cfg, Arc::new(MockTransportFactory));
        assert_eq!(m.command_timeout(), Duration::from_millis(456));
    }

    #[test]
    fn expect_ack_rejects_non_ack_responses() {
        // The `expect_*` helpers underpin the handshake's contract
        // that the mount returns the right response shape per query;
        // wrong-shape replies must bubble out as a codec/protocol
        // error rather than silently being mis-decoded.
        assert!(matches!(expect_ack(Response::Ack), Ok(()),));
        let err = expect_ack(Response::U24(0)).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
        let err = expect_ack(Response::Position(0)).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn expect_u24_rejects_non_u24_responses() {
        assert_eq!(expect_u24(Response::U24(42)).unwrap(), 42);
        let err = expect_u24(Response::Ack).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
        let err = expect_u24(Response::Position(0)).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn expect_position_rejects_non_position_responses() {
        assert_eq!(expect_position(Response::Position(-7)).unwrap(), -7);
        let err = expect_position(Response::Ack).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
        let err = expect_position(Response::U24(0)).unwrap_err();
        assert!(matches!(err, SkywatcherCodecError::Protocol(_)));
    }

    #[test]
    fn expect_position_runtime_rejects_non_position() {
        assert_eq!(expect_position_runtime(Response::Position(42)).unwrap(), 42);
        let err = expect_position_runtime(Response::Ack).unwrap_err();
        assert!(matches!(err, StarAdvError::Transport(_)));
    }

    #[test]
    fn expect_status_runtime_rejects_non_status() {
        // Construct a default AxisStatus so we can ensure the round-trip
        // works on the happy path before exercising the error branch.
        let status = AxisStatus {
            mode: ModeKind::Tracking,
            direction: skywatcher_motor_protocol::Direction::Cw,
            speed: skywatcher_motor_protocol::Speed::Slow,
            motion: skywatcher_motor_protocol::MotionFlags {
                running: false,
                blocked: false,
            },
            init: skywatcher_motor_protocol::InitFlags {
                initialized: true,
                level_switch_on: false,
            },
        };
        let s = expect_status_runtime(Response::Status(status)).unwrap();
        assert!(s.init.initialized);
        let err = expect_status_runtime(Response::Ack).unwrap_err();
        assert!(matches!(err, StarAdvError::Transport(_)));
    }

    #[tokio::test]
    async fn teardown_sends_halt_sequence() {
        // After session.close, the teardown hook should have issued
        // :L1, :L2, :K1 in order. Use CapturingMockFactory to inspect.
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let m = MountManager::new(&Config::default(), Arc::new(factory));
        let session = m.transport().acquire().await.unwrap();
        session.close().await.unwrap();
        let log = state.lock().await.command_log.clone();
        // Find the teardown commands (they come last, so they are walked
        // in reverse); the trailing reverse restores forward order.
        let mut forward: Vec<&[u8]> = log
            .iter()
            .rev()
            .take_while(|f| f.starts_with(b":L") || f.starts_with(b":K"))
            .map(std::vec::Vec::as_slice)
            .collect();
        forward.reverse();
        assert!(
            forward.iter().any(|f| f.starts_with(b":L1")),
            "teardown should issue :L1; got {forward:?}"
        );
        assert!(
            forward.iter().any(|f| f.starts_with(b":L2")),
            "teardown should issue :L2; got {forward:?}"
        );
        assert!(
            forward.iter().any(|f| f.starts_with(b":K1")),
            "teardown should issue :K1; got {forward:?}"
        );
    }

    // ========================================================================
    // Handshake transport-error classification: a TransportError surfaced
    // through the handshake hook (which returns Result<_, SkywatcherCodecError>)
    // must reach the device layer with its variant preserved, so a
    // connect-time timeout / EOF / open failure maps to the structured
    // StarAdvError::Timeout / ConnectionFailed instead of collapsing to
    // Protocol(FrameError("transport timeout after …")) and being routed
    // through the generic INVALID_OPERATION arm. See PR #280 for the
    // bug class.
    // ========================================================================

    use async_trait::async_trait;
    use rusty_photon_shared_transport::{FrameTransport, TransportError, TransportFactory};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Transport whose first `recv_frame` returns the configured
    /// `TransportError` variant (consumed once via the `AtomicBool` gate);
    /// subsequent operations error with `Eof` so the test fails fast if
    /// the manager unexpectedly retries.
    struct FailingRecvTransport {
        fail_with: std::sync::Mutex<Option<TransportError>>,
        consumed: AtomicBool,
    }

    #[async_trait]
    impl FrameTransport for FailingRecvTransport {
        async fn send_frame(&mut self, _bytes: &[u8]) -> std::result::Result<(), TransportError> {
            Ok(())
        }

        async fn recv_frame(
            &mut self,
            _buf: &mut Vec<u8>,
        ) -> std::result::Result<(), TransportError> {
            if !self.consumed.swap(true, Ordering::SeqCst) {
                let mut slot = self.fail_with.lock().unwrap();
                if let Some(err) = slot.take() {
                    return Err(err);
                }
            }
            Err(TransportError::Eof)
        }
    }

    struct FailingRecvFactory {
        fail_with: std::sync::Mutex<Option<TransportError>>,
    }

    #[async_trait]
    impl TransportFactory for FailingRecvFactory {
        async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
            let err = self.fail_with.lock().unwrap().take();
            Ok(Box::new(FailingRecvTransport {
                fail_with: std::sync::Mutex::new(err),
                consumed: AtomicBool::new(false),
            }))
        }
    }

    fn make_manager_with_failing_recv(err: TransportError) -> Arc<MountManager> {
        let factory = Arc::new(FailingRecvFactory {
            fail_with: std::sync::Mutex::new(Some(err)),
        });
        MountManager::new(&Config::default(), factory)
    }

    #[tokio::test]
    async fn handshake_timeout_surfaces_as_staradv_timeout_not_protocol() {
        // A read timeout during the first handshake command (`:F1`)
        // must propagate as `StarAdvError::Timeout`, not the generic
        // `Protocol(FrameError("transport timeout after …"))` the
        // pre-PR-280 string-collapse would produce.
        let manager = make_manager_with_failing_recv(TransportError::Timeout(
            std::time::Duration::from_secs(5),
        ));
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake timeout must surface as Err");
        let mapped = StarAdvError::from(err);
        match mapped {
            StarAdvError::Timeout(s) => assert!(s.contains('5')),
            other => panic!("expected StarAdvError::Timeout, got {other:?}"),
        }
        assert!(
            !manager.is_available(),
            "RollbackGuard should roll the refcount back on handshake failure"
        );
    }

    #[tokio::test]
    async fn handshake_eof_surfaces_as_staradv_communication_connection_closed() {
        // EOF mid-handshake → device-layer `Communication("Connection closed")`
        // (the shared `From<TransportError>` mapping), not
        // `Protocol(FrameError("connection closed"))`.
        let manager = make_manager_with_failing_recv(TransportError::Eof);
        let err = manager
            .transport()
            .acquire()
            .await
            .expect_err("handshake EOF must surface as Err");
        let mapped = StarAdvError::from(err);
        match mapped {
            StarAdvError::Communication(s) => assert!(s.contains("Connection closed")),
            other => panic!("expected StarAdvError::Communication, got {other:?}"),
        }
        assert!(
            !manager.is_available(),
            "RollbackGuard should roll the refcount back on handshake failure"
        );
    }

    // ========================================================================
    // Pre-encode validation: SetPosition / SetGotoTarget commands whose
    // `ticks` falls outside the signed-24-bit encoder range must be rejected
    // at `MountManager::send` with a structured `InvalidValue` error, *not*
    // reach the codec's `encode` (where the prior `.expect(...)` would have
    // panicked). See PR #285 Copilot review on codec.rs.
    // ========================================================================

    use skywatcher_motor_protocol::codec::{POSITION_MAX, POSITION_MIN};

    #[tokio::test]
    async fn send_set_position_with_in_range_ticks_succeeds() {
        // Sanity: the validation is permissive at the boundaries —
        // POSITION_MAX is accepted (and the mock factory acks any
        // `:E` write, so the round-trip completes).
        let manager = manager();
        let session = manager.transport().acquire().await.unwrap();
        let resp = manager
            .send(
                &session,
                Command::SetPosition {
                    axis: Axis::Ra,
                    ticks: POSITION_MAX,
                },
            )
            .await
            .unwrap();
        assert!(matches!(resp, Response::Ack));
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_set_position_with_overflow_ticks_returns_invalid_value_without_touching_wire() {
        // `POSITION_MAX + 1` is just past the encoder's signed-24-bit
        // ceiling. `MountManager::send` rejects it before calling
        // `Codec::encode`, so the wire is never touched and the
        // codec's encode-error path is unreachable in practice.
        let manager = manager();
        let session = manager.transport().acquire().await.unwrap();
        let err = manager
            .send(
                &session,
                Command::SetPosition {
                    axis: Axis::Ra,
                    ticks: POSITION_MAX + 1,
                },
            )
            .await
            .expect_err("out-of-range ticks must be rejected before wire");
        match err {
            StarAdvError::InvalidValue(s) => {
                assert!(s.contains("SetPosition"));
                assert!(s.contains("signed-24-bit"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_set_goto_target_with_underflow_ticks_returns_invalid_value() {
        // Mirror for the other fallible variant. `POSITION_MIN - 1`
        // is the i24 floor's underflow.
        let manager = manager();
        let session = manager.transport().acquire().await.unwrap();
        let err = manager
            .send(
                &session,
                Command::SetGotoTarget {
                    axis: Axis::Dec,
                    ticks: POSITION_MIN - 1,
                },
            )
            .await
            .expect_err("out-of-range ticks must be rejected before wire");
        match err {
            StarAdvError::InvalidValue(s) => {
                assert!(s.contains("SetGotoTarget"));
                assert!(s.contains("signed-24-bit"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
        session.close().await.unwrap();
    }

    #[test]
    fn validate_command_args_passes_non_position_commands_through() {
        // The validator only touches the two fallible-encode variants;
        // every other command shape (Initialize, Status reads,
        // StopMotion, …) must pass through untouched.
        for cmd in [
            Command::Initialize(Axis::Ra),
            Command::InquirePosition(Axis::Dec),
            Command::StopMotion(Axis::Ra),
            Command::InquireCpr(Axis::Dec),
        ] {
            assert!(validate_command_args(&cmd).is_ok(), "rejected {cmd:?}");
        }
    }

    // ========================================================================
    // Issue #254: `:e1` runs first and gates the rest of the handshake.
    //
    // On a wrong-device handshake the driver must send exactly one frame
    // (`:e1\r`), then bail out with a structured `StarAdvError::WrongDevice`
    // carrying the configured port label and a reason naming the failure mode
    // — no `:F1` / `:F2` / `:a*` / `:b*` / `:g*` / `:j*` should reach a device
    // that isn't a Sky-Watcher motor controller.
    // ========================================================================

    #[tokio::test]
    async fn acquire_issues_e1_as_the_very_first_wire_frame() {
        // The first frame in the command log must be `:e1\r`. Everything
        // else (`:F*`, `:a*`, `:b*`, `:g*`, `:j*`) follows only after the
        // device has been identified.
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let m = MountManager::new(&Config::default(), Arc::new(factory));
        let session = m.transport().acquire().await.unwrap();
        let log = state.lock().await.command_log.clone();
        assert!(!log.is_empty(), "handshake produced no wire frames");
        assert_eq!(
            log[0],
            b":e1\r",
            "first wire frame must be `:e1`; got {:?}",
            std::str::from_utf8(&log[0]).unwrap_or("<non-utf8>")
        );
        // Sanity: the rest of the handshake still ran (`:F1` shows up
        // somewhere after `:e1`), so reordering didn't drop commands.
        assert!(
            log.iter().any(|f| f == b":F1\r"),
            "`:F1` missing from handshake log: {log:?}"
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_rejects_unknown_mount_type_byte_without_issuing_mount_commands() {
        // Seed a motor-board-version whose type byte (low byte) is outside
        // the `MountType` whitelist before the handshake reaches the wire.
        // `0xFF` is a plausible "wrong device" byte: no Sky-Watcher motor
        // controller reports it (the documented IDs top out at `0x06` for
        // the EQ family and `0x82` for AZ-GTi).
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        state.lock().await.motor_board_version = 0x000C_30FF;
        let m = MountManager::new(&Config::default(), Arc::new(factory));
        let err = m
            .transport()
            .acquire()
            .await
            .expect_err("acquire must reject a wrong-device handshake");
        let mapped = StarAdvError::from(err);
        match mapped {
            StarAdvError::WrongDevice { port, reason } => {
                // Default `UsbConfig` port path is documented in
                // `config.rs::UsbConfig::default` (platform-dependent).
                #[cfg(not(windows))]
                assert_eq!(port, "/dev/ttyACM0", "wrong port in diagnostic");
                #[cfg(windows)]
                assert_eq!(port, "COM3", "wrong port in diagnostic");
                assert!(
                    reason.contains("0xFF"),
                    "reason must quote the rejected byte; got {reason:?}"
                );
                assert!(
                    reason.contains("Sky-Watcher"),
                    "reason must call out the whitelist mismatch; got {reason:?}"
                );
            }
            other => panic!("expected WrongDevice, got {other:?}"),
        }
        // The driver must NOT have issued any of the mount-specific init
        // commands. `:e1\r` is allowed (and required); everything else
        // would have leaked a write to the wrong device.
        let log = state.lock().await.command_log.clone();
        assert_eq!(
            log.len(),
            1,
            "expected exactly one wire frame after wrong-device rejection, got {log:?}"
        );
        assert_eq!(log[0], b":e1\r");
        assert!(
            !m.is_available(),
            "RollbackGuard should roll the refcount back on handshake failure"
        );
    }

    #[tokio::test]
    async fn handshake_rejects_e1_with_wrong_payload_length_as_wrong_device() {
        // Build a transport whose first `recv_frame` returns an `=...\r`
        // frame that is structurally valid (the codec accepts it) but
        // whose payload is the wrong length for the `:e1` U24 decoder:
        // 3 hex bytes instead of the required 6. `Response::decode`
        // surfaces this as `Err(ProtocolError::PayloadError(...))`. The
        // driver must reclassify it as `WrongDevice` so the operator
        // sees an actionable diagnostic instead of a generic
        // `INVALID_OPERATION` codec error.
        struct FakeTransport {
            served: AtomicBool,
        }

        #[async_trait]
        impl FrameTransport for FakeTransport {
            async fn send_frame(
                &mut self,
                _bytes: &[u8],
            ) -> std::result::Result<(), TransportError> {
                Ok(())
            }
            async fn recv_frame(
                &mut self,
                buf: &mut Vec<u8>,
            ) -> std::result::Result<(), TransportError> {
                if self.served.swap(true, Ordering::SeqCst) {
                    Err(TransportError::Eof)
                } else {
                    buf.clear();
                    buf.extend_from_slice(b"=100\r");
                    Ok(())
                }
            }
        }

        struct FakeFactory;

        #[async_trait]
        impl TransportFactory for FakeFactory {
            async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
                Ok(Box::new(FakeTransport {
                    served: AtomicBool::new(false),
                }))
            }
        }

        let m = MountManager::new(&Config::default(), Arc::new(FakeFactory));
        let err = m
            .transport()
            .acquire()
            .await
            .expect_err("malformed `:e1` reply must reject");
        let mapped = StarAdvError::from(err);
        match mapped {
            StarAdvError::WrongDevice { port, reason } => {
                // Default `UsbConfig` port path is platform-dependent (see
                // `config.rs::UsbConfig::default`).
                #[cfg(not(windows))]
                assert_eq!(port, "/dev/ttyACM0");
                #[cfg(windows)]
                assert_eq!(port, "COM3");
                // The path is unambiguous: `Response::decode` for
                // `InquireMotorBoardVersion` returns
                // `Err(ProtocolError::PayloadError("expected 6-hex-byte
                // u24 payload, got 3 bytes"))` for the `=100\r` reply,
                // which `wrong_device_for_e1` reclassifies as
                // `WrongDevice` with reason `"unexpected `:e1` reply:
                // payload error: ..."`. There is no "non-U24 success
                // reply" branch — that path was structurally
                // unreachable and was removed in the 5ccfcd1 refactor.
                assert!(
                    reason.contains("unexpected") && reason.contains("payload error"),
                    "reason should describe the PayloadError → WrongDevice path; got {reason:?}"
                );
            }
            other => panic!("expected WrongDevice, got {other:?}"),
        }
        assert!(
            !m.is_available(),
            "RollbackGuard should roll the refcount back on wrong-device failure"
        );
    }

    #[tokio::test]
    async fn handshake_rejects_mount_error_reply_to_e1_as_wrong_device() {
        // A device that speaks `:`/`!` framing but doesn't recognise
        // `:e` replies `!0\r` (UnknownCommand). The frame is
        // structurally valid (it is decodable as
        // `ProtocolError::MountError(UnknownCommand)`), so this is a
        // distinct path from the wrong-payload-length case in
        // `handshake_rejects_e1_with_wrong_payload_length_as_wrong_device`.
        // Still wrong-device — a real Sky-Watcher controller supports
        // `:e` from the protocol spec — but the diagnostic shouldn't
        // call this "malformed"; it's an unexpected (but well-formed)
        // reply.
        struct MountErrorTransport {
            served: AtomicBool,
        }

        #[async_trait]
        impl FrameTransport for MountErrorTransport {
            async fn send_frame(
                &mut self,
                _bytes: &[u8],
            ) -> std::result::Result<(), TransportError> {
                Ok(())
            }
            async fn recv_frame(
                &mut self,
                buf: &mut Vec<u8>,
            ) -> std::result::Result<(), TransportError> {
                if self.served.swap(true, Ordering::SeqCst) {
                    Err(TransportError::Eof)
                } else {
                    buf.clear();
                    buf.extend_from_slice(b"!0\r");
                    Ok(())
                }
            }
        }

        struct MountErrorFactory;

        #[async_trait]
        impl TransportFactory for MountErrorFactory {
            async fn open(&self) -> std::result::Result<Box<dyn FrameTransport>, TransportError> {
                Ok(Box::new(MountErrorTransport {
                    served: AtomicBool::new(false),
                }))
            }
        }

        let m = MountManager::new(&Config::default(), Arc::new(MountErrorFactory));
        let err = m
            .transport()
            .acquire()
            .await
            .expect_err("a `!0\\r` reply to `:e1` must reject as wrong-device");
        let mapped = StarAdvError::from(err);
        match mapped {
            StarAdvError::WrongDevice { port, reason } => {
                // Default `UsbConfig` port path is platform-dependent (see
                // `config.rs::UsbConfig::default`).
                #[cfg(not(windows))]
                assert_eq!(port, "/dev/ttyACM0");
                #[cfg(windows)]
                assert_eq!(port, "COM3");
                // The reason carries the underlying ProtocolError
                // stringification — `MountError(UnknownCommand)` formats
                // as "mount error: UnknownCommand" per its `thiserror`
                // attribute. The wrapper prefix is "unexpected", NOT
                // "malformed" — a `!X\r` frame is well-formed; it's the
                // *content* that's wrong for our query.
                assert!(
                    reason.contains("unexpected"),
                    "reason should use the broader 'unexpected' wording, not 'malformed'; got {reason:?}"
                );
                assert!(
                    reason.contains("mount error") || reason.contains("UnknownCommand"),
                    "reason should quote the underlying ProtocolError; got {reason:?}"
                );
            }
            other => panic!("expected WrongDevice, got {other:?}"),
        }
        // And the top-level Display string must not contradict — it
        // says "unexpected data", not "malformed data".
        let staradv = StarAdvError::WrongDevice {
            port: "/dev/ttyACM0".into(),
            reason: "unexpected `:e1` reply: mount error: UnknownCommand".into(),
        };
        assert!(
            staradv.to_string().contains("returned unexpected data"),
            "WrongDevice Display must say 'unexpected', not 'malformed'; got: {staradv}"
        );
    }

    #[tokio::test]
    async fn wrong_device_diagnostic_quotes_the_configured_port() {
        // The diagnostic is a smell-test for an operator who just pointed
        // the driver at the wrong transport target (serial port or UDP
        // host) — the message must name *their* configured port, not a
        // hardcoded default, so the verify-the-port hint is actionable.
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        state.lock().await.motor_board_version = 0x000C_3099;
        let mut cfg = Config::default();
        if let TransportConfig::Usb(usb) = &mut cfg.transport {
            usb.port = "/dev/serial/by-id/usb-Foo_Bar-port0".into();
        }
        let m = MountManager::new(&cfg, Arc::new(factory));
        let err = m.transport().acquire().await.expect_err("reject");
        let ascom: ascom_alpaca::ASCOMError = StarAdvError::from(err).into();
        // The to-string is what an ASCOM client surfaces to the operator
        // — assert the actionable bits are in it.
        let msg = ascom.message.to_string();
        assert!(
            msg.contains("/dev/serial/by-id/usb-Foo_Bar-port0"),
            "port path missing from ASCOM error message: {msg}"
        );
        assert!(
            msg.contains("Sky-Watcher motor controller"),
            "wrong-device hint missing: {msg}"
        );
        assert!(
            msg.contains("wrong device"),
            "wrong-device hypothesis missing: {msg}"
        );
        assert!(
            msg.contains("transport endpoint"),
            "verify-the-port hint missing: {msg}"
        );
    }

    /// A mount that answers the startup halt with `!XX` must stop the
    /// driver coming up — the case behind
    /// [#1250](https://github.com/rusty-photon/rusty-photon/issues/1250).
    ///
    /// The refusal is invisible below this layer: `SkywatcherCodec`
    /// hands back the `!0\r` frame as a perfectly good response, so
    /// `Connection::request` returns `Ok` and the shared crate's
    /// wire-failure counter never moves. Only `safety_stop`'s typed
    /// decode sees it, and only its verdict carries it out.
    #[tokio::test]
    async fn a_mount_that_refuses_the_startup_halt_fails_the_start() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        // `:L` only: the handshake uses none of it, so the mount
        // answers every init command and then refuses the halt.
        state.lock().await.fail_command = Some(b'L');
        let manager = MountManager::new(&Config::default(), Arc::new(factory));

        let err = manager.transport().start().await.unwrap_err();

        // The premise: this is a refusal *after* a clean handshake, not
        // a wrong device that never got that far.
        let log = state.lock().await.command_log.clone();
        assert!(
            log.iter().any(|f| f.starts_with(b":e1")),
            "the identity probe must have run: {log:?}"
        );
        assert!(
            log.iter().any(|f| f.starts_with(b":j2")),
            "and the whole handshake with it: {log:?}"
        );
        assert!(
            log.iter().any(|f| f.starts_with(b":L1")),
            "and the halt must have been attempted: {log:?}"
        );

        let msg = format!("{err}");
        assert!(
            msg.contains("not asserted"),
            "the start must fail saying the state was not asserted, got: {msg}"
        );
        // The refusal is *this* test's cause, but it is not the only
        // one that folds to `NotAsserted` — a garbled reply does too —
        // so the message must not name it. `safety_stop`'s own
        // per-command `warn!` is where the specific error is reported.
        assert!(
            !msg.contains("refus"),
            "and must not claim to know which failure it was, got: {msg}"
        );
        assert!(
            !manager.transport().is_available(),
            "and must not advertise a mount whose halt it could not assert"
        );
    }

    /// A mount that refuses the *shutdown* halt gets the loud log and
    /// nothing else: the lifecycle is ending, `Hooks::shutdown` has no
    /// verdict to return, and the teardown still has to finish. The
    /// next cold start is what re-asserts the state (#1251).
    ///
    /// Clearing the parameter cache is the observable proof the
    /// teardown ran past the refusal rather than bailing at it.
    #[tokio::test]
    async fn a_mount_that_refuses_the_shutdown_halt_still_finishes_teardown() {
        let factory = CapturingMockFactory::new();
        let state = Arc::clone(&factory.state);
        let manager = MountManager::new(&Config::default(), Arc::new(factory));

        // Start healthy — the refusal has to come after the handshake
        // has cached parameters, or there is nothing to clear.
        manager.transport().start().await.unwrap();
        assert!(
            manager.parameters().await.is_some(),
            "the handshake must have cached parameters before the shutdown"
        );

        // Now the mount refuses `:L`. `:K` still answers, so this is a
        // partial refusal — which folds to not-asserted all the same.
        state.lock().await.fail_command = Some(b'L');

        manager.transport().shutdown().await.unwrap();

        assert!(
            manager.parameters().await.is_none(),
            "the teardown must clear the cache even when the halt was refused"
        );
        // The premise, not an aside: `fail_command = Some(b'L')` makes
        // the mock answer `:L1` with `!0`, so seeing `:L1` on the wire
        // is seeing the refusal happen. Without it this test would pass
        // just as well on a halt the mount accepted, and would be
        // asserting nothing about the refusal path at all. The `error!`
        // itself is a log side-effect and deliberately not asserted.
        let log = state.lock().await.command_log.clone();
        assert!(
            log.iter().any(|f| f.starts_with(b":L1")),
            "the refused halt must have gone out: {log:?}"
        );
        assert!(
            log.iter().any(|f| f.starts_with(b":K1")),
            "and the sequence must continue past it, not stop at the first refusal: {log:?}"
        );
    }

    // ---------- Projecting a poll sample to the read instant (issue #1334) ----------

    /// `GTi` timer frequency and a step period that divides it evenly, so
    /// the expected rates are exact: 16 MHz / 400 000 = 40 steps/s.
    const TMR: u32 = 16_000_000;
    const PERIOD_40_PER_SEC: u32 = 400_000;
    const HSR: u32 = 32;
    const NO_CAP: Duration = Duration::from_secs(3600);

    fn tracking_sample(at: Instant) -> AxisSnapshot {
        AxisSnapshot {
            position_ticks: 1_000,
            sampled_at: Some(at),
            running: true,
            step_period: PERIOD_40_PER_SEC,
            ..AxisSnapshot::default()
        }
    }

    #[test]
    fn a_tracking_axis_turns_at_tmr_freq_over_the_commanded_period() {
        let s = tracking_sample(Instant::now());
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 40.0);
    }

    #[test]
    fn a_ccw_tracking_axis_counts_down() {
        let s = AxisSnapshot {
            ccw: true,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), -40.0);
    }

    #[test]
    fn the_fast_regime_multiplies_the_rate_by_the_high_speed_ratio() {
        let s = AxisSnapshot {
            fast: true,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 1_280.0);
    }

    #[test]
    fn the_rate_follows_whatever_period_was_commanded() {
        // Not tied to sidereal: a pulse-shifted or any future tracking
        // rate is just another `:I` period.
        let s = AxisSnapshot {
            step_period: PERIOD_40_PER_SEC * 2,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 20.0);
    }

    #[test]
    fn a_stopped_axis_has_no_rate() {
        let s = AxisSnapshot {
            running: false,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 0.0);
    }

    #[test]
    fn a_goto_axis_is_not_given_a_rate() {
        let s = AxisSnapshot {
            goto: true,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 0.0);
    }

    #[test]
    fn an_unknown_period_gives_no_rate() {
        let s = AxisSnapshot {
            step_period: 0,
            ..tracking_sample(Instant::now())
        };
        assert_eq!(s.rate_ticks_per_sec(TMR, HSR), 0.0);
    }

    #[test]
    fn projection_advances_a_tracking_sample_by_rate_times_age() {
        let t0 = Instant::now();
        let s = tracking_sample(t0);
        let p = s.projected_to(t0 + Duration::from_millis(150), TMR, HSR, NO_CAP);
        assert_eq!(p.position_ticks, 1_006);
        assert_eq!(p.sampled_at, Some(t0 + Duration::from_millis(150)));
    }

    #[test]
    fn projection_leaves_a_stopped_sample_where_it_was() {
        let t0 = Instant::now();
        let s = AxisSnapshot {
            running: false,
            ..tracking_sample(t0)
        };
        let p = s.projected_to(t0 + Duration::from_secs(10), TMR, HSR, NO_CAP);
        assert_eq!(p.position_ticks, 1_000);
    }

    #[test]
    fn projection_stops_advancing_past_the_age_cap() {
        let t0 = Instant::now();
        let s = tracking_sample(t0);
        let p = s.projected_to(
            t0 + Duration::from_secs(60),
            TMR,
            HSR,
            Duration::from_millis(500),
        );
        assert_eq!(p.position_ticks, 1_020);
    }

    #[test]
    fn projection_leaves_an_unsampled_axis_unchanged() {
        let s = AxisSnapshot {
            sampled_at: None,
            ..tracking_sample(Instant::now())
        };
        let p = s.projected_to(Instant::now() + Duration::from_secs(1), TMR, HSR, NO_CAP);
        assert_eq!(p.position_ticks, 1_000);
    }

    #[test]
    fn projection_to_an_instant_before_the_sample_is_the_sample() {
        let t0 = Instant::now() + Duration::from_secs(1);
        let s = tracking_sample(t0);
        let p = s.projected_to(Instant::now(), TMR, HSR, NO_CAP);
        assert_eq!(p.position_ticks, 1_000);
    }

    #[tokio::test]
    async fn a_poll_sample_carries_the_last_commanded_step_period() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        m.send(
            &session,
            Command::SetStepPeriod {
                axis: Axis::Ra,
                period: 123_456,
            },
        )
        .await
        .unwrap();
        let snap = m.poll_axes_now(&session).await.unwrap();
        assert_eq!(snap.ra.step_period, 123_456);
        assert_eq!(snap.dec.step_period, 0, "only the RA axis was commanded");
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_poll_sample_is_stamped_and_carries_the_status_bits() {
        let (m, state) = fast_polling_manager();
        let session = m.transport().acquire().await.unwrap();
        {
            let mut st = state.lock().await;
            st.ra.running = true;
            st.ra.mode = ModeKind::Tracking;
            st.ra.direction = Direction::Ccw;
            st.ra.speed = Speed::Fast;
        }
        let before = Instant::now();
        let snap = m.poll_axes_now(&session).await.unwrap();
        assert!(snap.ra.sampled_at.unwrap() >= before);
        assert!(snap.dec.sampled_at.unwrap() >= snap.ra.sampled_at.unwrap());
        assert!(snap.ra.running && snap.ra.ccw && snap.ra.fast && !snap.ra.goto);
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_fresh_handshake_forgets_the_commanded_step_periods() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        for axis in [Axis::Ra, Axis::Dec] {
            m.send(
                &session,
                Command::SetStepPeriod {
                    axis,
                    period: 123_456,
                },
            )
            .await
            .unwrap();
        }
        session.close().await.unwrap();
        // LazyAcquire: the next acquire re-runs the handshake.
        let session = m.transport().acquire().await.unwrap();
        let snap = m.poll_axes_now(&session).await.unwrap();
        assert_eq!(snap.ra.step_period, 0);
        assert_eq!(snap.dec.step_period, 0);
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn seeding_a_position_stamps_it_as_sampled_now() {
        let m = manager();
        let before = Instant::now();
        m.seed_ra_position(12_345).await;
        m.seed_dec_position(-6_789).await;
        let snap = m.snapshot().await;
        assert!(snap.ra.sampled_at.unwrap() >= before);
        assert!(snap.dec.sampled_at.unwrap() >= before);
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_now_carries_a_tracking_sample_forward_to_the_read() {
        let m = manager();
        let session = m.transport().acquire().await.unwrap();
        let params = m.parameters().await.unwrap();
        let t0 = Instant::now();
        *m.snapshot.write().await = MountSnapshot {
            ra: tracking_sample(t0),
            dec: AxisSnapshot::default(),
        };
        tokio::time::advance(Duration::from_millis(150)).await;
        let expected = sat_round_i32(
            tracking_sample(t0).rate_ticks_per_sec(params.tmr_freq, params.high_speed_ratio_ra)
                * 0.150
                + 1_000.0,
        );
        assert_eq!(m.snapshot_now().await.ra.position_ticks, expected);
        assert_eq!(
            m.snapshot().await.ra.position_ticks,
            1_000,
            "the raw sample is untouched"
        );
        session.close().await.unwrap();
    }
}
