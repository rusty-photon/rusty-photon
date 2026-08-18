//! Inherent methods on [`MountDevice`] — the helpers the `Device` and
//! `Telescope` trait impls compose to expose the ASCOM surface.
//!
//! Grouped here so the trait-impl files (`device.rs`, `telescope.rs`)
//! stay focused on protocol dispatch. The methods fall into a few
//! buckets:
//!
//! - **Error mapping**: [`MountDevice::ascom_session_err`],
//!   [`MountDevice::ascom_transport_err`] (for the cross-crate types
//!   the orphan rule blocks a direct `From`-impl for; everything else
//!   converts via the `From<StarAdvError> for ASCOMError` impl in
//!   [`crate::error`]).
//! - **Validation**: [`MountDevice::validate_coordinates`],
//!   [`MountDevice::check_within_safe_envelope`], and the free
//!   [`validate_guide_rate`] used by `set_guide_rate_*`.
//! - **Preconditions**: [`MountDevice::ensure_connected`],
//!   [`MountDevice::ensure_unparked`].
//! - **Motion control wrappers**: [`MountDevice::stop_and_wait`]
//!   (ASCOM-mapped wrapper over [`super::slew::stop_axis_and_wait`]),
//!   [`MountDevice::await_slew_complete`] (synchronous-slew polling).
//! - **Post-connect lifecycle**:
//!   [`MountDevice::seed_after_connect`],
//!   [`MountDevice::load_park_target_after_connect`].
//! - **Slew planner**:
//!   [`MountDevice::execute_slew_with_explicit_side`] — the shared
//!   body for `SlewToCoordinatesAsync` and `SetSideOfPier`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::{PierSide, Telescope};
use ascom_alpaca::api::Device;
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use rusty_photon_shared_transport::{Session, SessionError, TransportError};
use skywatcher_motor_protocol::{Axis, Command};
use tracing::{debug, info};

use crate::codec::SkywatcherCodec;
use crate::config::ApPark;
use crate::coordinates::{
    local_sidereal_time_hours, ra_dec_to_alt_az, side_of_pier as side_of_pier_calc,
    target_encoder_flipped, target_encoder_normal, SIDEREAL_DEG_PER_SEC,
};
use crate::error::StarAdvError;
use crate::manager::{MountParameters, MountSnapshot};
use crate::units::{Cpr, Dec, DecTicks, Lst, MechDec, MechHa, Ra, RaTicks};

use super::park_persistence::{read_connect_fields, MountConnectFields};
use super::slew::{
    check_non_flip_ra_path, flip_slew_dec_delta, flip_slew_ra_delta, issue_slew_axis,
    stop_axis_and_wait, AXIS_STOP_TIMEOUT,
};
use super::watchers::spawn_slew_completion_watcher;
use super::{pre_flip_side_for_latitude, MountDevice, SlewReservation};

/// Upper bound on how long the synchronous `SlewToCoordinates` /
/// `SlewToTarget` will wait for the watcher to clear `slew_in_progress`.
/// 5 minutes — far longer than any realistic slew (a worst-case full
/// half-revolution at high-speed slew rate is well under a minute on
/// the `GTi`) but finite enough that a stuck driver cannot wedge an
/// Alpaca request indefinitely.
const SYNC_SLEW_TIMEOUT: Duration = Duration::from_mins(5);

/// Maximum absolute encoder reading at connect that
/// [`MountDevice::seed_after_connect`] still treats as
/// "fresh power-up" and applies the `unpark_from_ap_position` seed to.
/// The
/// Sky-Watcher firmware does not always read exactly `0` after a
/// power-cycle — empirically the validation `GTi` reports `dec = −1`
/// on connect, a single-tick initialisation artifact (≈ 0.4″ at the
/// `GTi`'s CPR). Any genuine post-slew encoder reading is tens of
/// thousands of ticks away from zero, so this tolerance comfortably
/// distinguishes "just powered up" from "the operator already moved
/// the mount this session".
const FRESH_POWER_UP_TICK_TOLERANCE: i32 = 10;

/// Validate an ASCOM `GuideRate*` setter value (deg/sec) and return
/// the equivalent fraction of sidereal. Rejects values outside the
/// open interval `(0, SIDEREAL_DEG_PER_SEC)`:
///
/// - `≤ 0` is non-physical (zero rate = no motion; negative = wrong
///   direction).
/// - `≥ SIDEREAL_DEG_PER_SEC` would push East's `rate_factor = 1 -
///   fraction` to zero or negative, which divides by zero in the
///   step-period formula. INDI's eqmod driver imposes the same upper
///   bound for the same reason.
pub(super) fn validate_guide_rate(deg_per_sec: f64) -> ASCOMResult<f64> {
    let fraction = deg_per_sec / SIDEREAL_DEG_PER_SEC;
    if !fraction.is_finite() || fraction <= 0.0 || fraction >= 1.0 {
        return Err(ASCOMError::new(
            ASCOMErrorCode::INVALID_VALUE,
            format!(
                "guide rate {deg_per_sec} deg/sec is outside the supported \
                 range (0, {SIDEREAL_DEG_PER_SEC})"
            ),
        ));
    }
    Ok(fraction)
}

