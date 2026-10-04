//! ASCOM `ITelescopeV3` trait implementation for [`MountDevice`].
//!
//! The Alpaca client-facing surface — capability flags, coordinate
//! reads, target setters, slew / sync / park / abort / pulse-guide.
//! Heavy lifting (slew geometry, watcher loops, persistence) lives in
//! sibling submodules; methods here orchestrate but rarely compute.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::{
    AlignmentMode, DriveRate, EquatorialCoordinateType, GuideDirection, PierSide, Telescope,
    TelescopeAxis,
};
use ascom_alpaca::api::Device;
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use skywatcher_motor_protocol::{Axis, Command};
use tracing::debug;

use crate::coordinates::{
    encoder_to_celestial, is_flipped_side, local_sidereal_time_hours, pulse_guide_step_period,
    ra_dec_to_alt_az, select_pier_side_for_target, side_of_pier as side_of_pier_calc,
    target_encoder_flipped, target_encoder_normal, SIDEREAL_DEG_PER_SEC,
};
use crate::manager::MountParameters;
use crate::units::{Cpr, Dec, DecTicks, Ra, RaTicks};

use super::inherent::{validate_guide_rate, SideChoice};
use super::park_persistence::write_park_to_config;
use super::slew::enable_sidereal_tracking_ra;
use super::watchers::spawn_park_completion_watcher;
use super::{pre_flip_side_for_latitude, MountDevice, PulseGuiding, SlewReservation};

/// What a guide pulse in one direction does on the wire: which axis it
/// drives, which way, at what multiple of sidereal, and that axis'
/// sidereal step period.
///
/// The sidereal period is derived from the resolved axis rather than
/// picked alongside it, because the period is per-axis: the `GTi`'s Dec
/// axis has fewer counts per revolution than RA, so a Dec pulse sent an
/// RA-derived period guides 1.25× too fast.
///
/// The Dec direction is derived from the pier side for the same reason
/// it cannot be a constant: past a celestial pole the Dec encoder
/// counts against declination, so `guideNorth` is `ccw = false` on the
/// counterweight-down side and `ccw = true` on the counterweight-up one
/// (issue #1300).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct GuidePulse {
    pub(super) axis: Axis,
    pub(super) ccw: bool,
    /// Target rate as a multiple of sidereal: East/West shift RA
    /// tracking down/up by the RA guide fraction; North/South spin Dec
    /// from rest at the Dec guide fraction.
    pub(super) rate_factor: f64,
    pub(super) sidereal_period: u32,
}

impl GuidePulse {
    /// `current_side` is the side [`side_of_pier`] reports for the
    /// mount's present Dec encoder, and `site_latitude_deg` says which
    /// label that hemisphere calls counterweight-up. `PierSide::Unknown`
    /// resolves as counterweight-down.
    ///
    /// [`side_of_pier`]: crate::coordinates::side_of_pier
    pub(super) fn resolve(
        direction: GuideDirection,
        ra_fraction: f64,
        dec_fraction: f64,
        params: &MountParameters,
        current_side: PierSide,
        site_latitude_deg: f64,
    ) -> Self {
        let (axis, ccw, rate_factor) = match direction {
            GuideDirection::East => (Axis::Ra, false, 1.0 - ra_fraction),
            GuideDirection::West => (Axis::Ra, false, 1.0 + ra_fraction),
            GuideDirection::North => (Axis::Dec, false, dec_fraction),
            GuideDirection::South => (Axis::Dec, true, dec_fraction),
        };
        // The table above is the counterweight-down mapping, where the
        // Dec encoder and celestial declination run together. Past a
        // celestial pole `Dec = sign(θ) · (180° − |θ|)`, so the encoder
        // counts the other way and North/South must swap direction for
        // `guideNorth` to keep moving the OTA north (issue #1300). RA
        // needs no such correction: a flip shifts `mech_HA` by 12 h
        // rather than mirroring it, so the East/West rate shifts mean
        // the same thing on both sides.
        let ccw = ccw ^ (axis == Axis::Dec && is_flipped_side(current_side, site_latitude_deg));
        let sidereal_period = if axis == Axis::Ra {
            params.sidereal_step_period_ra()
        } else {
            params.sidereal_step_period_dec()
        };
        Self {
            axis,
            ccw,
            rate_factor,
            sidereal_period,
        }
    }

    /// The `:I` step period that runs the pulsed axis at `rate_factor`
    /// × sidereal, validated against the protocol's 24-bit payload
    /// range — `encode_u24` silently truncates above `0x00FF_FFFF`, so
    /// an un-validated period would wrap to an unintended speed. The
    /// floor is `rate_factor ≥ sidereal_period / 0xFFFFFF`: ≈ 0.023 on
    /// RA (period ≈ 380K) and ≈ 0.028 on Dec (≈ 475K). Tiny guide-rate
    /// fractions trip this; clients see `INVALID_VALUE`.
    pub(super) fn step_period(&self) -> ASCOMResult<u32> {
        const MAX_STEP_PERIOD: u32 = 0x00FF_FFFF;

        let Self {
            rate_factor,
            sidereal_period,
            ..
        } = *self;
        let shifted_period = pulse_guide_step_period(sidereal_period, rate_factor);
        if shifted_period == 0 || shifted_period > MAX_STEP_PERIOD {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                format!(
                    "PulseGuide step period {shifted_period} (rate_factor {rate_factor:.4} × \
                     sidereal_period {sidereal_period}) is outside the protocol's 24-bit \
                     range; pick a guide rate closer to sidereal"
                ),
            ));
        }
        Ok(shifted_period)
    }
}

