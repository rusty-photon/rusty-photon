//! ASCOM Alpaca Telescope device for the Star Adventurer `GTi`.
//!
//! This is the surface that Alpaca clients (NINA, `SGPro`, `rp`, ...) talk to.
//! Capability-flag overrides match the design doc's
//! [§"Capability flags"](../../../docs/services/star-adventurer-gti.md#capability-flags)
//! table; defaulted methods that the MVP does not implement are left to the
//! ascom-alpaca trait's `NOT_IMPLEMENTED` default.
//!
//! ## Submodule layout
//!
//! - [`actions`] — the three driver-specific ASCOM `Action` handlers
//!   (`SetUnparkFromApPosition`, `SetPreferredApPark`,
//!   `UnparkFromApPosition`) dispatched from `device`'s `action`.
//! - [`device`] — `impl Device for MountDevice` (connect/description,
//!   `SupportedActions` + `Action` dispatch).
//! - [`telescope`] — `impl Telescope for MountDevice` (the ASCOM
//!   surface: coordinate reads, slew/sync/park, side-of-pier,
//!   pulse-guide).
//! - [`inherent`] — methods on `MountDevice` shared between the trait
//!   impls (validation, motion-control wrappers, post-connect lifecycle,
//!   the slew planner).
//! - [`slew`] — wire-level slew helpers (`:K`/`:G`/`:I`/`:H`/`:M`/`:J`
//!   sequence) and flip-aware delta geometry.
//! - [`watchers`] — tokio tasks observing slew / park completion in the
//!   background.
//! - [`pulse`] — `PulseGuide`: starting a pulse (the live rate change on
//!   a tracking RA axis, or a start from rest) and the watcher that ends
//!   it, with its failure ladder.
//! - [`tracking_guard`] — per-connection background task that stops
//!   tracking before the encoder `mech_HA` drifts into the CW
//!   exclusion zone (issue #259).
//! - [`park_persistence`] — JSON config-file read/write for `SetPark`
//!   and the boot-time writability probe.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::PierSide;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;
use tracing::debug;

use rusty_photon_driver::ConfigActionCtx;

use crate::codec::SkywatcherCodec;
use crate::config::{ApPark, MountConfig};
use crate::config_actions::StarAdvDriver;
use crate::manager::MountManager;

mod actions;
mod device;
mod inherent;
mod park_persistence;
mod pulse;
mod slew;
mod telescope;
mod tracking_guard;
mod watchers;

#[cfg(all(test, feature = "mock"))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unwrap_used)]
mod tests;

pub use park_persistence::{
    canonicalise_config_path, probe_park_file_writability, warn_if_park_path_unwritable,
};

/// Default guide rate as a fraction of sidereal. ASCOM clients see
/// this multiplied by `SIDEREAL_DEG_PER_SEC` through
/// `GuideRateRightAscension` / `GuideRateDeclination`.
const DEFAULT_GUIDE_RATE_FRACTION: f64 = 0.5;

