//! The polar-alignment workflow: three-point measurement, then the
//! live adjustment loop. Publishes progress into the shared status
//! the HTTP layer serves on `GET /status`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, warn};

use crate::config::{MeasurementMode, PolarAlignConfig, SiteConfig};
use crate::ephemeris::EphemerisCtx;
use crate::error::{PolarAlignError, Result};
use crate::math::{
    alignment_errors, attitude_from_wcs, axis_from_attitudes, axis_from_three_points,
    relative_rotation, rotation_between, unit_from_radec, wcs_pixel_to_sky, wcs_sky_to_pixel,
    AlignmentErrors, Mat3, SolvedFrame, Vec3,
};
use crate::mcp_client::{DetectedStar, McpClient, SolveResult};

/// Measurement targets below this observed altitude abort before any
/// motion — near-horizon exposures are refraction-dominated garbage.
const MIN_TARGET_ALTITUDE_DEG: f64 = 10.0;

/// The cross-check separation (2′) above which the plane-normal and
/// attitude-based axes disagreeing is worth an operator's attention.
const CROSS_CHECK_WARN_ARCSEC: f64 = 120.0;

/// Workflow lifecycle, serialized verbatim into `/status.phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Measuring,
    Adjusting,
    Complete,
    Error,
}

/// The measurement block of `/status`, refreshed by every adjustment
/// solve as the operator converges.
#[derive(Debug, Clone, Serialize)]
pub struct MeasurementStatus {
    pub axis_azimuth_deg: f64,
    pub axis_altitude_deg: f64,
    pub azimuth_error_arcmin: f64,
    pub altitude_error_arcmin: f64,
    pub total_error_arcmin: f64,
    pub azimuth_direction: String,
    pub altitude_direction: String,
    /// Angular separation between the primary axis and the alternate
    /// method's axis (plane-normal vs attitude-based, whichever the
    /// mode does not use). A measurement-phase result; absent when
    /// the alternate method had nothing to work with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cross_check_arcsec: Option<f64>,
    pub measured_at: DateTime<Utc>,
}

/// A detected star paired with the pixel it will occupy once the
/// axis sits on the pole. Both pairs are 0-based pixel indices into
/// the adjustment capture — the convention `detect_stars` reports.
#[derive(Debug, Clone, Serialize)]
pub struct StarTarget {
    pub x: f64,
    pub y: f64,
    pub target_x: f64,
    pub target_y: f64,
}

/// The adjustment block of `/status`.
#[derive(Debug, Clone, Serialize)]
pub struct AdjustmentStatus {
    pub updated_at: DateTime<Utc>,
    pub image_path: String,
    pub in_frame: bool,
    pub stars: Vec<StarTarget>,
    pub last_solve: String,
    pub consecutive_solve_failures: u32,
    pub iterations: u32,
}

/// Everything `GET /status` serves.
#[derive(Debug, Clone, Serialize)]
pub struct StatusState {
    pub phase: Phase,
    pub workflow_id: Option<String>,
    /// The measurement point (2 or 3) a `manual_rotation` workflow is
    /// waiting on; the workflow is paused until `POST
    /// /measure/continue` or `measurement.manual_timeout`. Never set
    /// in the other modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting_point: Option<u8>,
    pub measurement: Option<MeasurementStatus>,
    pub adjustment: Option<AdjustmentStatus>,
    pub error: Option<String>,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            workflow_id: None,
            awaiting_point: None,
            measurement: None,
            adjustment: None,
            error: None,
        }
    }
}

/// Handles shared between the HTTP layer and the running workflow.
///
/// The finish signal is per-invocation: `Notify` stores a permit, so
/// a `/adjust/finish` that races the end of one run must not end the
/// next run's adjustment the instant it starts. Re-arming installs a
/// fresh `Notify`, and any stale permit dies with the old one. The
/// proceed signal works the same way but is re-armed per *wait*: a
/// duplicate `/measure/continue` must not skip the next manual
/// point.
#[derive(Clone, Default)]
pub struct WorkflowShared {
    pub status: Arc<RwLock<StatusState>>,
    /// The most recent captured frame, measurement or adjustment —
    /// what `GET /preview.png` renders. Kept across invocations (a
    /// completed run's last frame stays viewable).
    pub latest_image: Arc<RwLock<Option<PathBuf>>>,
    finish: Arc<RwLock<Arc<Notify>>>,
    proceed: Arc<RwLock<Arc<Notify>>>,
}

impl WorkflowShared {
    /// Installs a fresh finish signal for a new invocation.
    pub async fn arm_finish(&self) {
        *self.finish.write().await = Arc::new(Notify::new());
    }

    /// The finish signal of the current invocation.
    pub async fn finish_signal(&self) -> Arc<Notify> {
        self.finish.read().await.clone()
    }

    /// Signals the current invocation's adjustment loop to end.
    pub async fn signal_finish(&self) {
        self.finish.read().await.notify_one();
    }

    /// Installs and returns a fresh proceed signal for one manual
    /// wait, killing any permit a stale `/measure/continue` stored.
    pub async fn arm_proceed(&self) -> Arc<Notify> {
        let fresh = Arc::new(Notify::new());
        *self.proceed.write().await = fresh.clone();
        fresh
    }

    /// Signals the active manual wait to proceed.
    pub async fn signal_proceed(&self) {
        self.proceed.read().await.notify_one();
    }

    /// Records the most recent captured frame for `GET /preview.png`.
    pub async fn record_latest_image(&self, path: &str) {
        *self.latest_image.write().await = Some(PathBuf::from(path));
    }
}

/// What the completion report carries.
#[derive(Debug)]
pub struct WorkflowSummary {
    pub final_measurement: MeasurementStatus,
    pub adjustment_iterations: u32,
}

