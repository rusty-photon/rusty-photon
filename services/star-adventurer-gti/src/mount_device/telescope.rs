//! ASCOM `ITelescopeV3` trait implementation for [`MountDevice`].
//!
//! The Alpaca client-facing surface — capability flags, coordinate
//! reads, target setters, slew / sync / park / abort / pulse-guide.
//! Heavy lifting (slew geometry, watcher loops, persistence) lives in
//! sibling submodules; methods here orchestrate but rarely compute.

use std::ops::RangeInclusive;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ascom_alpaca::api::telescope::{
    AlignmentMode, DriveRate, EquatorialCoordinateType, GuideDirection, PierSide, Telescope,
    TelescopeAxis,
};
use ascom_alpaca::api::Device;
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult};
use async_trait::async_trait;
use skywatcher_motor_protocol::command::{ModeKind, MotionMode, Speed};
use skywatcher_motor_protocol::{Axis, Command};
use tracing::debug;

use crate::coordinates::{
    encoder_to_celestial, local_sidereal_time_hours, pulse_guide_step_period, ra_dec_to_alt_az,
    select_pier_side_for_target, side_of_pier as side_of_pier_calc, sidereal_step_period,
    SIDEREAL_DEG_PER_SEC,
};
use crate::units::{Cpr, Dec, DecTicks, Ra, RaTicks};