/// In-memory mirror of latched-from-the-client state (Tracking enabled,
/// `AtPark` flag, last target). The values that come from the wire (current
/// RA/Dec, Slewing) are read through [`MountManager`].
#[derive(Debug)]
struct DriverState {
    tracking_requested: bool,
    at_park: bool,
    target_ra_hours: Option<f64>,
    target_dec_degrees: Option<f64>,
    slew_settle_time: Option<Duration>,
    /// In-memory park-target encoder pair. Resolved per axis on the
    /// 0→1 connect transition: `MountConfig::park_*_ticks` if `Some`
    /// (honored regardless of anchoring), otherwise the
    /// `preferred_ap_park` pose ticks when the frame is anchored.
    /// `None` means the axis has **no park target** — `Park()` stops
    /// that axis in place instead of slewing (unanchored frame, no raw
    /// override). Re-armed by the sync that anchors the frame and by
    /// `SetPreferredApPark`. See the design doc's §Park lifecycle.
    park_ra_ticks: Option<i32>,
    park_dec_ticks: Option<i32>,
    /// Whether the encoder→pose mapping has operator-asserted or
    /// measured ground truth. `true` from connect when
    /// `unpark_from_ap_position` is a named park (`ap_park_1..5`);
    /// flips `true` on a successful `SyncToCoordinates` /
    /// `SyncToTarget` or a named-park `UnparkFromApPosition`. While
    /// `false`, `Park()` must not slew to an absolute AP-pose target —
    /// that would command real motion to a fabricated position
    /// (workspace tenet: no actuation on connect). Reset on disconnect
    /// and re-derived on the next connect.
    frame_anchored: bool,
    /// `preferred_ap_park` as resolved at connect (config-file read).
    /// Kept so the sync that anchors a previously unanchored frame can
    /// re-arm the park target without re-reading the file. `None`
    /// before the first connect.
    preferred_ap_park: Option<ApPark>,
    /// Pier side the most recent slew was *issued for*. Read by the
    /// slew-completion watcher's pickup loop so it picks
    /// `target_encoder_normal` vs `target_encoder_flipped` for the
    /// corrective re-slew. Without this, a successful flip slew would
    /// be undone by the pickup loop's first iteration (the post-flip
    /// Dec encoder is past the pole, and a pre-flip encoder target
    /// would order a slew back through the pole).
    target_pier_side: Option<PierSide>,
    /// `PulseGuide` rate on the RA axis as a fraction of sidereal in
    /// `(0, 1)`. `GuideRateRightAscension` is this × `SIDEREAL_DEG_PER_SEC`.
    /// Resets to [`DEFAULT_GUIDE_RATE_FRACTION`] on each disconnect.
    guide_rate_ra_fraction: f64,
    guide_rate_dec_fraction: f64,
    /// Per-axis `PulseGuide` ownership. See §"`PulseGuide` lifecycle" in
    /// the design doc.
    pulse_guiding: PulseGuiding,
    /// Id the next pulse gets. Ids are never reused within a process, so
    /// a watcher can tell its own pulse from a newer one on the same axis.
    next_pulse_id: u64,
}

/// What a reload carries from one driver lifecycle to the next: the part
/// of [`DriverState`] that [`DriverState::reset_for_disconnect`] keeps.
///
/// A reload rebuilds the driver, not the mount, so it keeps what a
/// disconnect keeps — `AtPark`, because the encoders have not moved, and a
/// `SlewSettleTime` a client set — and nothing else. [`MountDevice::retire`]
/// hands it over; [`MountDevice::with_retained`] starts the next lifecycle
/// from it. The default is what a fresh process starts from: not parked,
/// no settle override.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetainedState {
    at_park: bool,
    slew_settle_time: Option<Duration>,
}

/// Identity of one `PulseGuide` call, held in [`PulseGuiding`] for as long
/// as that pulse owns its axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PulseId(u64);

/// Per-axis `PulseGuide` ownership: the id of the pulse in flight on the
/// axis, or `None`. An axis holds its pulse's id from the moment
/// `PulseGuide` claims it until the pulse's watcher has ended it — or
/// until an operation that takes over the axis clears it (the
/// cancellation rule), under `axis_ownership`, before its own wire
/// commands. A watcher acts only while the slot still holds its own id,
/// so a cancelled pulse's watcher never touches a newer pulse.
#[derive(Debug, Clone, Copy, Default)]
struct PulseGuiding {
    ra: Option<PulseId>,
    dec: Option<PulseId>,
}

impl PulseGuiding {
    /// No pulse on either axis.
    const IDLE: Self = Self {
        ra: None,
        dec: None,
    };

    /// The pulse in flight on `axis`. `GuideDirection` only resolves to
    /// `Ra` or `Dec`; anything that is not RA is the Dec slot.
    const fn get(&self, axis: skywatcher_motor_protocol::Axis) -> Option<PulseId> {
        match axis {
            skywatcher_motor_protocol::Axis::Ra => self.ra,
            _ => self.dec,
        }
    }