fn measurement_status(
    axis_azimuth_deg: f64,
    axis_altitude_deg: f64,
    errors: &AlignmentErrors,
    measured_at: DateTime<Utc>,
) -> MeasurementStatus {
    MeasurementStatus {
        axis_azimuth_deg,
        axis_altitude_deg,
        azimuth_error_arcmin: errors.azimuth_error_deg * 60.0,
        altitude_error_arcmin: errors.altitude_error_deg * 60.0,
        total_error_arcmin: errors.total_error_deg * 60.0,
        azimuth_direction: errors.azimuth_direction().to_string(),
        altitude_direction: errors.altitude_direction().to_string(),
        cross_check_arcsec: None,
        measured_at,
    }
}

fn solved_frame(solve: &SolveResult) -> Option<SolvedFrame> {
    solve.wcs_matrix.map(|m| SolvedFrame {
        center_ra_deg: solve.ra_center,
        center_dec_deg: solve.dec_center,
        matrix: m.into(),
    })
}

/// Run the full polar-alignment workflow. The caller (routes) owns
/// posting the completion report from the returned summary or error.
pub async fn run(
    mcp: &McpClient,
    config: &PolarAlignConfig,
    shared: &WorkflowShared,
) -> Result<WorkflowSummary> {
    let result = run_inner(mcp, config, shared).await;

    if let Err(ref e) = result {
        warn!(error = %e, "polar alignment workflow failed");
        // Stop-class cleanup only (tenet 3): halt any in-flight
        // motion, leave the mount tracking where it stands. abort_slew
        // is a no-op error when nothing is slewing; that is fine.
        // Manual rotation never commands motion (and its rig may
        // register no mount tools at all), so there is nothing to
        // stop.
        if config.measurement.mode != MeasurementMode::ManualRotation {
            if let Err(abort_err) = mcp.abort_slew().await {
                debug!(error = %abort_err, "cleanup abort_slew reported (expected when no slew is in flight)");
            }
        }
        let mut status = shared.status.write().await;
        status.phase = Phase::Error;
        status.awaiting_point = None;
        status.error = Some(e.to_string());
    }

    result
}

/// The observer site a workflow runs against: the config's `site`
/// block when present, else rp's `get_site`. rp-sourced coordinates
/// pass the exact same newtype validation as configured ones.
async fn resolve_site(mcp: &McpClient, config: &PolarAlignConfig) -> Result<SiteConfig> {
    if let Some(site) = config.site {
        return Ok(site);
    }
    let rp_site = mcp.get_site().await.map_err(|e| {
        PolarAlignError::Workflow(format!(
            "no `site` in the polar-align config and rp's `get_site` did not return one \
             ({e}); set the polar-align `site` block, or make sure rp is reachable and \
             has a `site` block"
        ))
    })?;
    debug!(
        latitude_deg = rp_site.latitude_deg,
        longitude_deg = rp_site.longitude_deg,
        "site resolved from rp"
    );
    serde_json::from_value(serde_json::json!({
        "latitude_deg": rp_site.latitude_deg,
        "longitude_deg": rp_site.longitude_deg,
    }))
    .map_err(|e| {
        PolarAlignError::Workflow(format!(
            "rp's configured site is unusable for polar alignment ({e}); \
             set a usable `site` in the polar-align config or fix rp's `site` block"
        ))
    })
}

async fn run_inner(
    mcp: &McpClient,
    config: &PolarAlignConfig,
    shared: &WorkflowShared,
) -> Result<WorkflowSummary> {
    let site = resolve_site(mcp, config).await?;
    let eph = &EphemerisCtx::new(site, &config.refraction)?;

    // Preflight: sensor bounds for the overlay, park and tracking
    // state before any motion. Manual rotation drives no mount and
    // skips the mount preflight entirely.
    let camera = mcp.get_camera_info(&config.camera_id).await?;
    if config.measurement.mode != MeasurementMode::ManualRotation {
        mount_preflight(mcp).await?;
    }

    let plan = plan_measurement(mcp, config, eph).await?;
    let sweep = measure_sweep(mcp, config, shared, plan).await?;
    let (mut axis, cross_check_arcsec) = resolve_axis(config.measurement.mode, eph, &sweep)?;
    let mut final_measurement =
        publish_measured_axis(shared, eph, axis, cross_check_arcsec).await?;

    // Adjustment loop. The attitude seed comes from the last
    // measurement solve when it carried a wcs_matrix; otherwise the
    // first matrix-bearing adjustment solve seeds it (the axis simply
    // doesn't move until then).
    let mut attitude_prev: Option<Mat3> = match sweep.last_solve.as_ref().and_then(solved_frame) {
        Some(frame) => Some(attitude_from_wcs(&frame)?),
        None => None,
    };
    let mut hint = sweep.last_capture_center;
    let mut consecutive_failures = 0u32;
    let mut iterations = 0u32;
    // `sleep` computes now + duration itself, saturating to a far-future
    // deadline on overflow, so no overflowing `Instant` add is needed here.
    let max_duration_expired = tokio::time::sleep(config.adjustment.max_duration);
    tokio::pin!(max_duration_expired);

    let finish = shared.finish_signal().await;
    loop {
        tokio::select! {
            () = finish.notified() => {
                info!("adjustment finished by operator");
                break;
            }
            () = &mut max_duration_expired => {
                info!("adjustment reached its configured maximum duration");
                break;
            }
            () = tokio::time::sleep(config.adjustment.interval) => {}
        }

        iterations = iterations.saturating_add(1);
        match adjustment_iteration(
            mcp,
            config,
            eph,
            &mut axis,
            &mut attitude_prev,
            &mut hint,
            (camera.sensor_x, camera.sensor_y),
        )
        .await
        {
            Ok((mut measurement, mut adjustment)) => {
                consecutive_failures = 0;
                adjustment.consecutive_solve_failures = 0;
                adjustment.iterations = iterations;
                shared.record_latest_image(&adjustment.image_path).await;
                // The cross-check is a measurement-phase result;
                // adjustment solves refresh the errors, not it.
                measurement.cross_check_arcsec = cross_check_arcsec;
                final_measurement = measurement.clone();
                let mut status = shared.status.write().await;
                status.measurement = Some(measurement);
                status.adjustment = Some(adjustment);
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                debug!(
                    error = %e,
                    consecutive_failures,
                    "adjustment iteration failed (expected while the operator moves the mount)"
                );
                if consecutive_failures >= config.adjustment.max_solve_failures {
                    return Err(PolarAlignError::Workflow(format!(
                        "{consecutive_failures} consecutive adjustment solves failed (last: {e}); aborting — \
                         check sky conditions"
                    )));
                }
                let mut status = shared.status.write().await;
                let adjustment = status.adjustment.get_or_insert_with(|| AdjustmentStatus {
                    updated_at: Utc::now(),
                    image_path: String::new(),
                    in_frame: false,
                    stars: Vec::new(),
                    last_solve: "failed".to_string(),
                    consecutive_solve_failures: 0,
                    iterations: 0,
                });
                adjustment.updated_at = Utc::now();
                adjustment.last_solve = "failed".to_string();
                adjustment.consecutive_solve_failures = consecutive_failures;
                adjustment.iterations = iterations;
            }
        }
    }

    {
        let mut status = shared.status.write().await;
        status.phase = Phase::Complete;
    }

    Ok(WorkflowSummary {
        final_measurement,
        adjustment_iterations: iterations,
    })
}