/// Resolved connect-time config: the raw per-axis `park_*_ticks`
/// overrides (`None` = fall through to the AP-park target) plus the
/// effective `unpark_from_ap_position` / `preferred_ap_park` (the
/// on-disk value, or the in-memory startup value when the key is absent
/// or no config file is in use). Produced once per connect by
/// [`MountDevice::read_connect_config`] and consumed by the seed +
/// park-target hooks so neither re-reads the file.
#[derive(Debug)]
pub(super) struct ConnectConfig {
    pub park_ra_ticks: Option<i32>,
    pub park_dec_ticks: Option<i32>,
    pub unpark_from_ap_position: ApPark,
    pub preferred_ap_park: ApPark,
}

impl MountDevice {
    /// Map a `SessionError<SkywatcherCodecError>` (from
    /// `SharedTransport::acquire`) into the closest ASCOM error.
    ///
    /// Lives here rather than as a `From<…> for ASCOMError` impl
    /// because the orphan rule blocks that conversion (both
    /// `SessionError<_>` and `ASCOMError` are foreign types). Routes
    /// through the two-step chain `SessionError` → [`StarAdvError`]
    /// → [`ASCOMError`] (both individually `From`-convertible).
    pub(super) fn ascom_session_err(
        err: SessionError<crate::codec::SkywatcherCodecError>,
    ) -> ASCOMError {
        StarAdvError::from(err).into()
    }

    /// Map a `TransportError` (from `Session::close`) into the closest
    /// ASCOM error. The shared-transport teardown is best-effort —
    /// any failure here surfaces to the ASCOM caller rather than
    /// being swallowed by a `tracing::warn!` (the pre-migration
    /// pattern, removed by the Phase E migration).
    ///
    /// Like [`ascom_session_err`](Self::ascom_session_err), the orphan
    /// rule rules out a direct `From<TransportError> for ASCOMError`;
    /// route through [`StarAdvError`] instead.
    pub(super) fn ascom_transport_err(err: TransportError) -> ASCOMError {
        StarAdvError::from(err).into()
    }