    const fn set(&mut self, axis: skywatcher_motor_protocol::Axis, pulse: Option<PulseId>) {
        match axis {
            skywatcher_motor_protocol::Axis::Ra => self.ra = pulse,
            _ => self.dec = pulse,
        }
    }

    /// `IsPulseGuiding`.
    const fn is_active(&self) -> bool {
        self.ra.is_some() || self.dec.is_some()
    }

    /// The axes with a pulse in flight, RA first.
    fn axes(self) -> impl Iterator<Item = skywatcher_motor_protocol::Axis> {
        [
            skywatcher_motor_protocol::Axis::Ra,
            skywatcher_motor_protocol::Axis::Dec,
        ]
        .into_iter()
        .filter(move |axis| self.get(*axis).is_some())
    }
}

impl Default for DriverState {
    fn default() -> Self {
        Self {
            tracking_requested: false,
            at_park: false,
            target_ra_hours: None,
            target_dec_degrees: None,
            slew_settle_time: None,
            park_ra_ticks: None,
            park_dec_ticks: None,
            frame_anchored: false,
            preferred_ap_park: None,
            target_pier_side: None,
            guide_rate_ra_fraction: DEFAULT_GUIDE_RATE_FRACTION,
            guide_rate_dec_fraction: DEFAULT_GUIDE_RATE_FRACTION,
            pulse_guiding: PulseGuiding::IDLE,
            next_pulse_id: 0,
        }
    }
}

impl DriverState {
    /// A fresh state starting from what an earlier lifecycle kept.
    fn with_retained(retained: RetainedState) -> Self {
        Self {
            at_park: retained.at_park,
            slew_settle_time: retained.slew_settle_time,
            ..Self::default()
        }
    }

    /// The part of this state a reload carries over.
    const fn retained(&self) -> RetainedState {
        RetainedState {
            at_park: self.at_park,
            slew_settle_time: self.slew_settle_time,
        }
    }

    /// Allocate the next [`PulseId`].
    const fn allocate_pulse_id(&mut self) -> PulseId {
        let id = PulseId(self.next_pulse_id);
        self.next_pulse_id = self.next_pulse_id.wrapping_add(1);
        id
    }

    /// Clear `axis`' pulse if it is still `id`; returns whether it was.
    fn release_pulse(&mut self, axis: skywatcher_motor_protocol::Axis, id: PulseId) -> bool {
        let owned = self.pulse_guiding.get(axis) == Some(id);
        if owned {
            self.pulse_guiding.set(axis, None);
        }
        owned
    }