/// How the three measurement points come about: commanded slew
/// targets, or the operator rotating the axis by hand between
/// exposures.
enum MeasurementPlan {
    Commanded {
        targets: [(f64, f64); 3],
        slew_first: bool,
    },
    Manual,
}

/// What the three-point sweep measured: unit vectors and camera
/// attitudes per point, plus the last solve (the adjustment loop's
/// attitude seed) and its field center (the first adjustment hint).
struct SweepOutcome {
    centers: [Vec3; 3],
    attitudes: [Option<Mat3>; 3],
    last_solve: Option<SolveResult>,
    last_capture_center: (f64, f64),
}

/// Unpark and enable tracking before any commanded motion, refusing
/// when the mount supports neither.
async fn mount_preflight(mcp: &McpClient) -> Result<()> {
    let park = mcp.get_park_state().await?;
    if park.at_park {
        if !park.can_unpark {
            return Err(PolarAlignError::Workflow(
                "mount is parked and does not support unparking; unpark it manually first"
                    .to_string(),
            ));
        }
        debug!("mount is parked; unparking");
        mcp.unpark().await?;
    }

    let tracking = mcp.get_tracking().await?;
    if !tracking.tracking {
        if !tracking.can_set_tracking {
            return Err(PolarAlignError::Workflow(
                "tracking is off and the mount does not support enabling it; slews would fail"
                    .to_string(),
            ));
        }
        debug!("enabling sidereal tracking");
        mcp.set_tracking(true).await?;
    }
    Ok(())
}

/// Plan the three-point measurement sweep. Targets are planned once up
/// front (the ~arcminute of sidereal drift while the sweep runs is
/// immaterial — the measurement uses solved positions, commanded
/// ones only place the points on one pier side). Manual rotation
/// plans nothing: the operator moves the axis between exposures.
async fn plan_measurement(
    mcp: &McpClient,
    config: &PolarAlignConfig,
    eph: &EphemerisCtx,
) -> Result<MeasurementPlan> {
    let planned_at = Utc::now();
    let plan = match config.measurement.mode {
        MeasurementMode::NearPole => {
            // Equal-declination points on one side of the meridian,
            // hour angles first + i·sweep.
            let dec_deg = config.measurement_dec_deg(eph.hemisphere_sign());
            let ha_sign = config.measurement.direction.ha_sign();
            let lst_deg = eph.lst_hours(planned_at) * 15.0;
            let targets = [0.0_f64, 1.0, 2.0].map(|i| {
                let ha_deg = ha_sign
                    * (config.measurement.first_point_ha_deg.degrees()
                        + i * config.measurement.sweep_deg.degrees());
                ((lst_deg - ha_deg).rem_euclid(360.0), dec_deg)
            });
            MeasurementPlan::Commanded {
                targets,
                slew_first: true,
            }
        }
        MeasurementMode::CurrentPosition => {
            let position = mcp.get_mount_position().await?;
            let ha0_deg = (eph.lst_hours(planned_at) * 15.0 - position.ra_deg + 180.0)
                .rem_euclid(360.0)
                - 180.0;
            debug!(
                ra_deg = position.ra_deg,
                dec_deg = position.dec_deg,
                ha0_deg,
                "measuring from the current mount position"
            );
            MeasurementPlan::Commanded {
                targets: current_position_targets(
                    position.ra_deg,
                    position.dec_deg,
                    ha0_deg,
                    config.measurement.sweep_deg.degrees(),
                ),
                slew_first: false,
            }
        }
        MeasurementMode::ManualRotation => MeasurementPlan::Manual,
    };
    if let MeasurementPlan::Commanded { targets, .. } = &plan {
        require_targets_above_horizon(eph, planned_at, targets)?;
    }
    Ok(plan)
}