use super::inherent::validate_guide_rate;
use super::park_persistence::write_park_to_config;
use super::slew::enable_sidereal_tracking_ra;
use super::watchers::{clear_pulse_flag, spawn_park_completion_watcher, spawn_pulse_guide_watcher};
use super::{pre_flip_side_for_latitude, MountDevice, SlewReservation};

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
        let snap = self.manager.snapshot().await;
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
        let snap = self.manager.snapshot().await;
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
        if self.slew_in_progress.load(Ordering::SeqCst) {
            return Ok(true);
        }
        let snap = self.manager.snapshot().await;
        let ra_slewing = snap.ra.running && snap.ra.goto;
        let dec_slewing = snap.dec.running && snap.dec.goto;
        Ok(ra_slewing || dec_slewing)
    }

    async fn tracking(&self) -> ASCOMResult<bool> {
        Ok(self.state.read().await.tracking_requested)
    }

    async fn set_tracking(&self, tracking: bool) -> ASCOMResult<()> {
        self.ensure_connected().await?;
        // Cancel any in-flight RA pulse before mutating the RA axis.
        // The pulse-guide watcher's post-sleep restore step checks
        // `pulse_guiding.ra` and bails if cleared. Without this,
        // `set_tracking(false)` during an East/West pulse would be
        // silently undone when the watcher re-issued sidereal tracking
        // on restore.
        self.state.write().await.pulse_guiding.ra = false;
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
            let guard = self.session.read().await;
            let session = guard.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;
            enable_sidereal_tracking_ra(&self.manager, session, &params)
                .await
                .map_err(ASCOMError::from)?;
        } else {
            // Decelerate to stop on RA.
            self.send(Command::StopMotion(Axis::Ra))
                .await
                .map_err(ASCOMError::from)?;
        }
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
        let snap = self.manager.snapshot().await;
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let chosen_side = select_pier_side_for_target(
            Ra::new(ra),
            lst,
            current_side,
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
        if self.slew_in_progress.load(Ordering::SeqCst) {
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
        let snap = self.manager.snapshot().await;
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
        // `execute_slew_with_explicit_side` will re-compute the target
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
        self.execute_slew_with_explicit_side(cur_ra, cur_dec, side_of_pier)
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
        // Cancel any in-flight pulse-guide on either axis — sync is
        // an axis-position mutation and we don't want the watcher
        // restoring tracking against the freshly-set encoder position.
        {
            let mut s = self.state.write().await;
            s.pulse_guiding.ra = false;
            s.pulse_guiding.dec = false;
        }
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;
        // Reject syncs that would set the encoder outside the
        // mount's safe mechanical envelope — a bad sync would let
        // the *next* tracking step push the OTA into a hard stop.
        // Sync uses the pre-flip envelope (`target_is_flipped =
        // false`); operators must `AbortSlew` and re-sync the pre-
        // flip pointing first if a manual flip left the mount in a
        // post-flip state.
        self.check_within_safe_envelope(ra, dec, lst.value(), false)?;
        let mech_ha = lst.hour_angle_of(Ra::new(ra)).to_mech();
        let ra_ticks = mech_ha.to_ticks(Cpr::new(params.cpr_ra)).value();
        let dec_ticks = Dec::new(dec)
            .to_mech()
            .to_ticks(Cpr::new(params.cpr_dec))
            .value();
        self.send(Command::SetPosition {
            axis: Axis::Ra,
            ticks: ra_ticks,
        })
        .await
        .map_err(ASCOMError::from)?;
        // Publish the just-written RA position to the cached snapshot
        // so an immediate `RightAscension` read reflects the sync
        // without having to wait for the next background poll. Done
        // only after the wire `:E` succeeds.
        self.manager.seed_ra_position(ra_ticks).await;
        self.send(Command::SetPosition {
            axis: Axis::Dec,
            ticks: dec_ticks,
        })
        .await
        .map_err(ASCOMError::from)?;
        self.manager.seed_dec_position(dec_ticks).await;
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
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;

        // Compute target encoder ticks for the *current* LST. INDI's
        // EQMOD-style post-stop pickup loop (issue #205) handles the
        // residual that arises because RA drifts during the goto: when
        // the watcher detects both axes stopped, it reads the actual
        // RA/Dec, computes the residual against the latched target,
        // and re-issues a corrective goto if the residual exceeds the
        // INDI tolerance (`RAGOTORESOLUTION = 5"`). Earlier revisions
        // sidestepped this by pre-shifting LST by `MIN_SLEW_DWELL` —
        // that bounded mock drift but undershot real-hardware slews
        // of 3-7 s, leaving 45-120 arc-second RA residuals. The
        // pickup loop closes the gap cleanly.
        let lst = local_sidereal_time_hours(SystemTime::now(), self.config.site_longitude_deg)
            .map_err(ASCOMError::from)?;

        // Phase 6: determine target pier side via the flip policy. With
        // `flip_policy.enabled = false` (the default), `chosen_side`
        // always equals `current_side` and the rest of this function
        // reduces to the pre-Phase-6 pipeline. With it enabled, a
        // flip slew may be chosen — see the design doc's
        // [§"Meridian flip"](../../../../docs/services/star-adventurer-gti.md#meridian-flip).
        let snap = self.manager.snapshot().await;
        let current_side = side_of_pier_calc(
            DecTicks::new(snap.dec.position_ticks),
            Cpr::new(params.cpr_dec),
            self.config.site_latitude_deg,
        );
        let chosen_side = select_pier_side_for_target(
            Ra::new(ra),
            lst,
            current_side,
            &self.config.flip_policy,
            self.config.cw_exclusion_zone.bounds(),
            self.config.site_latitude_deg,
        );
        self.execute_slew_with_explicit_side(ra, dec, chosen_side)
            .await
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
        let Some(reservation) = SlewReservation::try_acquire(&self.slew_in_progress) else {
            return Err(ASCOMError::new(
                ASCOMErrorCode::INVALID_OPERATION,
                "park refused: slew already in progress",
            ));
        };
        // Cancel any in-flight pulse-guide — park takes ownership of
        // both axes from this point.
        {
            let mut s = self.state.write().await;
            s.pulse_guiding.ra = false;
            s.pulse_guiding.dec = false;
        }
        // Issue the motion sequence in an inner future. Any `?` failure
        // drops `reservation`, which clears `slew_in_progress` — no
        // explicit rollback needed.
        let result: ASCOMResult<()> = async {
            // Stop tracking before slewing home (per ASCOM, tracking
            // remains off after Park). The wire `:K1` is issued first
            // so the in-memory flag flip only follows a successful stop.
            if self.state.read().await.tracking_requested {
                self.send(Command::StopMotion(Axis::Ra))
                    .await
                    .map_err(ASCOMError::from)?;
                self.state.write().await.tracking_requested = false;
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
            self.stop_and_wait(Axis::Ra).await?;
            self.stop_and_wait(Axis::Dec).await?;
            // Fresh wire read after the stops — the cached background
            // snapshot lags the wire by up to one `polling_interval`.
            let snap = {
                let guard = self.session.read().await;
                let session = guard.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;
                self.manager
                    .poll_axes_now(session)
                    .await
                    .map_err(ASCOMError::from)?
            };
            for (axis, current_ticks, target_ticks) in [
                (Axis::Ra, snap.ra.position_ticks, target_ra_ticks),
                (Axis::Dec, snap.dec.position_ticks, target_dec_ticks),
            ] {
                let Some(target_ticks) = target_ticks else {
                    debug!(
                        ?axis,
                        "no park target (unanchored frame) — axis parks in place"
                    );
                    continue;
                };
                let mode = MotionMode {
                    kind: ModeKind::Goto,
                    speed: Speed::Fast,
                    ccw: current_ticks > target_ticks,
                };
                self.send(Command::SetMotionMode { axis, mode })
                    .await
                    .map_err(ASCOMError::from)?;
                // No `:I` in Goto mode — the firmware computes slew speed
                // internally. See the matching note in
                // `slew_to_coordinates_async`.
                self.send(Command::SetGotoTarget {
                    axis,
                    ticks: target_ticks,
                })
                .await
                .map_err(ASCOMError::from)?;
                self.send(Command::StartMotion(axis))
                    .await
                    .map_err(ASCOMError::from)?;
            }
            Ok(())
        }
        .await;
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
            Arc::clone(&self.slew_in_progress),
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
        if self.slew_in_progress.load(Ordering::SeqCst) {
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
        let snap = {
            let guard = self.session.read().await;
            let session = guard.as_ref().ok_or(ASCOMError::NOT_CONNECTED)?;
            self.manager
                .poll_axes_now(session)
                .await
                .map_err(ASCOMError::from)?
        };
        if snap.ra.running || snap.dec.running {
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
        // Clear slew_in_progress first so the slew/park watchers see the
        // abort and bail before clobbering the snapshot or at_park flag.
        // Also clear tracking_requested — `:L` halts any motion the
        // mount is doing including any sidereal tracking the watcher
        // may have re-issued. After abort the user must explicitly
        // re-enable tracking. Matches ASCOM's "AbortSlew does not
        // auto-restore tracking" guarantee.
        self.slew_in_progress.store(false, Ordering::SeqCst);
        {
            let mut s = self.state.write().await;
            s.tracking_requested = false;
            // Cancel any in-flight pulse-guide on either axis. The
            // watcher's post-sleep restore step bails when it sees the
            // flag cleared; `:L1`/`:L2` below already halt any
            // rate-shifted motion, so there's nothing for the watcher
            // to restore.
            s.pulse_guiding.ra = false;
            s.pulse_guiding.dec = false;
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
        let s = self.state.read().await;
        Ok(s.pulse_guiding.ra || s.pulse_guiding.dec)
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
        const MAX_STEP_PERIOD: u32 = 0x00FF_FFFF;

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
        // Resolve direction → (axis, ccw, rate_factor) under a read
        // lock. The in-flight check + flag-set happens later under a
        // write lock so it's atomic against concurrent same-axis
        // calls (the rate_factor / tracking_was_on snapshots taken
        // here are stable: rates can be updated concurrently, but
        // the worst case is a one-tick-late read which ASCOM
        // tolerates).
        let (axis, ccw, rate_factor, tracking_was_on) = {
            let s = self.state.read().await;
            let (axis, ccw, rate_factor) = match direction {
                GuideDirection::East => (Axis::Ra, false, 1.0 - s.guide_rate_ra_fraction),
                GuideDirection::West => (Axis::Ra, false, 1.0 + s.guide_rate_ra_fraction),
                GuideDirection::North => (Axis::Dec, false, s.guide_rate_dec_fraction),
                GuideDirection::South => (Axis::Dec, true, s.guide_rate_dec_fraction),
            };
            let tracking_was_on = axis == Axis::Ra && s.tracking_requested;
            (axis, ccw, rate_factor, tracking_was_on)
        };
        // Compute the shifted step period from the cached
        // sidereal-period helper and the rate factor. Validate against
        // the protocol's 24-bit `:I` payload range before sending —
        // `encode_u24` silently truncates above `0x00FF_FFFF`, so an
        // un-validated period would wrap to an unintended speed.
        // For sidereal_period ≈ 380K on the GTi, the floor is
        // `rate_factor ≥ sidereal_period / 0xFFFFFF ≈ 0.023`. Tiny
        // guide-rate fractions trip this; clients see `INVALID_VALUE`.
        let params = self
            .manager
            .parameters()
            .await
            .ok_or(ASCOMError::NOT_CONNECTED)?;
        let sidereal_period = sidereal_step_period(params.tmr_freq, Cpr::new(params.cpr_ra));
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
        // Atomically check `pulse_guiding_<axis>` and set it to true
        // under a single write lock. This closes the TOCTOU window: a
        // concurrent same-axis `pulse_guide` either acquires the
        // write lock first (and we see the flag set on the next read),
        // or acquires it later (and sees our flag). Without the
        // atomic set, the previous flow let a concurrent caller pass
        // the in-flight check while we were still awaiting the
        // `:K`/`:G`/`:I`/`:J` sends. `axis` is always `Ra` or `Dec`
        // here — `GuideDirection` only resolves to those two — so the
        // boolean dispatch is exhaustive without a third branch.
        let is_ra = axis == Axis::Ra;
        {
            let mut s = self.state.write().await;
            let already_in_flight = if is_ra {
                s.pulse_guiding.ra
            } else {
                s.pulse_guiding.dec
            };
            if already_in_flight {
                return Err(ASCOMError::new(
                    ASCOMErrorCode::INVALID_OPERATION,
                    "PulseGuide refused while a same-axis pulse is in flight",
                ));
            }
            if is_ra {
                s.pulse_guiding.ra = true;
            } else {
                s.pulse_guiding.dec = true;
            }
        }
        // Wire path: `:K<axis>` (decelerate and wait for the running
        // flag to clear so `:G` doesn't return `!2 MotorNotStopped`),
        // `:G<axis>` (Tracking + ccw), `:I<axis>` (shifted period),
        // `:J<axis>`. Any failure on the wire rolls back the
        // `pulse_guiding_<axis>` flag so the next caller isn't blocked
        // by a half-applied pulse, and so `IsPulseGuiding` reports
        // false consistent with the lack of actual motion.
        let mode = MotionMode {
            kind: ModeKind::Tracking,
            speed: Speed::Slow,
            ccw,
        };
        let wire_result: ASCOMResult<()> = async {
            self.stop_and_wait(axis).await?;
            self.send(Command::SetMotionMode { axis, mode })
                .await
                .map_err(ASCOMError::from)?;
            self.send(Command::SetStepPeriod {
                axis,
                period: shifted_period,
            })
            .await
            .map_err(ASCOMError::from)?;
            self.send(Command::StartMotion(axis))
                .await
                .map_err(ASCOMError::from)?;
            Ok(())
        }
        .await;
        if let Err(e) = wire_result {
            clear_pulse_flag(&self.state, axis).await;
            return Err(e);
        }
        spawn_pulse_guide_watcher(
            Arc::clone(&self.state),
            Arc::clone(&self.manager),
            Arc::clone(&self.session),
            Arc::clone(&self.slew_in_progress),
            axis,
            duration,
            tracking_was_on,
        )
        .await
        .map_err(ASCOMError::from)?;
        debug!(?direction, ?duration, axis = ?axis, "pulse_guide spawned");
        Ok(())
    }
}