    /// Reset per-session client state on `set_connected(false)`.
    ///
    /// Disconnect resets the per-session client state but leaves
    /// mechanical state (`at_park`) intact — the mount's encoder
    /// doesn't move just because we closed the socket. The
    /// `slew_settle_time` override is also preserved so a client that
    /// has already tuned it keeps the value across reconnects, and
    /// `target_pier_side` is left to be overwritten by the next slew.
    /// A reload keeps `at_park` and the `slew_settle_time` override as
    /// well, as [`RetainedState`]; it does not keep `target_pier_side`.
    ///
    /// Clear:
    ///   - `target_ra_hours` / `target_dec_degrees` — latched from a
    ///     `SetTargetRA` / `SetTargetDec` call; not durable.
    ///   - `tracking_requested` — disconnect halted tracking on the
    ///     wire (`:K1`); the in-memory flag must follow.
    ///   - `slew_in_progress` is **not** cleared here — it now lives on
    ///     [`MountDevice`] as an [`AtomicBool`], cleared synchronously by
    ///     [`SlewReservation`] on rollback and by the disconnect path in
    ///     `device.rs` (the `set_connected(false)` arm) alongside this
    ///     call. Clearing it there still tells any in-flight watcher
    ///     iteration to bail out.
    ///   - `park_ra_ticks` / `park_dec_ticks` — re-loaded on next
    ///     connect from config / handshake. Clearing here means a
    ///     mid-session edit to `MountConfig::park_*_ticks` would take
    ///     effect on reconnect.
    ///   - `frame_anchored` / `preferred_ap_park` — re-derived on the
    ///     next connect. A sync-derived anchor deliberately does not
    ///     survive disconnect: a new session cannot know what an
    ///     earlier one measured (see the design doc's §Park lifecycle).
    ///   - `pulse_guiding` — disconnect cancels every pulse (the
    ///     disconnect path does this under `axis_ownership` first; the
    ///     reset repeats it so the state is whole on its own).
    ///   - `guide_rate_*_fraction` — re-initialise to the default,
    ///     matching INDI's per-session reset.
    const fn reset_for_disconnect(&mut self) {
        self.target_ra_hours = None;
        self.target_dec_degrees = None;
        self.tracking_requested = false;
        self.park_ra_ticks = None;
        self.park_dec_ticks = None;
        self.frame_anchored = false;
        self.preferred_ap_park = None;
        self.pulse_guiding = PulseGuiding::IDLE;
        self.guide_rate_ra_fraction = DEFAULT_GUIDE_RATE_FRACTION;
        self.guide_rate_dec_fraction = DEFAULT_GUIDE_RATE_FRACTION;
    }
}

/// Cloning yields a second **handle to the same device**: the session
/// slot, driver state, slew flag, and manager are shared `Arc`s, and
/// the config is an immutable copy.
///
/// Used to hand the tracking watcher ([`tracking_guard`]) its own handle
/// so it can drive the full slew path (auto-flip) from a background task.
#[derive(Clone, derive_more::Debug)]
pub struct MountDevice {
    config: MountConfig,
    /// Optional config-file path. `Some` when the driver was started
    /// with `--config <path>`; `None` for `Config::default()` runs. Drives
    /// `CanSetPark` and is the destination for `SetPark` writes.
    config_file_path: Option<PathBuf>,
    /// Session held while connected. `Some` between successful
    /// `set_connected(true)` and `set_connected(false)`. The slot
    /// presence is the truth — no separate "requested" bool that can
    /// desync from the shared transport's refcount. Replaces the
    /// pre-migration `requested_connection: RwLock<bool>` flag.
    #[debug(skip)]
    session: Arc<RwLock<Option<Session<SkywatcherCodec>>>>,
    state: Arc<RwLock<DriverState>>,
    /// The slew/park slot: empty, or held by one slew or park. Lives
    /// here as a [`SlewSlot`] (atomics) rather than a [`DriverState`]
    /// field so [`SlewReservation`] can release it **synchronously** from
    /// `Drop` — a `Drop` impl can't `.await` the `state` `RwLock`. Claimed
    /// by the slew / park reservation, `ORed` into `slewing()` and the
    /// concurrent-motion refusals, released by the completion watchers,
    /// and emptied by `AbortSlew` and disconnect.
    slew_in_progress: Arc<SlewSlot>,
    /// Serializes *taking ownership of the axes* — held across a sync's
    /// encoder writes, taken by a slew or park around its
    /// [`SlewReservation`] acquisition, and held by every `PulseGuide`
    /// wire burst (a pulse's start and each restore attempt) and by
    /// every operation that cancels a pulse, while it does so. Lock order:
    /// this first, then the session slot or `state`.
    ///
    /// `slew_in_progress` alone cannot do this job for sync.
    /// `AbortSlew` clears that flag unconditionally, which is right for
    /// the motion it cancels but would strip a sync of the exclusivity
    /// it is relying on: a slew could then win the reservation and
    /// start moving between a sync's snapshot read and its `:E` writes.
    /// A lock no third party can release on an owner's behalf closes
    /// that, and keeps `Slewing` honest — a sync is not motion and does
    /// not set the flag.
    #[debug(skip)]
    axis_ownership: Arc<tokio::sync::Mutex<()>>,
    /// Whether the mount refused a live `:I1` (a step-period change on the
    /// running RA motor) on this connection. Once it has, guide pulses
    /// stop and restart RA instead of trying again. Cleared on
    /// disconnect.
    live_rate_refused: Arc<AtomicBool>,
    #[debug(skip)]
    manager: Arc<MountManager>,
    /// Config-action context; `Some` enables `config.get` / `config.apply` /
    /// `config.schema` on this device (alongside the `ApPark` vendor actions).
    /// `None` for focused unit-test devices.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<StarAdvDriver>>,
}