/// Run the three-point sweep: per point, position (slew or operator
/// rotation), capture, and plate-solve.
async fn measure_sweep(
    mcp: &McpClient,
    config: &PolarAlignConfig,
    shared: &WorkflowShared,
    plan: MeasurementPlan,
) -> Result<SweepOutcome> {
    let slew_first = matches!(
        plan,
        MeasurementPlan::Commanded {
            slew_first: true,
            ..
        }
    );
    // A manual point carries no hint: there is no trustworthy prediction
    // of where the operator left the axis, so the solve runs blind.
    let hints: [Option<(f64, f64)>; 3] = match plan {
        MeasurementPlan::Commanded { targets, .. } => targets.map(Some),
        MeasurementPlan::Manual => [None; 3],
    };

    let mut centers = [Vec3::new(0.0, 0.0, 0.0); 3];
    let mut attitudes: [Option<Mat3>; 3] = [None; 3];
    let mut last_solve: Option<SolveResult> = None;
    let mut last_capture_center = (0.0, 0.0);

    for (point, (hint, (center, attitude))) in (1u8..).zip(
        hints
            .into_iter()
            .zip(centers.iter_mut().zip(attitudes.iter_mut())),
    ) {
        match hint {
            Some((ra_deg, dec_deg)) => {
                if point > 1 || slew_first {
                    debug!(point, ra_deg, dec_deg, "slewing to measurement point");
                    mcp.slew(ra_deg, dec_deg, config.measurement.settle).await?;
                } else {
                    debug!(point, ra_deg, dec_deg, "capturing the first point in place");
                }
            }
            None => {
                if point > 1 {
                    wait_for_manual_rotation(shared, config.measurement.manual_timeout, point)
                        .await?;
                } else {
                    debug!(point, "capturing the first point in place");
                }
            }
        }

        let capture = mcp
            .capture(&config.camera_id, config.measurement.exposure)
            .await?;
        shared.record_latest_image(&capture.image_path).await;
        let solve = mcp
            .plate_solve(
                &capture.image_path,
                &capture.document_id,
                hint,
                config.solve.search_radius_deg,
                config.solve.timeout,
            )
            .await?;
        debug!(
            point,
            ra_center = solve.ra_center,
            dec_center = solve.dec_center,
            "measurement point solved"
        );
        *center = unit_from_radec(solve.ra_center, solve.dec_center);
        *attitude = measurement_attitude(config.measurement.mode, point, &solve)?;
        last_capture_center = (solve.ra_center, solve.dec_center);
        last_solve = Some(solve);
    }

    Ok(SweepOutcome {
        centers,
        attitudes,
        last_solve,
        last_capture_center,
    })
}

/// Axis: primary per mode, the alternate method as a best-effort
/// cross-check (plan D9). The cross-check is `None` when the alternate
/// method has no usable inputs.
fn resolve_axis(
    mode: MeasurementMode,
    eph: &EphemerisCtx,
    sweep: &SweepOutcome,
) -> Result<(Vec3, Option<f64>)> {
    let SweepOutcome {
        centers, attitudes, ..
    } = sweep;
    let toward = eph.pole_hemisphere_unit();
    let (axis, cross_check_arcsec) = match mode {
        MeasurementMode::NearPole => {
            let primary = axis_from_three_points(centers[0], centers[1], centers[2], toward)?;
            let cross = attitudes
                .iter()
                .copied()
                .collect::<Option<Vec<Mat3>>>()
                .and_then(|list| match axis_from_attitudes(&list, toward) {
                    Ok(alternate) => Some(alternate.angle_to(primary).to_degrees() * 3600.0),
                    Err(e) => {
                        debug!(error = %e, "attitude-based cross-check unavailable");
                        None
                    }
                });
            (primary, cross)
        }
        MeasurementMode::CurrentPosition | MeasurementMode::ManualRotation => {
            let list: Vec<Mat3> = attitudes.iter().copied().flatten().collect();
            let primary = axis_from_attitudes(&list, toward)?;
            let cross = match axis_from_three_points(centers[0], centers[1], centers[2], toward) {
                Ok(alternate) => Some(alternate.angle_to(primary).to_degrees() * 3600.0),
                Err(e) => {
                    debug!(error = %e, "plane-normal cross-check unavailable");
                    None
                }
            };
            (primary, cross)
        }
    };
    if let Some(arcsec) = cross_check_arcsec {
        if arcsec > CROSS_CHECK_WARN_ARCSEC {
            warn!(
                cross_check_arcsec = arcsec,
                "the plane-normal and attitude-based axes disagree by more than 2 arcminutes; \
                 the solves may be poor"
            );
        } else {
            debug!(cross_check_arcsec = arcsec, "axis method cross-check");
        }
    }
    Ok((axis, cross_check_arcsec))
}

/// Convert the measured axis to alt/az errors, publish the measurement
/// under `Phase::Adjusting`, and return it (the adjustment loop's
/// initial `final_measurement`).
async fn publish_measured_axis(
    shared: &WorkflowShared,
    eph: &EphemerisCtx,
    axis: Vec3,
    cross_check_arcsec: Option<f64>,
) -> Result<MeasurementStatus> {
    let now = Utc::now();
    let observed = eph.observed_of(axis, now)?;
    let pole = eph.pole_target_alt_az();
    let errors = alignment_errors(
        observed.azimuth_degrees,
        observed.altitude_degrees,
        pole.azimuth_degrees,
        pole.altitude_degrees,
    );
    info!(
        azimuth_error_arcmin = errors.azimuth_error_deg * 60.0,
        altitude_error_arcmin = errors.altitude_error_deg * 60.0,
        "polar axis measured"
    );

    let mut measurement = measurement_status(
        observed.azimuth_degrees,
        observed.altitude_degrees,
        &errors,
        now,
    );
    measurement.cross_check_arcsec = cross_check_arcsec;
    {
        let mut status = shared.status.write().await;
        status.measurement = Some(measurement.clone());
        status.phase = Phase::Adjusting;
    }
    Ok(measurement)
}