#[async_trait]
impl Telescope for MountDevice {
    // ---- Capability flags (constants from the design doc) ----

    async fn alignment_mode(&self) -> ASCOMResult<AlignmentMode> {
        Ok(AlignmentMode::GermanPolar)
    }

    async fn equatorial_system(&self) -> ASCOMResult<EquatorialCoordinateType> {
        Ok(EquatorialCoordinateType::Topocentric)
    }

    async fn can_slew(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_slew_async(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_sync(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_set_tracking(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_park(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_unpark(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_set_park(&self) -> ASCOMResult<bool> {
        // SetPark requires a config-file path to persist to. Without
        // one (i.e. the driver was started on `Config::default()`),
        // `SetPark` would have nowhere to write — see the design doc's
        // §"Park persistence" for the rationale. ASCOM permits
        // `CanSetPark` to vary with driver state, so this is a runtime
        // check rather than a compile-time constant.
        Ok(self.config_file_path.is_some())
    }
    async fn can_pulse_guide(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn can_set_pier_side(&self) -> ASCOMResult<bool> {
        // Phase 6: CanSetPierSide tracks `flip_policy.enabled`. With
        // the policy disabled (the shipped default), `SetSideOfPier`
        // returns NOT_IMPLEMENTED — the driver behaves as a
        // non-flipping GEM. With it enabled (only after a successful
        // first real-hardware GTi flip), the slew planner accepts
        // explicit flip requests. See the design doc's
        // [§"Meridian flip"](../../../../docs/services/star-adventurer-gti.md#meridian-flip).
        Ok(self.config.flip_policy.enabled)
    }
    async fn can_set_guide_rates(&self) -> ASCOMResult<bool> {
        Ok(true)
    }
    async fn does_refraction(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn tracking_rates(&self) -> ASCOMResult<Vec<DriveRate>> {
        Ok(vec![DriveRate::Sidereal])
    }

    // ---- Required-by-trait reads ----

    async fn at_home(&self) -> ASCOMResult<bool> {
        Ok(false)
    }

    async fn at_park(&self) -> ASCOMResult<bool> {
        Ok(self.state.read().await.at_park)
    }

    async fn right_ascension(&self) -> ASCOMResult<f64> {
        self.ensure_connected().await?;
        // Encoder carried forward to *now*, to pair with the LST taken
        // now: the raw poll sample is up to a poll old, and while
        // tracking that reads the sky off by the sample's age
        // (issue #1334).
        let snap = self.manager.snapshot_now().await;
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let (ra, _dec) = encoder_to_celestial(
            RaTicks::new(snap.ra.position_ticks),
            DecTicks::new(snap.dec.position_ticks),
            lst,
            Cpr::new(params.cpr_ra),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        Ok(ra.value())
    }

    async fn right_ascension_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn declination(&self) -> ASCOMResult<f64> {
        self.ensure_connected().await?;
        let snap = self.manager.snapshot_now().await;
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let (_ra, dec) = encoder_to_celestial(
            RaTicks::new(snap.ra.position_ticks),
            DecTicks::new(snap.dec.position_ticks),
            lst,
            Cpr::new(params.cpr_ra),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        Ok(dec.value())
    }

    async fn declination_rate(&self) -> ASCOMResult<f64> {
        Ok(0.0)
    }

    async fn azimuth(&self) -> ASCOMResult<f64> {
        let ra = self.right_ascension().await?;
        let dec = self.declination().await?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let (_alt, az) = ra_dec_to_alt_az(
            Ra::new(ra),
            Dec::new(dec),
            self.config.site_latitude_deg,
            lst,
        );
        Ok(az)
    }

    async fn altitude(&self) -> ASCOMResult<f64> {
        let ra = self.right_ascension().await?;
        let dec = self.declination().await?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let (alt, _az) = ra_dec_to_alt_az(
            Ra::new(ra),
            Dec::new(dec),
            self.config.site_latitude_deg,
            lst,
        );
        Ok(alt)
    }

    async fn sidereal_time(&self) -> ASCOMResult<f64> {
        local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map(super::super::units::Lst::value)
            .map_err(ASCOMError::from)
    }

    async fn slewing(&self) -> ASCOMResult<bool> {
        if !self.connected().await? {
            return Ok(false);
        }
        // `slew_in_progress` is true between issuing :J and the watcher
        // task signalling completion (after settle + tracking re-issue),
        // so the flag covers both the active-motion period and the
        // post-motion settle window.
        if self.slew_in_progress.is_held() {
            return Ok(true);
        }
        let snap = self.manager.snapshot().await;
        let ra_slewing = snap.ra.running() && snap.ra.goto();
        let dec_slewing = snap.dec.running() && snap.dec.goto();
        Ok(ra_slewing || dec_slewing)
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        Ok(self.state.read().await.tracking_requested)
    }

    async fn set_tracking(&self, tracking: bool) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        // Take RA from any pulse in flight before mutating it, and keep
        // `axis_ownership` until `Tracking` matches the wire. A restore
        // already on the wire finishes first and a later one sees the
        // pulse gone and sends nothing — without this, `set_tracking(false)`
        // during an East/West pulse would be undone when the pulse
        // restored sidereal — and no new pulse can claim RA against the
        // `Tracking` value this call is about to replace.
        let _axes = self.axis_ownership.lock().await;
        // The RA pulse this call takes over, until RA is stopped.
        let mut taken = PulseGuiding::IDLE;
        {
            let mut s = self.state.write().await;
            taken.set(Axis::Ra, s.pulse_guiding.get(Axis::Ra));
            s.pulse_guiding.set(Axis::Ra, None);
        }
        let result: ASCOMResult<()> = async {
            if tracking {
                // Enabling tracking while parked is invalid per ASCOM
                // ITelescopeV3. Disabling tracking while parked stays
                // allowed — Park itself leaves tracking off, but a caller
                // re-asserting that should not error.
                self.ensure_unparked().await?;
                let params = self
                    .manager
                    .parameters()
                    .await
                    .ok_or(ASCOMError::NOT_CONNECTED)?;
                // Per Sky-Watcher spec §2: "Motor must be at full stop
                // status before setting the motion mode." The RA axis
                // may already be running — from a prior tracking enable,
                // or because the firmware auto-engages Speed (Tracking)
                // Mode after every goto completes. Force a stop and wait
                // for the running flag to clear before re-issuing the
                // tracking-mode `:G`/`:I`/`:J` sequence.
                self.stop_and_wait(Axis::Ra).await?;
                taken = PulseGuiding::IDLE;
                self.with_session(async |session| {
                    enable_sidereal_tracking_ra(&self.manager, session, &params)
                        .await
                        .map_err(ASCOMError::from)
                })
                .await?;
            } else {
                // Decelerate to stop on RA.
                self.send(Command::StopMotion(Axis::Ra))
                    .await
                    .map_err(ASCOMError::from)?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            // RA was not stopped, and the pulse's watcher will send
            // nothing: stop it rather than leave it at a guide rate.
            self.stop_orphaned_axes(taken.axes()).await;
        }
        result?;
        self.state.write().await.tracking_requested = tracking;
        Ok(())
    }

    async fn tracking_rate(&self) -> ASCOMResult<DriveRate> {
        Ok(DriveRate::Sidereal)
    }

    async fn set_tracking_rate(&self, tracking_rate: DriveRate) -> ASCOMResult<()> {
        if tracking_rate != DriveRate::Sidereal {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "MVP supports sidereal tracking only",
            ));
        }
        Ok(())
    }

    async fn utc_date(&self) -> ASCOMResult<SystemTime> {
        Ok(SystemTime::now())
    }

    async fn axis_rates(&self, _axis: TelescopeAxis) -> ASCOMResult<Vec<RangeInclusive<f64>>> {
        Ok(vec![])
    }

    // ---- Site coordinates (configured, read-only) ----

    async fn site_latitude(&self) -> ASCOMResult<f64> {
        Ok(self.config.site_latitude_deg)
    }

    async fn site_longitude(&self) -> ASCOMResult<f64> {
        Ok(self.config.site_longitude_deg)
    }

    async fn site_elevation(&self) -> ASCOMResult<f64> {
        Ok(self.config.site_elevation_m)
    }

    // ---- Side-of-pier read ----

    async fn side_of_pier(&self) -> ASCOMResult<PierSide> {
        self.ensure_connected().await?;
        let snap = self.manager.snapshot().await;
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        Ok(side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        ))
    }

    async fn destination_side_of_pier(&self, ra: f64, dec: f64) -> ASCOMResult<PierSide> {
        // Pure prediction — no wire traffic, no slew. Shares the
        // flip-policy decision tree with `slew_to_coordinates_async`
        // (see the design doc's
        // [§"Pier-side decision tree"](../../../../docs/services/star-adventurer-gti.md#pier-side-decision-tree)),
        // then validates the target against the safety envelope for
        // the chosen side with the same `INVALID_VALUE` rejection a
        // slew would issue. With `flip_policy.enabled = false` (the
        // default) the decision tree collapses to "current side", so
        // any target inside the (pre-flip) safety envelope predicts
        // `pierWest` in the Northern Hemisphere (`pierEast` in the
        // Southern). With it enabled, an opposite side is returned
        // when the current side's envelope rejects the target.
        self.ensure_connected().await?;
        Self::validate_coordinates(ra, dec)?;
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let snap = self.manager.snapshot_now().await;
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        // The selector needs where the mount stands, not just which
        // side it is on: a side is only usable when an RA sweep to it
        // clears the CW exclusion zone.
        let current_mech_ha =
            RaTicks::new(snap.ra.position_ticks).to_mech_ha(Cpr::new(params.cpr_ra));
        let chosen_side = select_pier_side_for_target(
            Ra::new(ra),
            lst,
            current_side,
            current_mech_ha,
            &self.config.flip_policy,
            self.config.cw_exclusion_zone.bounds(),
            self.config.site_latitude_deg,
        );
        let pre_flip_side = pre_flip_side_for_latitude(self.config.site_latitude_deg);
        let target_is_flipped = chosen_side != pre_flip_side && chosen_side != PierSide::Unknown;
        self.check_within_safe_envelope(ra, dec, lst.value(), target_is_flipped)?;
        Ok(chosen_side)
    }

    async fn set_side_of_pier(&self, side_of_pier: PierSide) -> ASCOMResult<()> {
        // Phase 6: explicit meridian-flip trigger. With
        // `flip_policy.enabled = false` (the default), every code path
        // here short-circuits to NOT_IMPLEMENTED — the driver behaves
        // as a non-flipping GEM. With the policy enabled, this method
        // routes through `slew_to_coordinates_async` to the current
        // celestial target with the chosen side. See the design doc's
        // [§"`SetSideOfPier(side)`"](../../../../docs/services/star-adventurer-gti.md#setsideofpierside).
        if !self.config.flip_policy.enabled {
            return Err(ASCOMError::new(
                ASCOMErrorCode::NOT_IMPLEMENTED,
                "SetSideOfPier requires flip_policy.enabled = true",
            ));
        }
        if side_of_pier == PierSide::Unknown {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "SetSideOfPier rejects PierSide::Unknown",
            ));
        }
        self.ensure_connected().await?;
        self.ensure_unparked().await?;
        // Refuse mid-slew. The slew planner also self-refuses via its
        // own `slew_in_progress` check, but rejecting here yields a
        // cleaner error before we read the snapshot and compute a
        // stale celestial target.
        if self.slew_in_progress.is_held() {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "SetSideOfPier refused: slew already in progress",
            ));
        }
        // Compute the mount's current celestial position from the
        // encoder snapshot + LST. A flip slew keeps the OTA on this
        // same celestial direction while landing on the requested
        // pier side.
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        let snap = self.manager.snapshot_now().await;
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        if side_of_pier == current_side {
            // No-op success. Per ASCOM, SetSideOfPier(current_side)
            // is a valid request; we don't issue motion or perturb
            // the in-memory target.
            return Ok(());
        }
        // Read the *celestial* current pointing from the snapshot —
        // `encoder_to_celestial` applies the post-flip RA/Dec mapping
        // when the Dec encoder is past the pole.
        // `execute_slew` will re-compute the target
        // encoder for the chosen side.
        let (cur_ra, cur_dec) = encoder_to_celestial(
            RaTicks::new(snap.ra.position_ticks),
            DecTicks::new(snap.dec.position_ticks),
            lst,
            Cpr::new(params.cpr_ra),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let (cur_ra, cur_dec) = (cur_ra.value(), cur_dec.value());
        // Drive the slew with the chosen-side encoder math directly,
        // bypassing the policy decision tree. The selector's
        // stay-on-current preference is correct for slew_to_coordinates
        // but wrong for an explicit SetSideOfPier — the user pinned the
        // side, honour it.
        self.execute_slew(cur_ra, cur_dec, SideChoice::Pinned(side_of_pier))
            .await
    }

    // ---- Target setters ----

    async fn target_right_ascension(&self) -> ASCOMResult<f64> {
        self.state
            .read()
            .await
            .target_ra_hours
            .ok_or(ASCOMError::INVALID_OPERATION)
    }

    async fn set_target_right_ascension(&self, target_right_ascension: f64) -> ASCOMResult<()> {
        if !(0.0..24.0).contains(&target_right_ascension) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "TargetRightAscension must be in [0, 24) hours",
            ));
        }
        self.state.write().await.target_ra_hours = Some(target_right_ascension);
        Ok(())
    }

    async fn target_declination(&self) -> ASCOMResult<f64> {
        self.state
            .read()
            .await
            .target_dec_degrees
            .ok_or(ASCOMError::INVALID_OPERATION)
    }

    async fn set_target_declination(&self, target_declination: f64) -> ASCOMResult<()> {
        if !(-90.0..=90.0).contains(&target_declination) {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_VALUE,
                "TargetDeclination must be in [-90, +90] degrees",
            ));
        }
        self.state.write().await.target_dec_degrees = Some(target_declination);
        Ok(())
    }

    // ---- Sync ----

    async fn sync_to_coordinates(&self, ra: f64, dec: f64) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        Self::validate_coordinates(ra, dec)?;
        self.ensure_unparked().await?;
        // Take the axes for the duration, the way `Park` does. The
        // encoder pair written below is chosen from the *cached* pier
        // side, and an async slew (a flip most of all) returns as soon
        // as its completion watcher is spawned. A sync overlapping that
        // window reads the pre-flip Dec encoder, resolves the
        // counterweight-down solution, and writes it to a mount already
        // on its way to the other side — re-labelling it, so every
        // later slew plans from a false position. That is the
        // corruption this method's side-awareness exists to prevent,
        // arriving through the back door.
        //
        // `axis_ownership` is what makes it exclusive, not the flag.
        // A bare `load` would not do: the reads below are `.await`
        // points, so a slew could start after the load and be moving
        // by the time the `:E` writes land. Taking the slew's own
        // `SlewReservation` would not do either — `AbortSlew` clears
        // that flag unconditionally, correctly for the motion it
        // cancels, but that would strip a sync of the exclusivity it
        // is relying on and let the next slew in mid-write.
        //
        // So sync holds the one lock no third party can release on its
        // behalf, and a slew or park must take it to reach its own
        // reservation. The flag check below is then sound: while this
        // lock is held no *new* slew can acquire, so a `true` reading
        // means one is already under way and a `false` one cannot go
        // stale. A sync is not motion, so it deliberately does not set
        // the flag — `Slewing` stays honest.
        //
        // A guide pulse in flight is left alone: what it restores does
        // not depend on the encoder position, and its wire bursts take
        // this same lock, so they queue behind the `:E` writes below.
        let _axes = self.axis_ownership.lock().await;
        if self.slew_in_progress.is_held() {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "sync refused: slew already in progress",
            ));
        }
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        // Sync writes the encoder pair for the side the mount is
        // *physically* on — classified from the Dec encoder, exactly as
        // `SideOfPier` classifies it — and validates the target against
        // that side's `mech_HA`. Assuming the pre-flip side
        // unconditionally (as this did until 2026-09) refuses every
        // western target while the mount is counterweight-up, because
        // their pre-flip `mech_HA` sits in a CW exclusion zone the
        // mount is nowhere near; worse, it accepts the eastern ones and
        // writes a pre-flip encoder pair, silently re-labelling a
        // flipped mount as unflipped so every later slew plans from a
        // false position. See the design doc's
        // [§"Sync and pier side"](../../../../docs/services/star-adventurer-gti.md#sync-and-pier-side).
        //
        // Rejecting a sync that would put the encoder outside the safe
        // mechanical envelope stays: a bad sync lets the *next*
        // tracking step push the OTA into a hard stop.
        let snap = self.manager.snapshot_now().await;
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let pre_flip_side = pre_flip_side_for_latitude(self.config.site_latitude_deg);
        // An `Unknown` side (no Dec CPR) is treated as pre-flip — the
        // same fallback the rest of the driver takes when the encoder
        // classification is unavailable.
        let sync_is_flipped = current_side != pre_flip_side && current_side != PierSide::Unknown;
        self.check_within_safe_envelope(ra, dec, lst.value(), sync_is_flipped)?;
        let (ra_ticks, dec_ticks) = if sync_is_flipped {
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
        let (ra_ticks, dec_ticks) = (ra_ticks.value(), dec_ticks.value());
        let (_, ra_written) = self
            .send_timed(Command::SetPosition {
                axis: Axis::Ra,
                ticks: ra_ticks,
            })
            .await
            .map_err(ASCOMError::from)?;
        // Publish the just-written RA position to the cached snapshot
        // so an immediate `RightAscension` read reflects the sync
        // without having to wait for the next background poll. Done
        // only after the wire `:E` succeeds, and dated when the `:E`
        // went out, like a polled sample.
        self.manager
            .seed_ra_position(ra_ticks, ra_written.sent_at)
            .await;
        let (_, dec_written) = self
            .send_timed(Command::SetPosition {
                axis: Axis::Dec,
                ticks: dec_ticks,
            })
            .await
            .map_err(ASCOMError::from)?;
        self.manager
            .seed_dec_position(dec_ticks, dec_written.sent_at)
            .await;
        // Per ASCOM ITelescopeV3, a successful Sync sets
        // TargetRightAscension / TargetDeclination to the synced
        // coordinates. ConformU asserts this. Only write the in-memory
        // target after both `:E` sends succeed so a partial-failure
        // sync doesn't leave Target reflecting a position the mount
        // never actually accepted.
        {
            let mut s = self.state.write().await;
            s.target_ra_hours = Some(ra);
            s.target_dec_degrees = Some(dec);
        }
        // The sync is measured ground truth for the encoder→pose
        // mapping: anchor the frame and arm any park-target axis an
        // unanchored connect left empty, so `Park()` can slew to the
        // preferred AP park from here on.
        self.anchor_frame_and_rearm_park_target().await;
        Ok(())
    }

    async fn sync_to_target(&self) -> ASCOMResult<()> {
        let (ra, dec) = {
            let s = self.state.read().await;
            (
                s.target_ra_hours.ok_or(ASCOMError::INVALID_OPERATION)?,
                s.target_dec_degrees.ok_or(ASCOMError::INVALID_OPERATION)?,
            )
        };
        self.sync_to_coordinates(ra, dec).await
    }

    // ---- Slew (async, target-based, with completion watcher) ----

    async fn slew_to_coordinates_async(&self, ra: f64, dec: f64) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        Self::validate_coordinates(ra, dec)?;
        self.ensure_unparked().await?;
        // The flip policy picks the pier side. With
        // `flip_policy.enabled = false` (the default) the slew stays on
        // the current side; with it enabled, a flip slew may be chosen —
        // see the design doc's
        // [§"Meridian flip"](../../../../docs/services/star-adventurer-gti.md#meridian-flip).
        // The choice is made with the rest of the plan, from where the
        // mount stands.
        self.execute_slew(ra, dec, SideChoice::FlipPolicy).await
    }

    async fn slew_to_target_async(&self) -> ASCOMResult<()> {
        let (ra, dec) = {
            let s = self.state.read().await;
            (
                s.target_ra_hours.ok_or(ASCOMError::INVALID_OPERATION)?,
                s.target_dec_degrees.ok_or(ASCOMError::INVALID_OPERATION)?,
            )
        };
        self.slew_to_coordinates_async(ra, dec).await
    }

    async fn slew_to_coordinates(&self, ra: f64, dec: f64) -> ASCOMResult<()> {
        // ASCOM requires this synchronous variant when CanSlew = true.
        // ConformU flags the trait-default NotImplemented as a spec
        // violation. Implement as: start the async slew, then await the
        // completion watcher by polling `Slewing` until it clears.
        self.slew_to_coordinates_async(ra, dec).await?;
        self.await_slew_complete().await
    }

    async fn slew_to_target(&self) -> ASCOMResult<()> {
        self.slew_to_target_async().await?;
        self.await_slew_complete().await
    }

    // ---- Park / Unpark / Abort ----

    async fn park(&self) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        // Idempotent: already parked → no-op.
        if self.state.read().await.at_park {
            return Ok(());
        }
        // Reserve the in-progress slot **before** issuing any motion —
        // a concurrent `SetPark` must not read mid-slew encoder
        // positions. The guard clears `slew_in_progress` on drop, so any
        // `?` failure below (or a failed watcher hand-off) rolls it back
        // without an explicit clear.
        // Serialize with an in-flight sync's encoder writes before
        // claiming the axes; see `axis_ownership`. Held only across the
        // acquisition — park's own ownership is the reservation, which
        // it hands to the park watcher.
        // Park takes both axes from any pulse in flight under the same
        // lock, so no pulse restore lands between here and its motion.
        // `taken` holds those pulses until park has stopped their axes
        // itself.
        let mut taken = PulseGuiding::IDLE;
        let reservation = {
            let _axes = self.axis_ownership.lock().await;
            let reservation =
                SlewReservation::try_acquire(&self.slew_in_progress, &self.axis_ownership);
            if reservation.is_some() {
                taken = std::mem::replace(
                    &mut self.state.write().await.pulse_guiding,
                    PulseGuiding::IDLE,
                );
            }
            reservation
        };
        let Some(reservation) = reservation else {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "park refused: slew already in progress",
            ));
        };
        // Issue the motion sequence in an inner future. Any `?` failure
        // drops `reservation`, which clears `slew_in_progress` — no
        // explicit rollback needed.
        let result: ASCOMResult<()> = async {
            // Stop tracking before slewing home (per ASCOM, tracking
            // remains off after Park). The wire `:K1` is issued first
            // so the in-memory flag flip only follows a successful stop,
            // and under the claim's axes guard, so a park an abort has
            // already voided sends no stop into a successor's goto.
            {
                let claim = reservation.claim();
                let Some(_axes) = claim.hold_axes().await else {
                    return Err(ASCOMError::new(
                        ASCOMErrorCode::INVALID_OPERATION,
                        "park aborted before it started",
                    ));
                };
                if self.state.read().await.tracking_requested {
                    self.send(Command::StopMotion(Axis::Ra))
                        .await
                        .map_err(ASCOMError::from)?;
                    self.state.write().await.tracking_requested = false;
                }
            }
            // Per-axis park target: `Some` from a raw config override
            // or (anchored frame) the `preferred_ap_park` pose; `None`
            // when the frame is unanchored with no override — that
            // axis parks IN PLACE. A goto from an unanchored frame
            // would slew to a fabricated position (workspace tenet:
            // no actuation on connect).
            let (target_ra_ticks, target_dec_ticks) = {
                let s = self.state.read().await;
                (s.park_ra_ticks, s.park_dec_ticks)
            };
            // Same wire sequence as `slew_to_coordinates_async`:
            // `:K`-and-wait, `:G` with direction chosen from
            // `sign(target - current)`, `:S target`, `:J`. Both axes
            // are stopped BEFORE the positions that pick the goto
            // direction are read: a direction computed from a pre-stop
            // reading could point the long way around if an axis was
            // still moving (tracking, in-flight slew) when Park was
            // called.
            self.stop_and_wait_claimed(&reservation.claim(), Axis::Ra)
                .await?;
            taken.set(Axis::Ra, None);
            self.stop_and_wait_claimed(&reservation.claim(), Axis::Dec)
                .await?;
            taken.set(Axis::Dec, None);
            // The fresh read of where the axes stopped happens in here,
            // under the claim's axes guard, with the gotos.
            self.start_park_gotos(&reservation.claim(), (target_ra_ticks, target_dec_ticks))
                .await
        }
        .await;
        if result.is_err() {
            self.stop_taken_pulse_axes_claimed(&reservation.claim(), taken)
                .await;
        }
        result?;
        // Hand off to the park watcher; it owns `slew_in_progress` from
        // here and will clear it on completion. The watcher acquires its
        // own session so a user disconnect during park doesn't have to
        // wait for completion.
        let settle = self
            .state
            .read()
            .await
            .slew_settle_time
            .unwrap_or(self.config.settle_after_slew);
        spawn_park_completion_watcher(
            Arc::clone(&self.state),
            Arc::clone(&self.manager),
            Arc::clone(&self.session),
            reservation.claim(),
            self.manager.polling_interval_for_watcher(),
            settle,
        )
        .await
        .map_err(ASCOMError::from)?;
        reservation.dismiss();
        Ok(())
    }

    async fn unpark(&self) -> ASCOMResult<()> {
        // Unpark does NOT auto-enable tracking.
        self.state.write().await.at_park = false;
        Ok(())
    }

    async fn set_park(&self) -> ASCOMResult<()> {
        // Capability gate: without a config-file path we have nowhere
        // to persist to. `CanSetPark` advertises `false` in this case,
        // but ASCOM clients are allowed to call setters whose
        // capability is `false` and expect `NOT_IMPLEMENTED`.
        let config_path = self.config_file_path.as_ref().ok_or_else(|| {
            ASCOMError::new(
                ASCOMErrorCode::NOT_IMPLEMENTED,
                "SetPark requires the driver to be started with --config <path>",
            )
        })?;
        self.ensure_connected().await?;
        // Refuse mid-slew: the "current encoder pair" wouldn't be
        // stable while the motors are still moving. Also catches
        // mid-park: AtPark hasn't been set yet but slew_in_progress is.
        //
        // Two layers of defense for the concurrent-motion case (per
        // Copilot review on PR #221, comment 3242621736):
        //   1. The in-memory `slew_in_progress` flag: park() and
        //      slew_to_coordinates_async() now set this *before*
        //      issuing motion (with rollback-on-error), so the
        //      flag observation here is reliable.
        //   2. A fresh wire read of each axis' `running` flag (below):
        //      defense in depth against an axis that's running for any
        //      reason the in-memory flag wouldn't capture (a tracking
        //      pulse, an external `:J` from a future out-of-band path,
        //      a flag-set racing the wire send).
        if self.slew_in_progress.is_held() {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "SetPark refused while slew or park is in progress",
            ));
        }
        // Read the encoder pair **fresh** from the wire, not from the
        // background poll snapshot. SetPark captures the *current*
        // encoder pair, but the cached snapshot lags the wire by up to
        // one `polling_interval` — reading it could persist a stale
        // position when the operator moved the mount out-of-band just
        // before SetPark. The lag also made the BDD persistence scenario
        // flaky on slow CI (issue #308): the eager service-start
        // handshake seeds the snapshot *before* the test sets the
        // encoder, and the user's connect is a refcount bump rather than
        // a fresh handshake, so the captured position hinged on a
        // background poll landing in that gap. A synchronous
        // `poll_axes_now` removes the timing dependency (and refreshes
        // the cache as a side effect). Same session-read idiom as
        // `set_tracking` / `stop_and_wait`.
        let snap = self
            .with_session(async |session| {
                self.manager
                    .poll_axes_now(session)
                    .await
                    .map_err(ASCOMError::from)
            })
            .await?;
        if snap.ra.running() || snap.dec.running() {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "SetPark refused while an axis is running per the wire snapshot",
            ));
        }
        let ra_ticks = snap.ra.position_ticks;
        let dec_ticks = snap.dec.position_ticks;
        // Disk I/O runs on the blocking pool so the async runtime
        // isn't held up while we read+parse+stage+fsync+rename. Same
        // pattern as `services/rp/src/persistence/document.rs::write_sidecar`.
        let path = config_path.clone();
        tokio::task::spawn_blocking(move || write_park_to_config(&path, ra_ticks, dec_ticks))
            .await
            .map_err(|e| {
                ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    format!("set_park write task join error: {e}"),
                )
            })?
            .map_err(ASCOMError::from)?;
        // Only mutate the in-memory target after the disk write
        // succeeds — otherwise a failed write would leave the live
        // park target out of sync with what's persisted.
        let mut s = self.state.write().await;
        s.park_ra_ticks = Some(ra_ticks);
        s.park_dec_ticks = Some(dec_ticks);
        drop(s);
        debug!(
            ra_ticks,
            dec_ticks,
            path = ?config_path,
            "set_park persisted to config file"
        );
        Ok(())
    }

    async fn abort_slew(&self) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        // Aborting while parked is invalid per ASCOM ITelescopeV3.
        // Refuse before mutating any state so a caller that mistakenly
        // calls AbortSlew on a parked mount gets a clean error without
        // side-effects on tracking_requested or slew_in_progress.
        self.ensure_unparked().await?;
        // Take the axes before touching the flag, and hold them through
        // the stops. Without this, abort's own ordering — clear the
        // flag, *then* `await` the `:L` sends — hands a waiting sync a
        // `false` reading while the original motion is still running,
        // and its `:E` writes land mid-slew. `axis_ownership` is what
        // makes the flag check inside sync sound, so the operation that
        // falsifies the flag has to hold it too.
        //
        // Blocking here is bounded by the longest holder: a
        // `Tracking = true` write's RA stop-and-wait and restart, up to
        // about 2 s; otherwise a sync's two encoder writes or a pulse's
        // burst. It is the right order anyway: an abort arriving mid-sync
        // should let the position write finish rather than interleave
        // with it.
        let _axes = self.axis_ownership.lock().await;
        // Empty the slew slot first, voiding the claim of the slew or
        // park in flight: its watcher bails before clobbering the
        // snapshot or at_park flag, and a slew still waiting on its
        // stops starts no goto (or stops the one it just started).
        // Also clear tracking_requested — `:L` halts any motion the
        // mount is doing including any sidereal tracking the watcher
        // may have re-issued. After abort the user must explicitly
        // re-enable tracking. Matches ASCOM's "AbortSlew does not
        // auto-restore tracking" guarantee.
        self.slew_in_progress.clear();
        {
            let mut s = self.state.write().await;
            s.tracking_requested = false;
            // Cancel any pulse in flight on either axis. Its watcher
            // sends nothing when it wakes; `:L1`/`:L2` below already
            // halt any rate-shifted motion.
            s.pulse_guiding = PulseGuiding::IDLE;
        }
        // Issue :L on both axes (instant stop). Log the underlying
        // transport error if either send fails — silent failure here
        // hides bugs (a watcher race that leaves the manager with no
        // open transport, for instance) until BDD assertions on the
        // command log time out far downstream.
        if let Err(e) = self.send(Command::InstantStop(Axis::Ra)).await {
            debug!("abort_slew :L1 send failed: {e}");
        }
        if let Err(e) = self.send(Command::InstantStop(Axis::Dec)).await {
            debug!("abort_slew :L2 send failed: {e}");
        }
        Ok(())
    }

    // ---- Slew settle time (read/write, lives in the in-memory mirror) ----

    async fn slew_settle_time(&self) -> ASCOMResult<Duration> {
        Ok(self
            .state
            .read()
            .await
            .slew_settle_time
            .unwrap_or(self.config.settle_after_slew))
    }

    async fn set_slew_settle_time(&self, slew_settle_time: Duration) -> ASCOMResult<()> {
        self.state.write().await.slew_settle_time = Some(slew_settle_time);
        Ok(())
    }

    // ---- PulseGuide ----

    async fn is_pulse_guiding(&self) -> ASCOMResult<bool> {
        Ok(self.state.read().await.pulse_guiding.is_active())
    }

    async fn guide_rate_right_ascension(&self) -> ASCOMResult<f64> {
        let f = self.state.read().await.guide_rate_ra_fraction;
        Ok(f * SIDEREAL_DEG_PER_SEC)
    }

    async fn set_guide_rate_right_ascension(
        &self,
        guide_rate_right_ascension: f64,
    ) -> ASCOMResult<()> {
        let fraction = validate_guide_rate(guide_rate_right_ascension)?;
        self.state.write().await.guide_rate_ra_fraction = fraction;
        Ok(())
    }

    async fn guide_rate_declination(&self) -> ASCOMResult<f64> {
        let f = self.state.read().await.guide_rate_dec_fraction;
        Ok(f * SIDEREAL_DEG_PER_SEC)
    }

    async fn set_guide_rate_declination(&self, guide_rate_declination: f64) -> ASCOMResult<()> {
        let fraction = validate_guide_rate(guide_rate_declination)?;
        self.state.write().await.guide_rate_dec_fraction = fraction;
        Ok(())
    }

    async fn pulse_guide(&self, direction: GuideDirection, duration: Duration) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        self.ensure_unparked().await?;
        if self.slewing().await? {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "PulseGuide refused while slewing",
            ));
        }
        // Duration zero is a no-op success per ASCOM convention. Skip
        // before resolving direction / acquiring locks to keep the
        // hot-path predictable.
        if duration.is_zero() {
            return Ok(());
        }
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        // Which way a Dec pulse has to turn depends on whether the Dec
        // axis sits past a celestial pole, so the pulse resolves
        // against the side the mount is on — the same Dec-encoder
        // classification `SideOfPier` reports, read from the same
        // background-poll snapshot.
        //
        // Sampled once, at pulse start. A slew or auto-flip that starts
        // before the pulse claims its axis is refused by the claim, which
        // re-checks for one under `axis_ownership`; one that starts later
        // takes the axis from the pulse under the same lock before it
        // moves. So the side a pulse resolved is never applied to an axis
        // a slew has since moved.
        //
        // The sample also cannot straddle a pole crossing *within* one
        // pulse. That needs the OTA to start within the pulse's own
        // travel of the celestial pole — 37.6″ for a 5 s pulse at the
        // default rate — where declination genuinely peaks and comes
        // back down whichever way the encoder turns. See the design
        // doc's Dec sign convention.
        let current_side = side_of_pier_calc(
            DecTicks::new(self.manager.snapshot().await.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let pulse = {
            let s = self.state.read().await;
            GuidePulse::resolve(
                direction,
                s.guide_rate_ra_fraction,
                s.guide_rate_dec_fraction,
                &params,
                current_side,
                self.config.site_latitude_deg,
            )
        };
        // Claiming the axis, choosing the wire shape (a live rate change
        // on a tracking RA axis, or a start from rest) and handing the
        // pulse to its watcher all happen in `start_pulse`; see the
        // `pulse` module.
        self.start_pulse(direction, pulse, duration, &params).await
    }
}