impl MountDevice {
    #[must_use]
    pub fn new(config: MountConfig, manager: Arc<MountManager>) -> Self {
        Self::with_config_file_path(config, manager, None)
    }

    /// Construct with an optional config-file path. `Some(path)` enables
    /// `CanSetPark` / `SetPark` persistence; `None` leaves
    /// `CanSetPark = false` and `SetPark = NOT_IMPLEMENTED`.
    #[must_use]
    pub fn with_config_file_path(
        config: MountConfig,
        manager: Arc<MountManager>,
        config_file_path: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            config_file_path,
            session: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(DriverState::default())),
            slew_in_progress: Arc::new(SlewSlot::default()),
            axis_ownership: Arc::new(tokio::sync::Mutex::new(())),
            live_rate_refused: Arc::new(AtomicBool::new(false)),
            manager,
            config_ctx: None,
        }
    }

    /// Attach the config-action context, enabling the config vendor actions.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<StarAdvDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    /// Start from what an earlier lifecycle of this driver kept (see
    /// [`RetainedState`]). Call it while building the device, before any
    /// clone of it exists: it replaces the state the clones would share.
    #[must_use]
    pub fn with_retained(mut self, retained: RetainedState) -> Self {
        debug!(
            ?retained,
            "starting from the state the previous lifecycle kept"
        );
        self.state = Arc::new(RwLock::new(DriverState::with_retained(retained)));
        self
    }

    /// End this lifecycle for a reload and hand over what the next one
    /// keeps. Sends nothing to the mount.
    ///
    /// A reload rebuilds the driver without disconnecting it, so a park
    /// still in flight keeps its claim on the slew slot, and the
    /// shutdown's safety stop then halts its axes — which its watcher
    /// cannot tell from a park that arrived. Called once serving has
    /// ended and before that stop, this empties the slot under
    /// `axis_ownership`, as `AbortSlew` and disconnect do. A park watcher
    /// marks the mount parked only while holding `axis_ownership` and its
    /// claim, so it either finished before the slot was emptied, and the
    /// park is carried, or never will: the state read below is final.
    pub async fn retire(&self) -> RetainedState {
        {
            let _axes = self.axis_ownership.lock().await;
            self.slew_in_progress.clear();
        }
        let retained = self.state.read().await.retained();
        debug!(?retained, "retired the mount for a reload");
        retained
    }

    /// Send one command through the device's session and return the
    /// typed response. Returns [`crate::error::StarAdvError::NotConnected`]
    /// when the session slot is empty.
    pub(super) async fn send(
        &self,
        cmd: skywatcher_motor_protocol::Command,
    ) -> crate::error::Result<skywatcher_motor_protocol::Response> {
        self.with_session(async |session| self.manager.send(session, cmd).await)
            .await
    }

    /// [`Self::send`], plus when the command crossed the wire; see
    /// [`MountManager::send_timed`](crate::manager::MountManager::send_timed).
    pub(super) async fn send_timed(
        &self,
        cmd: skywatcher_motor_protocol::Command,
    ) -> crate::error::Result<(
        skywatcher_motor_protocol::Response,
        rusty_photon_shared_transport::WireTiming,
    )> {
        self.with_session(async |session| self.manager.send_timed(session, cmd).await)
            .await
    }

    /// Borrow the held session for the closure's wire I/O — a single
    /// request or a multi-step sequence (the read guard is held for the
    /// closure's whole duration either way) — converting the empty-slot
    /// case into the caller's error type via
    /// [`crate::error::StarAdvError::NotConnected`].
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the session reference borrows the read guard, which is deliberately held across the device I/O so a disconnect's write lock waits out in-flight commands"
    )]
    async fn with_session<F, T, E>(&self, f: F) -> Result<T, E>
    where
        E: From<crate::error::StarAdvError>,
        F: AsyncFnOnce(&Session<SkywatcherCodec>) -> Result<T, E>,
    {
        let guard = self.session.read().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| E::from(crate::error::StarAdvError::NotConnected))?;
        f(session).await
    }
}