/// Pauses a `manual_rotation` measurement until the operator posts
/// `/measure/continue`, bounded by `measurement.manual_timeout`. The
/// proceed signal is re-armed *before* `awaiting_point` is
/// published, so a post can only land on the fresh signal — a stale
/// or duplicate post from an earlier wait cannot skip this one.
async fn wait_for_manual_rotation(
    shared: &WorkflowShared,
    timeout: Duration,
    point: u8,
) -> Result<()> {
    let signal = shared.arm_proceed().await;
    {
        let mut status = shared.status.write().await;
        status.awaiting_point = Some(point);
    }
    debug!(
        point,
        "waiting for the operator to rotate the RA axis and POST /measure/continue"
    );
    let waited = tokio::time::timeout(timeout, signal.notified()).await;
    {
        let mut status = shared.status.write().await;
        status.awaiting_point = None;
    }
    match waited {
        Ok(()) => {
            debug!(point, "operator confirmed the rotation");
            Ok(())
        }
        Err(_) => Err(PolarAlignError::Workflow(format!(
            "operator did not confirm the rotation to measurement point {point} within {}; \
             aborting",
            humantime::format_duration(timeout)
        ))),
    }
}

/// Whether a mode's axis is extracted from full camera attitudes —
/// which makes every measurement solve's `wcs_matrix` mandatory.
const fn requires_attitudes(mode: MeasurementMode) -> bool {
    matches!(
        mode,
        MeasurementMode::CurrentPosition | MeasurementMode::ManualRotation
    )
}

/// The camera attitude of a measurement solve. `near_pole` measures
/// from centers alone, so a missing or degenerate `wcs_matrix` only
/// costs the cross-check; the attitude-based modes have no axis
/// without full attitudes and must abort naming the point.
fn measurement_attitude(
    mode: MeasurementMode,
    point: u8,
    solve: &SolveResult,
) -> Result<Option<Mat3>> {
    match solved_frame(solve) {
        Some(frame) => match attitude_from_wcs(&frame) {
            Ok(attitude) => Ok(Some(attitude)),
            Err(e) if requires_attitudes(mode) => Err(PolarAlignError::Workflow(format!(
                "measurement point {point} solve has an unusable wcs_matrix ({e}); this \
                 measurement mode needs full camera attitudes"
            ))),
            Err(e) => {
                debug!(
                    point,
                    error = %e,
                    "measurement solve yielded no usable attitude; the cross-check is skipped"
                );
                Ok(None)
            }
        },
        None if requires_attitudes(mode) => Err(PolarAlignError::Workflow(format!(
            "measurement point {point} solve carried no wcs_matrix; this measurement mode \
             needs full camera attitudes"
        ))),
        None => {
            debug!(
                point,
                "measurement solve carried no wcs_matrix; the cross-check is skipped"
            );
            Ok(None)
        }
    }
}

/// The three equal-declination targets of a current-position sweep:
/// anchored on the mount's own reported position and stepping away
/// from the meridian on the side the mount already stands (positive
/// hour angle = west of the meridian), so an RA-only sweep can never
/// cross it and invite a `GoTo` flip. Keeping the mount's *reported*
/// declination is what guarantees the dec axis never moves.
fn current_position_targets(
    ra0_deg: f64,
    dec0_deg: f64,
    ha0_deg: f64,
    sweep_deg: f64,
) -> [(f64, f64); 3] {
    // Hour angle grows westward as RA shrinks; sweep toward larger
    // |hour angle| on the current side.
    let ra_step = if ha0_deg >= 0.0 {
        -sweep_deg
    } else {
        sweep_deg
    };
    [0.0_f64, 1.0, 2.0].map(|i| ((ra0_deg + i * ra_step).rem_euclid(360.0), dec0_deg))
}

/// Rejects any measurement target below the altitude floor before
/// the mount moves at all.
fn require_targets_above_horizon(
    eph: &EphemerisCtx,
    at: DateTime<Utc>,
    targets: &[(f64, f64)],
) -> Result<()> {
    for (point, &(ra_deg, dec_deg)) in (1u8..).zip(targets) {
        let observed = eph.observed_of(unit_from_radec(ra_deg, dec_deg), at)?;
        if observed.altitude_degrees < MIN_TARGET_ALTITUDE_DEG {
            return Err(PolarAlignError::Workflow(format!(
                "measurement point {point} (RA {ra_deg:.1}°, dec {dec_deg:.1}°) sits at \
                 altitude {:.1}°, below the {MIN_TARGET_ALTITUDE_DEG}° floor; start from a \
                 higher pointing",
                observed.altitude_degrees
            )));
        }
    }
    Ok(())
}