    /// Validate an RA value (hours, [0, 24)) and a Dec value (degrees,
    /// [-90, +90]), returning `INVALID_VALUE` when either is out of range.
    pub(super) fn validate_coordinates(ra_hours: f64, dec_degrees: f64) -> ASCOMResult<()> {
        if !(0.0..24.0).contains(&ra_hours) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("RightAscension must be in [0, 24) hours, got {ra_hours}"),
            ));
        }
        if !(-90.0..=90.0).contains(&dec_degrees) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!("Declination must be in [-90, +90] degrees, got {dec_degrees}"),
            ));
        }
        Ok(())
    }

    /// Reject a slew / sync / destination-side prediction whose target
    /// would land inside the CW exclusion zone or below the configured
    /// minimum-altitude floor.
    ///
    /// **Why:** the Star Adventurer `GTi` has a mechanical safety
    /// constraint — the counterweights must not rise more than 0.95 h
    /// above horizontal at any point. The arc where the CW exceeds
    /// that threshold is the configured CW exclusion zone (defaults
    /// `(0.95, 11.05)` h of `mech_HA`). Slewing the OTA past it stalls
    /// the motor against a hard stop while the encoder counter
    /// continues to advance — on a real-hardware `ConformU` run that
    /// drove the mount into the counterweight-up region we heard the
    /// motor whine and saw the axis stop physically for several
    /// seconds at a time. The 2026-05-17 San Diego session
    /// additionally demonstrated OTA-vs-tripod contact when the
    /// previously-narrower zone let the CW sweep through the
    /// ascending half.
    ///
    /// The check is in **encoder `mech_HA` space** (signed hours
    /// folded to `[−12, +12)`). For a target on the natural pier
    /// side, `target_mech_HA = celestial_HA`. For a flipped target,
    /// `target_mech_HA = celestial_HA + 12 h` folded. The zone comes from
    /// [`crate::config::CwExclusionZone::bounds`] and is treated as an
    /// **open** interval `(min, max)` — a target landing exactly on a
    /// boundary is permitted (and matches the open-interval convention
    /// [`super::slew::check_non_flip_ra_path`] uses for path checks). A
    /// disabled zone (JSON `null`, where `bounds()` returns `min > max`)
    /// disables the check, matching the same path-check convention.
    ///
    /// Note: this is the *destination-only* leg of the safety model.
    /// Path crossings are handled separately by
    /// [`super::slew::check_non_flip_ra_path`] (non-flip slews) and
    /// [`super::slew::flip_slew_ra_delta`] (flip slews); a target
    /// outside the zone with a sweep that crosses the zone is caught
    /// there, not here. The combination — destination check plus path
    /// check — is the safety floor.
    ///
    /// `flip_policy.flip_range_hours` is **not** consulted here — that
    /// rule lives in `select_pier_side_for_target` for pier-side
    /// preference only. Park 1 / Park 5 (anti-meridian poses with
    /// `mech_HA = ±12` on the chosen pier) are reachable via slew
    /// because their `mech_HA` is outside the CW exclusion zone.
    ///
    /// The second gate is the **altitude floor**
    /// ([`crate::config::MountConfig::min_altitude_degrees`]): the
    /// target's apparent altitude, computed from HA + Dec + site
    /// latitude via [`ra_dec_to_alt_az`], must be at or above the
    /// configured floor. A target exactly at the floor is accepted;
    /// a floor of `-90°` never rejects. Unlike the CW zone this is an
    /// operator pointing preference (default `0°`, the geometric
    /// horizon), not a mechanical constraint, and it has no path leg —
    /// only the destination is checked.
    ///
    /// Both gates are validated together, before any motion, so a
    /// partial-failure slew can't issue motion on RA before
    /// discovering the target fails the altitude gate.
    pub(super) fn check_within_safe_envelope(
        &self,
        ra_hours: f64,
        dec_degrees: f64,
        lst_hours: f64,
        target_is_flipped: bool,
    ) -> ASCOMResult<()> {
        let target_mech_ha = {
            let normal = Lst::new(lst_hours)
                .hour_angle_of(Ra::new(ra_hours))
                .to_mech()
                .value();
            if target_is_flipped {
                MechHa::new(normal + 12.0).value()
            } else {
                normal
            }
        };
        let (zone_min, zone_max) = self.config.cw_exclusion_zone.bounds();
        // Open interval: target landing exactly on a boundary is OK.
        // Disable on `min >= max` (empty zone), matching the path
        // check's convention so destination and path checks agree on
        // which configurations are "disabled."
        if zone_min < zone_max && target_mech_ha > zone_min && target_mech_ha < zone_max {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "target mech_HA {target_mech_ha:.3} h is inside the CW exclusion zone \
                     ({zone_min}, {zone_max}) h"
                ),
            ));
        }
        // Altitude floor: apparent altitude is a celestial property of
        // the target (a function of HA + Dec + site latitude), so the
        // check is identical for both pier sides — no
        // `target_is_flipped` involvement.
        let floor = self.config.min_altitude_degrees.value();
        let (target_alt, _az) = ra_dec_to_alt_az(
            Ra::new(ra_hours),
            Dec::new(dec_degrees),
            self.config.site_latitude_deg,
            Lst::new(lst_hours),
        );
        if target_alt < floor {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "target altitude {target_alt:.3}° is below the configured \
                     minimum altitude {floor:.3}°"
                ),
            ));
        }
        Ok(())
    }

    /// Refuse the operation when `AtPark` is set. Returns
    /// `INVALID_WHILE_PARKED` per ASCOM.
    pub(super) async fn ensure_unparked(&self) -> ASCOMResult<()> {
        if self.state.read().await.at_park {
            Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_WHILE_PARKED,
                "operation invalid while parked",
            ))
        } else {
            Ok(())
        }
    }

    /// Refuse the operation when the transport is not connected. Returns
    /// `NOT_CONNECTED` per ASCOM.
    pub(super) async fn ensure_connected(&self) -> ASCOMResult<()> {
        if self.connected().await? {
            Ok(())
        } else {
            Err(ASCOMError::NOT_CONNECTED)
        }
    }

    /// `ASCOMResult` wrapper over the free-function
    /// [`super::slew::stop_axis_and_wait`] — issues `:K<axis>` and
    /// polls `:f<axis>` until the running flag clears. Used by every
    /// `MountDevice` caller that needs ASCOM error mapping:
    /// `set_tracking(true)`, `park`, and the per-axis stop preceding
    /// each `issue_slew_axis` in `slew_to_coordinates_async`. The
    /// slew-completion and park watchers run inside spawned tasks and
    /// call `stop_axis_and_wait` directly (no `MountDevice` to wrap
    /// the error).
    pub(super) async fn stop_and_wait(&self, axis: Axis) -> ASCOMResult<()> {
        let guard = self.session.read().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| ASCOMError::from(StarAdvError::NotConnected))?;
        stop_axis_and_wait(&self.manager, session, axis, AXIS_STOP_TIMEOUT)
            .await
            .map_err(ASCOMError::from)
    }

    /// Block until the slew-completion watcher clears `slew_in_progress`,
    /// or until [`SYNC_SLEW_TIMEOUT`] elapses. Used by the synchronous
    /// `SlewToCoordinates` / `SlewToTarget` variants — those wrap their
    /// `_async` siblings, but ASCOM requires the synchronous methods
    /// not return until the slew is finished.
    ///
    /// Polls at the transport's `polling_interval` (same cadence the
    /// background snapshot poller uses, so `slewing()` can transition
    /// within one tick of the watcher's clear). The upper bound is
    /// well above any realistic real-mount slew but finite — a stuck
    /// watcher must not block an Alpaca request forever.
    pub(super) async fn await_slew_complete(&self) -> ASCOMResult<()> {
        let poll = self.manager.polling_interval_for_watcher();
        let start = std::time::Instant::now();
        while start.elapsed() < SYNC_SLEW_TIMEOUT {
            if !self.slewing().await? {
                return Ok(());
            }
            tokio::time::sleep(poll).await;
        }
        Err(ASCOMError::invalid_operation(
            "synchronous slew timed out waiting for completion",
        ))
    }

    /// Post-connect park-target load. Source priority, per axis:
    ///
    /// 1. `mount.park_*_ticks` from the **on-disk** config file when one
    ///    was supplied via `--config` (or `self.config.park_*_ticks` for
    ///    `Config::default()` runs). The raw-encoder override: an
    ///    operator who pinned a specific tick pair via `SetPark` or a
    ///    hand-edit — honored **regardless of anchoring** (raw ticks are
    ///    the operator's own frame assertion). Per-axis — one axis can
    ///    be pinned while the other falls through. Reading fresh from
    ///    disk means a `SetPark` followed by disconnect/reconnect picks
    ///    up the new target.
    /// 2. The `preferred_ap_park` encoder pair (the design's `Park()`
    ///    target), resolved from the configured AP park, the site
    ///    latitude, and the handshake counts-per-revolution — **only
    ///    when the coordinate frame is anchored** (named
    ///    `unpark_from_ap_position`, or a sync already anchored it this
    ///    connection). `preferred_ap_park` ships as `ap_park_3`.
    /// 3. **No target.** With an unanchored frame (`ap_park_0`, no sync
    ///    yet) and no raw override the axis keeps `None`: `Park()`
    ///    stops it in place instead of slewing. Slewing to an absolute
    ///    pose from an unanchored frame would command real motion to a
    ///    fabricated position — the workspace tenet *no actuation on
    ///    connect* forbids it. The sync that anchors the frame re-arms
    ///    the target via [`Self::anchor_frame_and_rearm_park_target`].
    ///
    /// Extracted from `set_connected` so a failure here (file missing,
    /// malformed JSON, lost transport mid-load) can be rolled back by the
    /// caller without leaking the connection ref-count. See the design
    /// doc's §"Park lifecycle" for the resolution rules.
    /// Read the connect-time `mount.*` config fields in a single disk
    /// read + parse (when started with `--config`), resolving each
    /// AP-park field to the in-memory startup value when the key is
    /// absent. For `Config::default()` runs (no config file) every field
    /// comes straight from `self.config`. Called once per connect by
    /// `set_connected` and re-used by both the seed and park-target
    /// hooks; also re-run by `SetPreferredApPark` to re-resolve the live
    /// target after a write.
    pub(super) async fn read_connect_config(&self) -> ASCOMResult<ConnectConfig> {
        let Some(path) = self.config_file_path.clone() else {
            return Ok(ConnectConfig {
                park_ra_ticks: self.config.park_ra_ticks,
                park_dec_ticks: self.config.park_dec_ticks,
                unpark_from_ap_position: self.config.unpark_from_ap_position,
                preferred_ap_park: self.config.preferred_ap_park,
            });
        };
        let MountConnectFields {
            park_ra_ticks,
            park_dec_ticks,
            unpark_from_ap_position,
            preferred_ap_park,
        } = tokio::task::spawn_blocking(move || read_connect_fields(&path))
            .await
            .map_err(|e| {
                ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    format!("connect-config read task join error: {e}"),
                )
            })?
            .map_err(ASCOMError::from)?;
        Ok(ConnectConfig {
            park_ra_ticks,
            park_dec_ticks,
            unpark_from_ap_position: unpark_from_ap_position
                .unwrap_or(self.config.unpark_from_ap_position),
            preferred_ap_park: preferred_ap_park.unwrap_or(self.config.preferred_ap_park),
        })
    }

    pub(super) async fn load_park_target_after_connect(
        &self,
        cfg: &ConnectConfig,
    ) -> ASCOMResult<()> {
        // A named unpark pose anchors the frame from config; a sync
        // this connection may already have anchored it (the OR keeps a
        // sync-derived anchor alive across a `SetPreferredApPark`
        // re-resolve — at connect time the flag is freshly reset).
        let anchored = cfg.unpark_from_ap_position != ApPark::ApPark0
            || self.state.read().await.frame_anchored;
        // Fallback for an axis without a raw override: the
        // `preferred_ap_park` encoder pair when the frame is anchored,
        // else no target at all (park-in-place).
        let pose_pair = if anchored {
            self.ap_park_target_ticks(cfg.preferred_ap_park).await
        } else {
            None
        };
        let ra_target = cfg.park_ra_ticks.or_else(|| pose_pair.map(|(ra, _)| ra));
        let dec_target = cfg.park_dec_ticks.or_else(|| pose_pair.map(|(_, dec)| dec));
        {
            let mut s = self.state.write().await;
            s.park_ra_ticks = ra_target;
            s.park_dec_ticks = dec_target;
            s.frame_anchored = anchored;
            s.preferred_ap_park = Some(cfg.preferred_ap_park);
        }
        debug!(
            ra_target,
            dec_target,
            anchored,
            from_config_ra = cfg.park_ra_ticks.is_some(),
            from_config_dec = cfg.park_dec_ticks.is_some(),
            preferred_ap_park = ?cfg.preferred_ap_park,
            from_file = self.config_file_path.is_some(),
            "park target loaded"
        );
        Ok(())
    }

    /// Mark the coordinate frame anchored (measured or operator-asserted
    /// ground truth arrived) and arm any still-empty park-target axis
    /// from the connect-resolved `preferred_ap_park`.
    ///
    /// Called after a successful `SyncToCoordinates` / `SyncToTarget`
    /// and after a named-park `UnparkFromApPosition`. Axes with a raw
    /// `park_*_ticks` override (or an already-armed pose target) are
    /// left untouched — only the `None` slots left by an unanchored
    /// connect are filled. Best-effort: if the transport parameters are
    /// unavailable the targets stay empty and `Park()` keeps its
    /// park-in-place behaviour.
    pub(super) async fn anchor_frame_and_rearm_park_target(&self) {
        let preferred = {
            let mut s = self.state.write().await;
            s.frame_anchored = true;
            if s.park_ra_ticks.is_some() && s.park_dec_ticks.is_some() {
                return;
            }
            s.preferred_ap_park
        };
        let Some(preferred) = preferred else {
            // No connect ran yet (unit-test-only path) — nothing to arm.
            return;
        };
        if let Some((ra, dec)) = self.ap_park_target_ticks(preferred).await {
            let mut s = self.state.write().await;
            if s.park_ra_ticks.is_none() {
                s.park_ra_ticks = Some(ra);
            }
            if s.park_dec_ticks.is_none() {
                s.park_dec_ticks = Some(dec);
            }
            let ra_target = s.park_ra_ticks;
            let dec_target = s.park_dec_ticks;
            debug!(
                ra_target,
                dec_target,
                preferred_ap_park = ?preferred,
                "frame anchored; park target re-armed"
            );
        }
    }

    /// Safe-stop-then-seed the firmware encoder counter to the given
    /// `(ra, dec)` tick pair. Wraps the bare `:E1` / `:E2` writes in a
    /// stop envelope so the operation is correct regardless of in-flight
    /// firmware state (pending `:G` goto, active `:I` tracking):
    ///
    /// 1. `:K1` / `:K2` (stop both axes) + poll `:f1` / `:f2` until idle.
    /// 2. `:E1` / `:E2` write the seed encoder values, publishing each
    ///    to the cached snapshot so an immediate read reflects it.
    /// 3. Clear the driver-internal slew / target / tracking-request
    ///    flags so the just-written encoder is the source of truth.
    ///
    /// Invoked by [`Self::seed_after_connect`] (where the stops are
    /// no-ops on a fresh-power-up mount — the motors are idle) and by
    /// the `UnparkFromApPosition(ap_park_N)` recovery Action for
    /// `N ≥ 1` (where the stops cancel whatever motion a crash left
    /// running). The standard `Unpark()` flow does **not** call this —
    /// writing the encoder there would silently destroy session state.
    ///
    /// Takes the session explicitly because the connect-time seed runs
    /// *before* the session is stored in `self.session`. On a
    /// `stop_axis_and_wait` failure the encoder writes are **not**
    /// attempted — motion is still in flight and re-seeding then would
    /// race the firmware.
    pub(super) async fn reset_mount_encoders(
        &self,
        session: &Session<SkywatcherCodec>,
        ra_target_ticks: i32,
        dec_target_ticks: i32,
    ) -> ASCOMResult<()> {
        // 1. Stop both axes and wait for idle. A stop failure bails
        //    before any `:E` so we never seed against a moving axis.
        stop_axis_and_wait(&self.manager, session, Axis::Ra, AXIS_STOP_TIMEOUT)
            .await
            .map_err(ASCOMError::from)?;
        stop_axis_and_wait(&self.manager, session, Axis::Dec, AXIS_STOP_TIMEOUT)
            .await
            .map_err(ASCOMError::from)?;
        // 2. Write the seed encoder values and publish them to the
        //    cached snapshot.
        self.manager
            .send(
                session,
                Command::SetPosition {
                    axis: Axis::Ra,
                    ticks: ra_target_ticks,
                },
            )
            .await
            .map_err(ASCOMError::from)?;
        self.manager.seed_ra_position(ra_target_ticks).await;
        self.manager
            .send(
                session,
                Command::SetPosition {
                    axis: Axis::Dec,
                    ticks: dec_target_ticks,
                },
            )
            .await
            .map_err(ASCOMError::from)?;
        self.manager.seed_dec_position(dec_target_ticks).await;
        // 3. Clear driver-internal motion / target / tracking state so
        //    the freshly written encoder is the source of truth.
        self.slew_in_progress.store(false, Ordering::SeqCst);
        let mut state = self.state.write().await;
        state.target_ra_hours = None;
        state.target_dec_degrees = None;
        state.tracking_requested = false;
        Ok(())
    }

    /// Encoder `(ra, dec)` tick pair for an AP park, or [`None`] when
    /// the park has no codebase mapping (`ap_park_0`) or transport
    /// parameters are unavailable (not connected). Resolves against the
    /// configured site latitude and the handshake-reported counts-per-
    /// revolution.
    pub(super) async fn ap_park_target_ticks(&self, park: ApPark) -> Option<(i32, i32)> {
        let mech_ha = park.codebase_mech_ha_hours(self.config.site_latitude_deg)?;
        let dec_deg = park.codebase_dec_encoder_degrees(self.config.site_latitude_deg)?;
        let params = self.manager.parameters().await?;
        Some((
            MechHa::new(mech_ha)
                .to_ticks(Cpr::new(params.cpr_ra))
                .value(),
            MechDec::new(dec_deg)
                .to_ticks(Cpr::new(params.cpr_dec))
                .value(),
        ))
    }

    /// Post-connect encoder seed for the operator's configured
    /// `unpark_from_ap_position`.
    ///
    /// The Sky-Watcher firmware's encoder counter resets to `(0, 0)`
    /// every power-up. For `ap_park_1..ap_park_5` the codebase's
    /// convention for `(0, 0)` doesn't match the operator's physical
    /// pose, so we run [`Self::reset_mount_encoders`] right after connect
    /// (on a fresh power-up the stop steps are no-ops; the `:E1` / `:E2`
    /// encoder writes are the meaningful work) to align the firmware's
    /// encoder counter with the codebase convention for the pose.
    ///
    /// Skipped when:
    /// - The configured pose is `ap_park_0` ("current position"): the
    ///   operator asserts they will plate-solve and `SyncToCoordinates`
    ///   themselves, so the driver does not touch the encoder. No
    ///   `info!()` lines are emitted in this case.
    /// - The firmware reports a non-zero encoder reading at connect time
    ///   **beyond a small fresh-power-up tolerance**. That indicates the
    ///   mount has already been slewed or synced this power cycle —
    ///   re-seeding would clobber it. The tolerance absorbs the
    ///   Sky-Watcher firmware's 1-tick fresh-power-up artifact
    ///   (observed `dec = −1` on the validation `GTi`, ~0.4″).
    ///
    /// Documented operator assumption: when `unpark_from_ap_position`
    /// is one of `ap_park_1..ap_park_5`, the operator powers up the
    /// mount **at** the configured pose and connects the driver before
    /// any slew or sync. Reconnecting mid-session after a slew is safe
    /// (the non-zero-encoder guard catches it — a real slew lands tens
    /// of thousands of ticks away from zero, well outside the tolerance).
    pub(super) async fn seed_after_connect(
        &self,
        session: &Session<SkywatcherCodec>,
        unpark_from_ap_position: ApPark,
    ) -> ASCOMResult<()> {
        // `ap_park_0` ("current position") has no codebase encoder
        // mapping — trust the firmware encoder as-is and emit no logs.
        let (Some(mech_ha), Some(dec_deg)) = (
            unpark_from_ap_position.codebase_mech_ha_hours(self.config.site_latitude_deg),
            unpark_from_ap_position.codebase_dec_encoder_degrees(self.config.site_latitude_deg),
        ) else {
            return Ok(());
        };
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let snap = self.manager.snapshot().await;
        info!(
            pre_seed_ra_ticks = snap.ra.position_ticks,
            pre_seed_dec_ticks = snap.dec.position_ticks,
            unpark_from_ap_position = ?unpark_from_ap_position,
            "pre-seed encoder snapshot at connect"
        );
        if snap.ra.position_ticks.abs() > FRESH_POWER_UP_TICK_TOLERANCE
            || snap.dec.position_ticks.abs() > FRESH_POWER_UP_TICK_TOLERANCE
        {
            debug!(
                ra = snap.ra.position_ticks,
                dec = snap.dec.position_ticks,
                tolerance = FRESH_POWER_UP_TICK_TOLERANCE,
                "skipping unpark_from_ap_position encoder seed: firmware encoder \
                 is non-zero beyond tolerance"
            );
            return Ok(());
        }
        let ra_ticks = MechHa::new(mech_ha)
            .to_ticks(Cpr::new(params.cpr_ra))
            .value();
        let dec_ticks = MechDec::new(dec_deg)
            .to_ticks(Cpr::new(params.cpr_dec))
            .value();
        self.reset_mount_encoders(session, ra_ticks, dec_ticks)
            .await?;
        info!(
            seeded_ra_ticks = ra_ticks,
            seeded_dec_ticks = dec_ticks,
            unpark_from_ap_position = ?unpark_from_ap_position,
            "seeded firmware encoder for unpark_from_ap_position"
        );
        Ok(())
    }

    /// Execute a slew to celestial (ra, dec) on the explicitly-chosen
    /// pier side. Used by both `slew_to_coordinates_async` (where the
    /// side is picked from the flip policy decision tree) and
    /// `set_side_of_pier` (where the user pins the side directly).
    ///
    /// Caller must have already validated: connected, coords in
    /// range, not parked. The helper then:
    ///   1. Computes target encoder ticks for the chosen side
    ///      (pre-flip or post-flip) and validates against the
    ///      per-side safety envelope.
    ///   2. Atomically latches `slew_in_progress` and the
    ///      target RA/Dec.
    ///   3. Computes deltas with `fold_to_canonical_band` (handles a
    ///      post-through-wrap raw encoder) and applies
    ///      `flip_slew_ra_delta` on the RA axis for flip slews
    ///      (forces CCW direction through the safe negative-mech_HA
    ///      half).
    ///   4. Issues the INDI wire sequence per axis and hands off to
    ///      the slew-completion watcher.
    pub(super) async fn execute_slew_with_explicit_side(
        &self,
        ra: f64,
        dec: f64,
        chosen_side: PierSide,
    ) -> ASCOMResult<()> {
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let pre_flip_side = pre_flip_side_for_latitude(self.config.site_latitude_deg);
        let target_is_flipped = chosen_side != pre_flip_side && chosen_side != PierSide::Unknown;
        let (ra_ticks, dec_ticks) = if target_is_flipped {
            target_encoder_flipped(
                Ra::new(ra),
                Dec::new(dec),
                lst,
                Cpr::new(params.cpr_ra),
                Cpr::new(params.cpr_dec),
            )
        } else {
            target_encoder_normal(
                Ra::new(ra),
                Dec::new(dec),
                lst,
                Cpr::new(params.cpr_ra),
                Cpr::new(params.cpr_dec),
            )
        };
        // Refuse before any wire motion if the slew target falls
        // outside the configured mechanical envelope for the chosen
        // pier side.
        self.check_within_safe_envelope(ra, dec, lst.value(), target_is_flipped)?;

        // Reserve the in-progress slot **before** issuing any motion.
        // The returned guard clears `slew_in_progress` on drop, so every
        // `?` below — a failed wire command or a failed watcher hand-off
        // — rolls the flag back without an explicit clear.
        let Some(reservation) = SlewReservation::try_acquire(&self.slew_in_progress) else {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "slew refused: slew already in progress",
            ));
        };
        // Latch the target + capture the tracking flag.
        let tracking_was_on;
        {
            let mut s = self.state.write().await;
            s.target_ra_hours = Some(ra);
            s.target_dec_degrees = Some(dec);
            s.target_pier_side = Some(chosen_side);
            tracking_was_on = s.tracking_requested;
            s.pulse_guiding.ra = false;
            s.pulse_guiding.dec = false;
        }

        // Issue the motion sequence. Any `?` failure inside drops
        // `reservation`, which clears `slew_in_progress` — the driver
        // can't get stuck reporting Slewing after a failed slew.
        let result: ASCOMResult<()> = async {
            let snap = self.manager.snapshot().await;
            let (ra_delta, dec_delta) =
                self.slew_axis_deltas(&snap, &params, ra_ticks, dec_ticks, chosen_side)?;
            // Both axes use the INDI wire sequence: `:K` + poll `:f`
            // (decelerate stop) → `:G goto+fast` → `:I 6` → `:H |delta|`
            // → `:M breaks` → `:J`. The RA-axis `:K` is also the wire
            // event that halts any in-progress sidereal tracking;
            // mirror that into the in-memory `tracking_requested`
            // flag only after the stop has actually succeeded so the
            // state never gets ahead of the wire on transport failures.
            let guard = self.session.read().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| ASCOMError::from(StarAdvError::NotConnected))?;
            self.stop_and_wait(Axis::Ra).await?;
            self.state.write().await.tracking_requested = false;
            issue_slew_axis(&self.manager, session, Axis::Ra, ra_delta)
                .await
                .map_err(ASCOMError::from)?;
            self.stop_and_wait(Axis::Dec).await?;
            issue_slew_axis(&self.manager, session, Axis::Dec, dec_delta)
                .await
                .map_err(ASCOMError::from)?;
            Ok(())
        }
        .await;
        result?;

        // Hand off to the completion watcher. The watcher acquires its
        // own session so the user's disconnect path doesn't have to
        // wait for slews to finish — see `spawn_slew_completion_watcher`.
        let settle = {
            let s = self.state.read().await;
            s.slew_settle_time.unwrap_or(self.config.settle_after_slew)
        };
        spawn_slew_completion_watcher(
            Arc::clone(&self.state),
            Arc::clone(&self.manager),
            Arc::clone(&self.session),
            Arc::clone(&self.slew_in_progress),
            self.config.clone(),
            self.manager.polling_interval_for_watcher(),
            settle,
            tracking_was_on,
        )
        .await
        .map_err(ASCOMError::from)?;
        // Watcher spawned — hand off the flag. If the spawn above had
        // failed, `?` would have dropped `reservation` and rolled the
        // flag back, so a failed hand-off can no longer leave the driver
        // stuck reporting Slewing with no watcher to clear it.
        reservation.dismiss();
        Ok(())
    }

    /// The per-axis wire deltas for a slew from `snap` to the target
    /// encoder pair, flip-aware. Pure: reads only the snapshot,
    /// parameters, and config.
    fn slew_axis_deltas(
        &self,
        snap: &MountSnapshot,
        params: &MountParameters,
        ra_ticks: RaTicks,
        dec_ticks: DecTicks,
        chosen_side: PierSide,
    ) -> ASCOMResult<(i32, i32)> {
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let is_flip_slew = current_side != chosen_side;
        // Fold the raw delta to canonical so a snapshot value that
        // landed outside `[−cpr/2, +cpr/2)` after a prior
        // through-wrap flip doesn't trigger a full-revolution
        // correction here.
        let ra_delta_canonical = ra_ticks
            .canonical_delta_since(
                RaTicks::new(snap.ra.position_ticks),
                Cpr::new(params.cpr_ra),
            )
            .value();
        let binding_zone = self.config.cw_exclusion_zone.bounds();
        let ra_delta = if is_flip_slew {
            // Flip slews steer the polar-axis sweep out of the
            // CW exclusion zone — see
            // [`super::slew::flip_slew_ra_delta`] and the design doc's
            // [§"Through-wrap slew routing"](../../../../docs/services/star-adventurer-gti.md#through-wrap-slew-routing).
            flip_slew_ra_delta(
                ra_delta_canonical,
                snap.ra.position_ticks,
                params.cpr_ra,
                binding_zone,
            )?
        } else {
            // Non-flip slews can't rewrite the direction the way
            // flip slews can — the canonical short delta is the
            // unique path between current and target on the chosen
            // pier side. Refuse if that sweep enters the CW
            // exclusion zone (e.g. cur mech_HA = +0.5 h → target +11.5 h
            // would otherwise sweep the CW through the zone even
            // though both endpoints sit outside it).
            check_non_flip_ra_path(
                ra_delta_canonical,
                snap.ra.position_ticks,
                params.cpr_ra,
                binding_zone,
            )?;
            ra_delta_canonical
        };
        let dec_delta_canonical = dec_ticks
            .canonical_delta_since(
                DecTicks::new(snap.dec.position_ticks),
                Cpr::new(params.cpr_dec),
            )
            .value();
        let dec_delta = if is_flip_slew {
            // Flip slews force the Dec axis to traverse the
            // visible celestial pole (NCP for north, SCP for
            // south) rather than the below-horizon pole — see
            // [`super::slew::flip_slew_dec_delta`].
            flip_slew_dec_delta(
                dec_delta_canonical,
                snap.dec.position_ticks,
                params.cpr_dec,
                self.config.site_latitude_deg >= 0.0,
            )
        } else {
            dec_delta_canonical
        };
        Ok((ra_delta, dec_delta))
    }
}