/// RAII reservation of the `slew_in_progress` slot on [`MountDevice`].
///
/// The slew/park slot: empty, or held by one slew or park, which it
/// names by that operation's [`SlewToken`].
///
/// A plain "in progress" flag cannot tell *still running* from
/// *aborted, then claimed by the next slew*: a watcher or a slew that
/// only reads `true` carries on under the next operation's claim. After
/// `AbortSlew` and an immediate new slew, the aborted slew's watcher
/// would then run its pickup re-slew into the middle of the new one. So
/// the slot holds its owner's token, and each actor checks for its own
/// ([`SlewClaim::is_current`]) before it acts. A release only empties
/// the slot while it still holds the releaser's token, so a late
/// release cannot clear a newer owner's claim. `AbortSlew` and
/// disconnect empty it outright ([`SlewSlot::clear`]), voiding whoever
/// held it.
#[derive(Debug, Default)]
pub(super) struct SlewSlot {
    /// `0` when empty, otherwise the owner's token.
    owner: AtomicU64,
    /// The last token handed out; tokens start at 1.
    issued: AtomicU64,
}

/// Names one slew or park for as long as it holds the [`SlewSlot`].
/// Never `0`, which marks the slot empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SlewToken(u64);

impl SlewSlot {
    /// Whether any slew or park holds the slot.
    pub(super) fn is_held(&self) -> bool {
        self.owner.load(Ordering::SeqCst) != 0
    }

    /// Claim the empty slot under a fresh token, or [`None`] when a slew
    /// or park already holds it. The check-and-set is a single
    /// `compare_exchange`, so two concurrent callers can't both win.
    pub(super) fn try_claim(&self) -> Option<SlewToken> {
        // `saturating_add` keeps the token off `0` even after the counter
        // wraps — which 2^64 claims will not reach.
        let token = self.issued.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        self.owner
            .compare_exchange(0, token, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| SlewToken(token))
    }

    /// Whether `token`'s operation still holds the slot.
    pub(super) fn holds(&self, token: SlewToken) -> bool {
        self.owner.load(Ordering::SeqCst) == token.0
    }