/// One adjustment iteration: capture, solve, update the axis through
/// the attitude change, recompute errors, and build the star/target
/// overlay.
#[allow(clippy::too_many_arguments)]
async fn adjustment_iteration(
    mcp: &McpClient,
    config: &PolarAlignConfig,
    eph: &EphemerisCtx,
    axis: &mut Vec3,
    attitude_prev: &mut Option<Mat3>,
    hint: &mut (f64, f64),
    sensor: (u32, u32),
) -> Result<(MeasurementStatus, AdjustmentStatus)> {
    let capture = mcp
        .capture(&config.camera_id, config.adjustment.exposure)
        .await?;
    let solve = mcp
        .plate_solve(
            &capture.image_path,
            &capture.document_id,
            Some(*hint),
            config.solve.search_radius_deg,
            config.solve.timeout,
        )
        .await?;
    *hint = (solve.ra_center, solve.dec_center);

    let frame = solved_frame(&solve).ok_or_else(|| {
        PolarAlignError::Workflow(
            "solve carried no wcs_matrix; the adjustment loop needs the full WCS".to_string(),
        )
    })?;
    let attitude_now = attitude_from_wcs(&frame)?;
    if let Some(prev) = attitude_prev.take() {
        let rotation = relative_rotation(prev, attitude_now);
        *axis = rotation.mul_vec(*axis).normalized()?;
    }
    *attitude_prev = Some(attitude_now);

    let now = Utc::now();
    let observed = eph.observed_of(*axis, now)?;
    let pole = eph.pole_target_alt_az();
    let errors = alignment_errors(
        observed.azimuth_degrees,
        observed.altitude_degrees,
        pole.azimuth_degrees,
        pole.altitude_degrees,
    );

    // Star/target overlay: where each bright star lands once the
    // axis is on the pole. Correction rotation in ICRS; the target
    // pixel of a star at sky direction s is W⁻¹(R_corrᵀ · s).
    let target_icrs = eph.axis_target_icrs(now)?;
    let correction_t = rotation_between(*axis, target_icrs)?.transpose();

    let detected = mcp
        .detect_stars(&capture.image_path, &capture.document_id)
        .await?;
    let mut brightest: Vec<_> = detected
        .stars
        .into_iter()
        .filter(|s| s.saturated_pixel_count == 0)
        .collect();
    brightest.sort_by(|a, b| b.flux.total_cmp(&a.flux));
    brightest.truncate(config.adjustment.star_count);

    let (stars, in_frame) = star_overlay(&frame, correction_t, brightest, sensor)?;

    Ok((
        measurement_status(
            observed.azimuth_degrees,
            observed.altitude_degrees,
            &errors,
            now,
        ),
        AdjustmentStatus {
            updated_at: now,
            image_path: capture.image_path,
            in_frame,
            stars,
            last_solve: "ok".to_string(),
            consecutive_solve_failures: 0,
            iterations: 0,
        },
    ))
}

