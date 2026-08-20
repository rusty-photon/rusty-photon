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
//! - [`watchers`] — tokio tasks observing slew / park / pulse-guide
//!   completion in the background.
//! - [`tracking_guard`] — per-connection background task that stops
//!   tracking before the encoder `mech_HA` drifts into the CW
//!   exclusion zone (issue #259).
//! - [`park_persistence`] — JSON config-file read/write for `SetPark`
//!   and the boot-time writability probe.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::telescope::PierSide;
use rusty_photon_shared_transport::Session;
use tokio::sync::RwLock;

use rusty_photon_driver::ConfigActionCtx;

use crate::codec::SkywatcherCodec;
use crate::config::{ApPark, MountConfig};
use crate::config_actions::StarAdvDriver;
use crate::manager::MountManager;

mod actions;
mod device;
mod inherent;
mod park_persistence;
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
    /// Per-axis `PulseGuide` in-flight flags. See §"`PulseGuide`
    /// lifecycle" in the design doc.
    pulse_guiding: PulseGuiding,
}

/// Per-axis `PulseGuide` in-flight flags. An axis' flag is `true`
/// between issuing a `PulseGuide` on it and the watcher clearing the
/// flag after the pulse `duration` has elapsed (or earlier, via the
/// cancellation rule — any axis-mutating operation clears the flags
/// before issuing its own wire commands so the watcher's post-sleep
/// restore bails out).
#[derive(Debug, Clone, Copy, Default)]
struct PulseGuiding {
    ra: bool,
    dec: bool,
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
            pulse_guiding: PulseGuiding::default(),
        }
    }
}

impl DriverState {
    /// Reset per-session client state on `set_connected(false)`.
    ///
    /// Disconnect resets the per-session client state but leaves
    /// mechanical state (`at_park`) intact — the mount's encoder
    /// doesn't move just because we closed the socket. The
    /// `slew_settle_time` override is also preserved so a client that
    /// has already tuned it keeps the value across reconnects, and
    /// `target_pier_side` is left to be overwritten by the next slew.
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
    ///   - `pulse_guiding` — the pulse-guide watchers are bound to
    ///     the now-closed transport; cancellation is implicit.
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
        // Literal instead of Default::default(): trait calls are not
        // allowed in a `const fn`.
        self.pulse_guiding = PulseGuiding {
            ra: false,
            dec: false,
        };
        self.guide_rate_ra_fraction = DEFAULT_GUIDE_RATE_FRACTION;
        self.guide_rate_dec_fraction = DEFAULT_GUIDE_RATE_FRACTION;
    }
}

/// Cloning yields a second **handle to the same device**: the session
/// slot, driver state, slew flag, and manager are shared `Arc`s, and
/// the config is an immutable copy. Used to hand the tracking watcher
/// ([`tracking_guard`]) its own handle so it can drive the full slew
/// path (auto-flip) from a background task.
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
    /// Slew/park "in progress" flag. Lives here as an [`AtomicBool`]
    /// rather than a [`DriverState`] field so [`SlewReservation`] can
    /// roll it back **synchronously** from `Drop` — a `Drop` impl can't
    /// `.await` the `state` `RwLock`. Set by the slew / park reservation,
    /// `ORed` into `slewing()` and the concurrent-motion refusals, and
    /// cleared by the completion watchers, `AbortSlew`, and disconnect.
    slew_in_progress: Arc<AtomicBool>,
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
            slew_in_progress: Arc::new(AtomicBool::new(false)),
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

    /// Borrow the held session for one request, converting the empty-slot
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
/// Acquired before a slew or park issues any motion. While held, the
/// reservation **rolls back on drop** — clearing `slew_in_progress` — so
/// every `?` early-return on the motion-issue path (a failed wire
/// command, or a failed hand-off to the completion watcher) restores the
/// flag without an explicit clear at the call site. On the success path
/// the caller calls [`SlewReservation::dismiss`] once the completion
/// watcher has been spawned; from that point the watcher owns clearing
/// the flag.
///
/// The flag is an [`AtomicBool`] rather than a field behind the device's
/// `RwLock<DriverState>` precisely so this rollback can be a synchronous
/// store from `Drop` (a `Drop` impl cannot `.await` a `tokio::sync::RwLock`
/// write). Mirrors the synchronous rollback-on-drop guard the
/// `rusty-photon-shared-transport` `acquire()` path uses for its refcount.
#[must_use = "a dropped reservation rolls back slew_in_progress; bind it for the operation's duration"]
pub(super) struct SlewReservation {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl SlewReservation {
    /// Reserve the slot, returning the guard, or [`None`] when a slew /
    /// park is already in progress. The check-and-set is a single
    /// `compare_exchange`, so two concurrent callers can't both win the
    /// reservation (the TOCTOU-free guarantee the previous lock-guarded
    /// check-and-set gave).
    pub(super) fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self {
                flag: Arc::clone(flag),
                armed: true,
            })
    }

    /// Hand the flag's lifecycle off to the completion watcher: disarm
    /// the rollback so dropping this guard leaves `slew_in_progress` set.
    /// Call only after the watcher has been successfully spawned.
    pub(super) fn dismiss(mut self) {
        self.armed = false;
    }
}

impl Drop for SlewReservation {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(false, Ordering::SeqCst);
        }
    }
}

/// Convert latitude sign into the natural pre-flip pier side: `West`
/// for the Northern Hemisphere (Polaris-side counterweight), `East`
/// for the Southern. Used everywhere the slew planner / watcher
/// needs to compare the user-requested pier side against the
/// pre-flip pose.
fn pre_flip_side_for_latitude(site_latitude_deg: f64) -> PierSide {
    if site_latitude_deg >= 0.0 {
        PierSide::West
    } else {
        PierSide::East
    }
}