    /// Empty the slot if `token` still holds it. Does nothing once an
    /// abort, a disconnect or a later claim has moved it on.
    pub(super) fn release(&self, token: SlewToken) {
        // `Err` is the "moved on" case, which is exactly when there is
        // nothing to release.
        let _ = self
            .owner
            .compare_exchange(token.0, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Empty the slot whoever holds it: `AbortSlew` and disconnect void
    /// the operation in flight.
    pub(super) fn clear(&self) {
        self.owner.store(0, Ordering::SeqCst);
    }
}

/// One operation's hold on the [`SlewSlot`], as its completion watcher
/// carries it.
#[derive(Debug, Clone)]
pub(super) struct SlewClaim {
    slot: Arc<SlewSlot>,
    token: SlewToken,
    /// The device's `axis_ownership` lock; see [`Self::hold_axes`].
    axes: Arc<tokio::sync::Mutex<()>>,
}

impl SlewClaim {
    /// Whether this operation still holds the slot — `false` once
    /// `AbortSlew` or disconnect has emptied it, even if another slew has
    /// claimed it since.
    pub(super) fn is_current(&self) -> bool {
        self.slot.holds(self.token)
    }

    /// Take `axis_ownership`, if this operation still holds the slot.
    ///
    /// `AbortSlew`, disconnect and every new claim go through
    /// `axis_ownership`, so the claim cannot lapse while the returned
    /// guard is held. A check of [`Self::is_current`] followed by wire
    /// commands races them: an abort can land in between, and the next
    /// slew can claim the slot before the commands go out. Under the
    /// guard, an abort lands either before the check, which then fails,
    /// or after the commands, where its `:L` stops what they started.
    /// Hold it across the act only, never across a wait for an axis to
    /// stop, so that an abort waits for a few frames at most.
    pub(super) async fn hold_axes(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.axes.lock().await;
        self.is_current().then_some(guard)
    }

    /// Empty the slot if this operation still holds it.
    pub(super) fn release(&self) {
        self.slot.release(self.token);
    }
}

/// Acquired before a slew or park issues any motion. While held, the
/// reservation **rolls back on drop** — releasing its claim on the
/// [`SlewSlot`] — so every `?` early-return on the motion-issue path (a
/// refused plan, a failed wire command, or a failed hand-off to the
/// completion watcher) restores the slot without an explicit release at
/// the call site. On the success path the caller hands
/// [`SlewReservation::claim`] to the completion watcher and calls
/// [`SlewReservation::dismiss`] once it has been spawned; from that
/// point the watcher owns the release.
///
/// The slot is atomics rather than a field behind the device's
/// `RwLock<DriverState>` precisely so this rollback can be synchronous
/// from `Drop` (a `Drop` impl cannot `.await` a `tokio::sync::RwLock`
/// write). Mirrors the synchronous rollback-on-drop guard the
/// `rusty-photon-shared-transport` `acquire()` path uses for its refcount.
#[must_use = "a dropped reservation releases the slew slot; bind it for the operation's duration"]
pub(super) struct SlewReservation {
    claim: SlewClaim,
    armed: bool,
}

impl SlewReservation {
    /// Reserve the slot, returning the guard, or [`None`] when a slew /
    /// park is already in progress. `axes` is the device's
    /// `axis_ownership`, which the caller holds while it claims.
    pub(super) fn try_acquire(
        slot: &Arc<SlewSlot>,
        axes: &Arc<tokio::sync::Mutex<()>>,
    ) -> Option<Self> {
        slot.try_claim().map(|token| Self {
            claim: SlewClaim {
                slot: Arc::clone(slot),
                token,
                axes: Arc::clone(axes),
            },
            armed: true,
        })
    }

    /// This operation's claim, for its own checks and its watcher.
    pub(super) fn claim(&self) -> SlewClaim {
        self.claim.clone()
    }

    /// Hand the slot's release off to the completion watcher: disarm the
    /// rollback so dropping this guard leaves the claim in place. Call
    /// only after the watcher has been successfully spawned.
    pub(super) fn dismiss(mut self) {
        self.armed = false;
    }
}

impl Drop for SlewReservation {
    fn drop(&mut self) {
        if self.armed {
            self.claim.release();
        }
    }
}

/// Convert latitude sign into the natural pre-flip pier side: `West`
/// for the Northern Hemisphere (Polaris-side counterweight), `East`
/// for the Southern. Used everywhere the slew planner / watcher
/// needs to compare the user-requested pier side against the
/// pre-flip pose.
///
/// Thin alias for [`crate::coordinates::pre_flip_side`], which the
/// coordinate layer needs for the same comparisons; the hemisphere
/// rule has one definition.
fn pre_flip_side_for_latitude(site_latitude_deg: f64) -> PierSide {
    crate::coordinates::pre_flip_side(site_latitude_deg)
}