/// Pairs each detected star with the pixel it will occupy once the
/// correction rotation is applied, and reports whether every target
/// stays on the sensor. Detected centroids are 0-based array indices
/// while the WCS math speaks FITS 1-based pixels, so coordinates
/// shift by one on the way into the WCS and back on the way out;
/// `/status` publishes both halves of each pair 0-based.
fn star_overlay(
    frame: &SolvedFrame,
    correction_t: Mat3,
    brightest: Vec<DetectedStar>,
    sensor: (u32, u32),
) -> Result<(Vec<StarTarget>, bool)> {
    let mut stars = Vec::with_capacity(brightest.len());
    let mut in_frame = !brightest.is_empty();
    for star in brightest {
        let sky = wcs_pixel_to_sky(frame, star.x + 1.0, star.y + 1.0)?;
        match wcs_sky_to_pixel(frame, correction_t.mul_vec(sky))? {
            Some((fits_x, fits_y)) => {
                if !(1.0..=f64::from(sensor.0)).contains(&fits_x)
                    || !(1.0..=f64::from(sensor.1)).contains(&fits_y)
                {
                    in_frame = false;
                }
                stars.push(StarTarget {
                    x: star.x,
                    y: star.y,
                    target_x: fits_x - 1.0,
                    target_y: fits_y - 1.0,
                });
            }
            None => in_frame = false,
        }
    }
    Ok((stars, in_frame))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_status_state_defaults_to_idle() {
        let status = StatusState::default();
        assert_eq!(status.phase, Phase::Idle);
        assert!(status.workflow_id.is_none());
        assert!(status.awaiting_point.is_none());
        assert!(status.measurement.is_none());
        assert!(status.adjustment.is_none());
    }

    /// `awaiting_point` is a manual-mode-only field and must not
    /// clutter the wire format of the other modes.
    #[test]
    fn test_awaiting_point_is_omitted_from_status_json_unless_set() {
        let status = StatusState::default();
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("awaiting_point").is_none());
        let waiting = StatusState {
            awaiting_point: Some(2),
            ..Default::default()
        };
        let json = serde_json::to_value(&waiting).unwrap();
        assert_eq!(json["awaiting_point"], 2);
    }

    #[test]
    fn test_phase_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Phase::Adjusting).unwrap(),
            "\"adjusting\""
        );
        assert_eq!(serde_json::to_string(&Phase::Idle).unwrap(), "\"idle\"");
    }

    #[test]
    fn test_measurement_status_converts_degrees_to_arcminutes() {
        let errors = AlignmentErrors {
            azimuth_error_deg: 0.35,
            altitude_error_deg: -0.2,
            total_error_deg: 0.4,
        };
        let status = measurement_status(0.5, 48.2, &errors, Utc::now());
        assert!((status.azimuth_error_arcmin - 21.0).abs() < 1e-9);
        assert!((status.altitude_error_arcmin + 12.0).abs() < 1e-9);
        assert!((status.total_error_arcmin - 24.0).abs() < 1e-9);
        assert_eq!(status.azimuth_direction, "move azimuth west");
        assert_eq!(status.altitude_direction, "raise altitude");
    }

    #[test]
    fn test_solved_frame_requires_the_matrix() {
        let json = r#"{
            "ra_center": 52.1, "dec_center": 85.2,
            "pixel_scale_arcsec": 1.05, "rotation_deg": 12.0
        }"#;
        let solve: SolveResult = serde_json::from_str(json).unwrap();
        assert!(solved_frame(&solve).is_none());
    }

    fn matrixless_solve() -> SolveResult {
        serde_json::from_str(
            r#"{
                "ra_center": 52.1, "dec_center": 85.2,
                "pixel_scale_arcsec": 1.05, "rotation_deg": 12.0
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn test_measurement_attitude_matrixless_aborts_the_attitude_based_modes() {
        let solve = matrixless_solve();
        let near_pole = measurement_attitude(MeasurementMode::NearPole, 3, &solve).unwrap();
        assert!(near_pole.is_none(), "near-pole degrades to no cross-check");
        for mode in [
            MeasurementMode::CurrentPosition,
            MeasurementMode::ManualRotation,
        ] {
            let err = measurement_attitude(mode, 3, &solve).unwrap_err();
            assert!(err.to_string().contains("point 3"), "{mode:?}: {err}");
            assert!(
                err.to_string().contains("full camera attitudes"),
                "{mode:?}: {err}"
            );
        }
    }

    /// A matrix that exists but is degenerate must fail the same way
    /// as a missing one: naming the point, current-position only.
    #[test]
    fn test_measurement_attitude_degenerate_matrix_names_the_point() {
        let solve: SolveResult = serde_json::from_str(
            r#"{
                "ra_center": 52.1, "dec_center": 85.2,
                "pixel_scale_arcsec": 1.05, "rotation_deg": 0.0,
                "wcs_matrix": {
                    "crpix1": 512.0, "crpix2": 384.0,
                    "cd1_1": 0.0, "cd1_2": 0.0, "cd2_1": 0.0, "cd2_2": 0.0
                }
            }"#,
        )
        .unwrap();
        let near_pole = measurement_attitude(MeasurementMode::NearPole, 2, &solve).unwrap();
        assert!(near_pole.is_none(), "near-pole degrades to no cross-check");
        let err = measurement_attitude(MeasurementMode::CurrentPosition, 2, &solve).unwrap_err();
        assert!(err.to_string().contains("point 2"), "{err}");
        assert!(err.to_string().contains("unusable wcs_matrix"), "{err}");
    }

    #[test]
    fn test_measurement_attitude_extracts_from_a_matrix_bearing_solve() {
        let solve: SolveResult = serde_json::from_str(
            r#"{
                "ra_center": 52.1, "dec_center": 85.2,
                "pixel_scale_arcsec": 1.05, "rotation_deg": 0.0,
                "wcs_matrix": {
                    "crpix1": 512.0, "crpix2": 384.0,
                    "cd1_1": -2.9e-4, "cd1_2": 0.0, "cd2_1": 0.0, "cd2_2": 2.9e-4
                }
            }"#,
        )
        .unwrap();
        let attitude = measurement_attitude(MeasurementMode::CurrentPosition, 1, &solve)
            .unwrap()
            .expect("matrix-bearing solve must yield an attitude");
        let boresight = attitude.columns()[2];
        let expected = unit_from_radec(52.1, 85.2);
        assert!((boresight - expected).norm() < 1e-9, "boresight column");
    }

    #[test]
    fn test_current_position_targets_sweep_away_from_the_meridian() {
        // West of the meridian (positive hour angle): RA must shrink.
        let west = current_position_targets(100.0, 60.0, 30.0, 15.0);
        assert_eq!(west, [(100.0, 60.0), (85.0, 60.0), (70.0, 60.0)]);
        // East of the meridian: RA must grow.
        let east = current_position_targets(100.0, 60.0, -20.0, 15.0);
        assert_eq!(east, [(100.0, 60.0), (115.0, 60.0), (130.0, 60.0)]);
        // On the meridian either side works; the west branch is taken.
        let tied = current_position_targets(100.0, 60.0, 0.0, 15.0);
        assert_eq!(tied[2], (70.0, 60.0));
        // RA folds through 0.
        let folded = current_position_targets(5.0, 60.0, 30.0, 15.0);
        assert_eq!(folded[2], (335.0, 60.0));
    }

    fn test_eph() -> EphemerisCtx {
        let site: SiteConfig =
            serde_json::from_str(r#"{ "latitude_deg": 48.0, "longitude_deg": -122.8 }"#).unwrap();
        let refraction: crate::config::RefractionConfig =
            serde_json::from_str(r#"{ "enabled": false }"#).unwrap();
        EphemerisCtx::new(site, &refraction).unwrap()
    }

    #[test]
    fn test_targets_above_horizon_rejects_a_sunken_point() {
        let eph = test_eph();
        let at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 8, 0, 0).unwrap();
        // Dec −80 never rises above ~−38° at latitude 48.
        let err =
            require_targets_above_horizon(&eph, at, &[(10.0, 85.0), (100.0, -80.0)]).unwrap_err();
        assert!(err.to_string().contains("point 2"), "{err}");
        assert!(err.to_string().contains("below the 10° floor"), "{err}");
    }

    #[test]
    fn test_targets_above_horizon_accepts_a_circumpolar_sweep() {
        let eph = test_eph();
        let at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 1, 8, 0, 0).unwrap();
        // Dec 60 at latitude 48 never dips below ~18°.
        let targets = current_position_targets(100.0, 60.0, 30.0, 15.0);
        require_targets_above_horizon(&eph, at, &targets).unwrap();
    }

    #[test]
    fn test_star_overlay_speaks_zero_based_outside_and_one_based_into_the_wcs() {
        use crate::math::WcsMatrix;

        // Square-pixel frame; a 90° rotation about the boresight is an
        // exact pixel rotation about CRPIX, which turns any 0/1-based
        // mixup into a full-pixel error instead of a second-order one.
        let frame = SolvedFrame {
            center_ra_deg: 100.0,
            center_dec_deg: 40.0,
            matrix: WcsMatrix {
                crpix1: 512.0,
                crpix2: 384.0,
                cd1_1: -2.9167e-4,
                cd1_2: 0.0,
                cd2_1: 0.0,
                cd2_2: 2.9167e-4,
            },
        };
        let boresight = unit_from_radec(100.0, 40.0);
        let correction_t = Mat3::from_axis_angle(boresight, std::f64::consts::FRAC_PI_2);
        // 50 pixels right of the rotation center, in 0-based indices.
        let star = DetectedStar {
            x: 561.0,
            y: 383.0,
            flux: 1000.0,
            saturated_pixel_count: 0,
        };

        let (stars, in_frame) =
            star_overlay(&frame, correction_t, vec![star], (1024, 768)).unwrap();

        assert!(in_frame);
        assert_eq!(stars[0].x, 561.0);
        assert_eq!(stars[0].y, 383.0);
        // The rotation center in 0-based indices is CRPIX − 1.
        let (dx, dy) = (stars[0].target_x - 511.0, stars[0].target_y - 383.0);
        let radius = dx.hypot(dy);
        assert!(
            (radius - 50.0).abs() < 1e-6,
            "target must stay 50 px from the rotation center, got {radius}"
        );
        assert!(
            (50.0 * dx).abs() < 1e-3,
            "a 90° rotation must land the target perpendicular to the star offset, got dot {}",
            50.0 * dx
        );
    }

    #[test]
    fn test_star_overlay_clears_in_frame_when_a_target_leaves_the_sensor() {
        use crate::math::WcsMatrix;

        let frame = SolvedFrame {
            center_ra_deg: 100.0,
            center_dec_deg: 40.0,
            matrix: WcsMatrix {
                crpix1: 512.0,
                crpix2: 384.0,
                cd1_1: -2.9167e-4,
                cd1_2: 0.0,
                cd2_1: 0.0,
                cd2_2: 2.9167e-4,
            },
        };
        let boresight = unit_from_radec(100.0, 40.0);
        let correction_t = Mat3::from_axis_angle(boresight, std::f64::consts::FRAC_PI_2);
        // 500 px right of center: the 90° rotation sends the target
        // ~500 px vertically, off the 768-row sensor.
        let star = DetectedStar {
            x: 1011.0,
            y: 383.0,
            flux: 1000.0,
            saturated_pixel_count: 0,
        };

        let (stars, in_frame) =
            star_overlay(&frame, correction_t, vec![star], (1024, 768)).unwrap();

        assert!(!in_frame);
        assert_eq!(stars.len(), 1, "the pair is still published for the UI");
    }

    #[test]
    fn test_star_overlay_clears_in_frame_when_a_target_falls_behind_the_tangent_plane() {
        use crate::math::WcsMatrix;

        let frame = SolvedFrame {
            center_ra_deg: 100.0,
            center_dec_deg: 40.0,
            matrix: WcsMatrix {
                crpix1: 512.0,
                crpix2: 384.0,
                cd1_1: -2.9167e-4,
                cd1_2: 0.0,
                cd2_1: 0.0,
                cd2_2: 2.9167e-4,
            },
        };
        // Rotate 120° about an axis perpendicular to the boresight:
        // the corrected direction points away from the tangent plane,
        // so it has no pixel at all.
        let boresight = unit_from_radec(100.0, 40.0);
        let perpendicular = boresight
            .cross(Vec3::new(0.0, 0.0, 1.0))
            .normalized()
            .unwrap();
        let correction_t = Mat3::from_axis_angle(perpendicular, 120.0_f64.to_radians());
        let star = DetectedStar {
            x: 511.0,
            y: 383.0,
            flux: 1000.0,
            saturated_pixel_count: 0,
        };

        let (stars, in_frame) =
            star_overlay(&frame, correction_t, vec![star], (1024, 768)).unwrap();

        assert!(!in_frame);
        assert!(stars.is_empty(), "a target with no pixel publishes no pair");
    }

    #[tokio::test]
    async fn test_stale_finish_permit_does_not_survive_re_arming() {
        let shared = WorkflowShared::default();
        // A finish that raced the previous run's end leaves a permit.
        shared.signal_finish().await;
        shared.arm_finish().await;
        let armed = shared.finish_signal().await;
        let woke = tokio::time::timeout(std::time::Duration::from_millis(50), armed.notified());
        assert!(
            woke.await.is_err(),
            "a stale permit leaked into the new invocation"
        );
    }

    #[tokio::test]
    async fn test_finish_signal_sent_before_the_loop_waits_still_ends_adjustment() {
        let shared = WorkflowShared::default();
        shared.arm_finish().await;
        shared.signal_finish().await;
        let signal = shared.finish_signal().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), signal.notified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_manual_wait_publishes_the_point_and_ends_on_continue() {
        let shared = WorkflowShared::default();
        let waiter = shared.clone();
        let wait = tokio::spawn(async move {
            wait_for_manual_rotation(&waiter, Duration::from_secs(5), 2).await
        });
        // The wait publishes `awaiting_point` before it blocks; poll
        // until it appears, then confirm.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while shared.status.read().await.awaiting_point != Some(2) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the wait never published awaiting_point"
            );
            tokio::task::yield_now().await;
        }
        shared.signal_proceed().await;
        wait.await.unwrap().unwrap();
        assert!(
            shared.status.read().await.awaiting_point.is_none(),
            "the wait must clear awaiting_point on exit"
        );
    }

    #[tokio::test]
    async fn test_manual_wait_timeout_names_the_point() {
        let shared = WorkflowShared::default();
        let err = wait_for_manual_rotation(&shared, Duration::from_millis(50), 3)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("point 3"), "{err}");
        assert!(
            shared.status.read().await.awaiting_point.is_none(),
            "a timed-out wait must clear awaiting_point"
        );
    }

    /// A `/measure/continue` from before the wait was armed (a
    /// double-click on the previous point) must not skip this wait.
    #[tokio::test]
    async fn test_stale_continue_does_not_skip_the_next_manual_wait() {
        let shared = WorkflowShared::default();
        shared.signal_proceed().await;
        let err = wait_for_manual_rotation(&shared, Duration::from_millis(50), 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not confirm"), "{err}");
    }

    #[tokio::test]
    async fn test_record_latest_image_stores_the_path() {
        let shared = WorkflowShared::default();
        assert!(shared.latest_image.read().await.is_none());
        shared
            .record_latest_image("/data/rp/images/pa-000042.fits")
            .await;
        assert_eq!(
            shared.latest_image.read().await.as_deref(),
            Some(std::path::Path::new("/data/rp/images/pa-000042.fits"))
        );
    }
}
