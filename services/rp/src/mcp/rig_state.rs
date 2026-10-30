//! The rig state a capture records (rp.md § Core Fields, Rig state):
//! the focusers and rotators in the camera's train and the site's
//! observing-conditions devices and safety monitors, all read once the
//! frame is ready; the guider's running totals, read at both ends of
//! the exposure; and the position angle the frame was meant to be taken
//! at. The mount's share, pier side and tracking, rides the frame's one
//! mount read in `internals`.

use std::future::Future;
use std::sync::Arc;

use ascom_alpaca::api::{Focuser, ObservingConditions, Rotator, SafetyMonitor};
use rusty_photon_rolling_stats::SquareSums;
use tokio::time::Instant;
use tracing::debug;

use super::handler::McpHandler;
use super::internals::{bounded_read, finished_spawned_read, SpawnedRead};
use crate::equipment::trains::{Train, TrainDeviceKind};
use crate::persistence::{
    ExposureFocuser, ExposureGuiding, ExposureObservingConditions, ExposureRotator,
    ExposureSafetyMonitor,
};

/// What a frame's rig-state reads found once it was ready; each field
/// `None` when it has nothing to record.
#[derive(Debug, Default)]
pub(super) struct RigState {
    pub(super) focusers: Option<Vec<ExposureFocuser>>,
    pub(super) rotators: Option<Vec<ExposureRotator>>,
    pub(super) guiding: Option<ExposureGuiding>,
    pub(super) observing_conditions: Option<Vec<ExposureObservingConditions>>,
    pub(super) safety_monitors: Option<Vec<ExposureSafetyMonitor>>,
}

/// The guider read a capture starts with its exposure: the guider
/// service's stats, or why they could not be read.
pub(super) type GuidingStartRead = SpawnedRead<Result<rp_guider::GuidingStats, String>>;

/// What a capture's target says about the frame's framing (rp.md
/// § Rig state, `intended_position_angle_degrees`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Framing {
    /// No target from the store: an untyped capture, or a calibration
    /// frame filed under its reserved slug.
    #[default]
    NoStoreTarget,
    /// A target from the store, with its own position angle if it has
    /// one.
    StoreTarget { position_angle_degrees: Option<f64> },
}

impl Framing {
    /// The angle the frame was meant to be taken at: a store target's
    /// effective angle, with the camera's train as layer two (rp.md
    /// § Position angle), else the train's default.
    pub(super) fn intended_position_angle(self, train: Option<&Train>) -> Option<f64> {
        let train_default = train.and_then(|train| train.default_position_angle_degrees);
        match self {
            Self::StoreTarget {
                position_angle_degrees,
            } => Some(position_angle_degrees.or(train_default).unwrap_or(0.0)),
            Self::NoStoreTarget => train_default,
        }
    }
}

impl McpHandler {
    /// Start the frame's first guider read on its own task, so the
    /// exposure never waits on it. `None` when no guider is configured.
    pub(super) fn start_guiding_read(&self) -> Option<GuidingStartRead> {
        let client = self.guider.clone()?;
        Some(SpawnedRead(tokio::spawn(async move {
            client.guiding_stats().await.map_err(|e| e.to_string())
        })))
    }

    /// The rig-state reads a ready frame takes, all at once, each device
    /// read bounded by `deadline` (rp.md § Rig state).
    pub(super) async fn read_rig_state(
        &self,
        camera_id: &str,
        guiding_start: Option<GuidingStartRead>,
        deadline: Instant,
    ) -> RigState {
        let train = self.trains.train_for_camera(camera_id);
        let (focusers, rotators, guiding, observing_conditions, safety_monitors) = tokio::join!(
            all_found(self.focuser_reads(train, deadline)),
            all_found(self.rotator_reads(train, deadline)),
            self.read_guiding(guiding_start, deadline),
            all_found(self.observing_conditions_reads(deadline)),
            all_found(self.safety_monitor_reads(deadline)),
        );
        RigState {
            focusers,
            rotators,
            guiding,
            observing_conditions,
            safety_monitors,
        }
    }

    /// One read per focuser in the train, in optical order; a focuser
    /// whose session is down gets none.
    fn focuser_reads(
        &self,
        train: Option<&Train>,
        deadline: Instant,
    ) -> Vec<impl Future<Output = Option<ExposureFocuser>> + Send + 'static> {
        let Some(train) = train else {
            return Vec::new();
        };
        train
            .ids_of_kind(TrainDeviceKind::Focuser)
            .into_iter()
            .filter_map(|id| {
                let device = self
                    .equipment
                    .find_focuser(id)
                    .and_then(crate::equipment::FocuserEntry::device);
                if device.is_none() {
                    debug!(focuser_id = id, "focuser not connected; the frame omits it");
                }
                Some(read_focuser(id.to_string(), device?, deadline))
            })
            .collect()
    }

    /// One read per rotator in the train, as [`Self::focuser_reads`].
    fn rotator_reads(
        &self,
        train: Option<&Train>,
        deadline: Instant,
    ) -> Vec<impl Future<Output = Option<ExposureRotator>> + Send + 'static> {
        let Some(train) = train else {
            return Vec::new();
        };
        train
            .ids_of_kind(TrainDeviceKind::Rotator)
            .into_iter()
            .filter_map(|id| {
                let device = self
                    .equipment
                    .find_rotator(id)
                    .and_then(crate::equipment::RotatorEntry::device);
                if device.is_none() {
                    debug!(rotator_id = id, "rotator not connected; the frame omits it");
                }
                Some(read_rotator(id.to_string(), device?, deadline))
            })
            .collect()
    }

    /// One read per configured observing-conditions device whose
    /// session is up, in config order.
    fn observing_conditions_reads(
        &self,
        deadline: Instant,
    ) -> Vec<impl Future<Output = Option<ExposureObservingConditions>> + Send + 'static> {
        self.equipment
            .observing_conditions
            .iter()
            .filter_map(|entry| {
                let device = entry.device();
                if device.is_none() {
                    debug!(observing_conditions_id = %entry.id, "observing conditions device not connected; the frame omits it");
                }
                Some(read_observing_conditions(
                    entry.id.clone(),
                    device?,
                    deadline,
                ))
            })
            .collect()
    }

    /// One read per configured safety monitor whose session is up, in
    /// config order.
    fn safety_monitor_reads(
        &self,
        deadline: Instant,
    ) -> Vec<impl Future<Output = Option<ExposureSafetyMonitor>> + Send + 'static> {
        self.equipment
            .safety_monitors
            .iter()
            .filter_map(|entry| {
                let device = entry.device();
                if device.is_none() {
                    debug!(safety_monitor_id = %entry.id, "safety monitor not connected; the frame omits it");
                }
                Some(read_safety_monitor(entry.id.clone(), device?, deadline))
            })
            .collect()
    }

    /// The guiding over the exposure: the start read's stats, once they
    /// have answered within their grace, against a second read sent only
    /// then, so the guider service takes the two snapshots in order.
    async fn read_guiding(
        &self,
        start: Option<GuidingStartRead>,
        deadline: Instant,
    ) -> Option<ExposureGuiding> {
        let client = self.guider.clone()?;
        let start = finished_spawned_read(start, "guider").await?;
        let start = reading("guider", "guiding stats as the exposure started", start)?;
        let end = bounded_read(deadline, client.guiding_stats()).await;
        let end = reading("guider", "guiding stats once the frame was ready", end)?;
        guiding_between(&start, &end)
    }
}

/// Run one read per device, each on its own task so they all run at
/// once, and keep what they found in device order. `None` when none
/// found anything. Dropping the returned future, as a cancelled capture
/// does, aborts the reads still out.
async fn all_found<T, F>(reads: Vec<F>) -> Option<Vec<T>>
where
    T: Send + 'static,
    F: Future<Output = Option<T>> + Send + 'static,
{
    let spawned: Vec<SpawnedRead<Option<T>>> = reads
        .into_iter()
        .map(|read| SpawnedRead(tokio::spawn(read)))
        .collect();
    let mut found = Vec::with_capacity(spawned.len());
    for mut read in spawned {
        if let Ok(Some(entry)) = (&mut read.0).await {
            found.push(entry);
        }
    }
    (!found.is_empty()).then_some(found)
}

/// A rig-state reading, or `None` with the reason logged.
fn reading<T>(device_id: &str, what: &str, read: Result<T, String>) -> Option<T> {
    match read {
        Ok(value) => Some(value),
        Err(error) => {
            debug!(
                device_id,
                reading = what,
                %error,
                "rig-state read failed; the frame omits it"
            );
            None
        }
    }
}

/// A rig-state reading that is also finite; a non-finite one is
/// dropped and logged.
fn finite(device_id: &str, what: &str, read: Result<f64, String>) -> Option<f64> {
    let value = reading(device_id, what, read)?;
    if value.is_finite() {
        Some(value)
    } else {
        debug!(
            device_id,
            reading = what,
            value,
            "rig-state reading is not finite; the frame omits it"
        );
        None
    }
}

/// Whether a focuser or rotator stood still at the read: its `IsMoving`
/// answered `false`. One that moves, or cannot say, gives no position,
/// since the frame saw none of the positions it passed through.
fn stood_still(device_id: &str, moving: Result<bool, String>) -> bool {
    match moving {
        Ok(false) => true,
        other => {
            debug!(device_id, moving = ?other, "device moving, or unable to say; the frame omits its position");
            false
        }
    }
}

async fn read_focuser(
    id: String,
    focuser: Arc<dyn Focuser>,
    deadline: Instant,
) -> Option<ExposureFocuser> {
    let (moving, position, temperature) = tokio::join!(
        bounded_read(deadline, focuser.is_moving()),
        bounded_read(deadline, focuser.position()),
        bounded_read(deadline, focuser.temperature()),
    );
    let position = if stood_still(&id, moving) {
        reading(&id, "position", position)
    } else {
        None
    };
    let temperature_c = finite(&id, "temperature", temperature);
    (position.is_some() || temperature_c.is_some()).then_some(ExposureFocuser {
        id,
        position,
        temperature_c,
    })
}

async fn read_rotator(
    id: String,
    rotator: Arc<dyn Rotator>,
    deadline: Instant,
) -> Option<ExposureRotator> {
    let (moving, angle, mechanical_angle) = tokio::join!(
        bounded_read(deadline, rotator.is_moving()),
        bounded_read(deadline, rotator.position()),
        bounded_read(deadline, rotator.mechanical_position()),
    );
    if !stood_still(&id, moving) {
        return None;
    }
    let angle_degrees = finite(&id, "position", angle);
    let mechanical_angle_degrees = finite(&id, "mechanical position", mechanical_angle);
    (angle_degrees.is_some() || mechanical_angle_degrees.is_some()).then_some(ExposureRotator {
        id,
        angle_degrees,
        mechanical_angle_degrees,
    })
}

async fn read_observing_conditions(
    id: String,
    device: Arc<dyn ObservingConditions>,
    deadline: Instant,
) -> Option<ExposureObservingConditions> {
    let (
        temperature,
        humidity,
        dew_point,
        pressure,
        cloud_cover,
        rain_rate,
        sky_brightness,
        sky_quality,
        sky_temperature,
        star_fwhm,
        wind_direction,
        wind_speed,
        wind_gust,
    ) = tokio::join!(
        bounded_read(deadline, device.temperature()),
        bounded_read(deadline, device.humidity()),
        bounded_read(deadline, device.dew_point()),
        bounded_read(deadline, device.pressure()),
        bounded_read(deadline, device.cloud_cover()),
        bounded_read(deadline, device.rain_rate()),
        bounded_read(deadline, device.sky_brightness()),
        bounded_read(deadline, device.sky_quality()),
        bounded_read(deadline, device.sky_temperature()),
        bounded_read(deadline, device.star_fwhm()),
        bounded_read(deadline, device.wind_direction()),
        bounded_read(deadline, device.wind_speed()),
        bounded_read(deadline, device.wind_gust()),
    );
    let entry = ExposureObservingConditions {
        temperature_c: finite(&id, "temperature", temperature),
        humidity_pct: finite(&id, "humidity", humidity),
        dew_point_c: finite(&id, "dew point", dew_point),
        pressure_hpa: finite(&id, "pressure", pressure),
        cloud_cover_pct: finite(&id, "cloud cover", cloud_cover),
        rain_rate_mm_per_hour: finite(&id, "rain rate", rain_rate),
        sky_brightness_lux: finite(&id, "sky brightness", sky_brightness),
        sky_quality_mag_per_arcsec2: finite(&id, "sky quality", sky_quality),
        sky_temperature_c: finite(&id, "sky temperature", sky_temperature),
        star_fwhm_arcsec: finite(&id, "star FWHM", star_fwhm),
        wind_direction_degrees: finite(&id, "wind direction", wind_direction),
        wind_speed_m_per_s: finite(&id, "wind speed", wind_speed),
        wind_gust_m_per_s: finite(&id, "wind gust", wind_gust),
        id,
    };
    let has_reading = [
        entry.temperature_c,
        entry.humidity_pct,
        entry.dew_point_c,
        entry.pressure_hpa,
        entry.cloud_cover_pct,
        entry.rain_rate_mm_per_hour,
        entry.sky_brightness_lux,
        entry.sky_quality_mag_per_arcsec2,
        entry.sky_temperature_c,
        entry.star_fwhm_arcsec,
        entry.wind_direction_degrees,
        entry.wind_speed_m_per_s,
        entry.wind_gust_m_per_s,
    ]
    .iter()
    .any(Option::is_some);
    has_reading.then_some(entry)
}

async fn read_safety_monitor(
    id: String,
    monitor: Arc<dyn SafetyMonitor>,
    deadline: Instant,
) -> Option<ExposureSafetyMonitor> {
    let is_safe = reading(
        &id,
        "is_safe",
        bounded_read(deadline, monitor.is_safe()).await,
    )?;
    Some(ExposureSafetyMonitor { id, is_safe })
}

/// The guiding between two reads of the guider service's stats (rp.md
/// § Rig state, `guiding`). `None` when either lacks the running totals;
/// only `guided_throughout: false` when the two cannot be subtracted.
fn guiding_between(
    start: &rp_guider::GuidingStats,
    end: &rp_guider::GuidingStats,
) -> Option<ExposureGuiding> {
    let (Some(first), Some(last)) = (&start.totals, &end.totals) else {
        debug!("the guider service reports no running totals; the frame omits guiding");
        return None;
    };
    let Some(between) = TotalsBetween::of(first, last) else {
        debug!(
            start_epoch = %first.epoch,
            end_epoch = %last.epoch,
            "the guider's totals cannot be subtracted; the frame records only that guiding was not throughout"
        );
        return Some(ExposureGuiding::default());
    };
    let rms_ra_px = between.ra.rms();
    let rms_dec_px = between.dec.rms();
    Some(ExposureGuiding {
        guided_throughout: start.guiding && end.guiding && between.interruptions == 0,
        guide_steps: Some(between.guide_steps),
        rms_ra_px,
        rms_dec_px,
        total_rms_px: rms_ra_px.zip(rms_dec_px).map(|(ra, dec)| ra.hypot(dec)),
        star_lost: Some(between.star_lost),
    })
}

/// What the guider service folded between two readings of its running
/// totals.
#[derive(Debug, PartialEq)]
struct TotalsBetween {
    guide_steps: u64,
    ra: SquareSums,
    dec: SquareSums,
    star_lost: u64,
    interruptions: u64,
}

impl TotalsBetween {
    /// `None` when the readings cannot be subtracted: they come from two
    /// runs of the service (their epochs differ), or a count or sum went
    /// backwards or is not one the service could report.
    fn of(first: &rp_guider::GuidingTotals, last: &rp_guider::GuidingTotals) -> Option<Self> {
        if first.epoch != last.epoch {
            return None;
        }
        let ra = SquareSums::from_parts(last.ra_samples, last.ra_sum_sq_px2)?.since(
            &SquareSums::from_parts(first.ra_samples, first.ra_sum_sq_px2)?,
        )?;
        let dec = SquareSums::from_parts(last.dec_samples, last.dec_sum_sq_px2)?.since(
            &SquareSums::from_parts(first.dec_samples, first.dec_sum_sq_px2)?,
        )?;
        Some(Self {
            guide_steps: last.guide_steps.checked_sub(first.guide_steps)?,
            ra,
            dec,
            star_lost: last.star_lost.checked_sub(first.star_lost)?,
            interruptions: last.interruptions.checked_sub(first.interruptions)?,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn totals(guide_steps: u64, interruptions: u64) -> rp_guider::GuidingTotals {
        let steps = f64::from(u32::try_from(guide_steps).unwrap());
        rp_guider::GuidingTotals {
            epoch: "run-1".to_string(),
            guide_steps,
            ra_samples: guide_steps,
            ra_sum_sq_px2: 0.5625 * steps,
            dec_samples: guide_steps,
            dec_sum_sq_px2: steps,
            star_lost: guide_steps / 10,
            interruptions,
        }
    }

    fn stats(guiding: bool, totals: Option<rp_guider::GuidingTotals>) -> rp_guider::GuidingStats {
        rp_guider::GuidingStats {
            app_state: if guiding { "Guiding" } else { "Stopped" }.to_string(),
            guiding,
            rms_ra_px: None,
            rms_dec_px: None,
            total_rms_px: None,
            snr: None,
            star_mass: None,
            sample_count: 0,
            totals,
        }
    }

    #[test]
    fn guiding_that_ran_through_records_the_error_over_the_steps_between() {
        let guiding = guiding_between(
            &stats(true, Some(totals(10, 2))),
            &stats(true, Some(totals(30, 2))),
        )
        .unwrap();
        assert_eq!(
            guiding,
            ExposureGuiding {
                guided_throughout: true,
                guide_steps: Some(20),
                rms_ra_px: Some(0.75),
                rms_dec_px: Some(1.0),
                total_rms_px: Some(1.25),
                star_lost: Some(2),
            }
        );
    }

    #[test]
    fn an_interruption_between_the_reads_means_not_throughout() {
        let guiding = guiding_between(
            &stats(true, Some(totals(10, 2))),
            &stats(true, Some(totals(30, 3))),
        )
        .unwrap();
        assert!(!guiding.guided_throughout);
        assert_eq!(guiding.guide_steps, Some(20));
    }

    #[test]
    fn phd2_not_guiding_at_either_read_means_not_throughout() {
        for (at_start, at_end) in [(false, true), (true, false)] {
            let guiding = guiding_between(
                &stats(at_start, Some(totals(10, 0))),
                &stats(at_end, Some(totals(30, 0))),
            )
            .unwrap();
            assert!(!guiding.guided_throughout, "{at_start} {at_end}");
        }
    }

    #[test]
    fn no_steps_between_the_reads_records_no_rms() {
        let guiding = guiding_between(
            &stats(false, Some(totals(10, 0))),
            &stats(false, Some(totals(10, 0))),
        )
        .unwrap();
        assert_eq!(
            guiding,
            ExposureGuiding {
                guided_throughout: false,
                guide_steps: Some(0),
                star_lost: Some(0),
                ..ExposureGuiding::default()
            }
        );
    }

    #[test]
    fn totals_from_two_runs_of_the_service_record_only_not_throughout() {
        let mut restarted = totals(5, 0);
        restarted.epoch = "run-2".to_string();
        let guiding = guiding_between(
            &stats(true, Some(totals(10, 0))),
            &stats(true, Some(restarted)),
        );
        assert_eq!(guiding, Some(ExposureGuiding::default()));
    }

    #[test]
    fn a_count_that_went_backwards_records_only_not_throughout() {
        let mut earlier = totals(10, 0);
        earlier.star_lost = 9;
        let guiding = guiding_between(
            &stats(true, Some(earlier)),
            &stats(true, Some(totals(30, 0))),
        );
        assert_eq!(guiding, Some(ExposureGuiding::default()));
    }

    #[test]
    fn a_guider_without_running_totals_records_no_guiding() {
        assert_eq!(
            guiding_between(&stats(true, None), &stats(true, Some(totals(30, 0)))),
            None
        );
        assert_eq!(
            guiding_between(&stats(true, Some(totals(10, 0))), &stats(true, None)),
            None
        );
    }

    fn train(default_position_angle_degrees: Option<f64>) -> Train {
        Train {
            id: "main".to_string(),
            purpose: crate::config::TrainPurpose::Imaging,
            telescope: None,
            focal_length_mm: None,
            aperture_mm: None,
            default_position_angle_degrees,
            devices: Vec::new(),
            auto_focus: None,
        }
    }

    #[test]
    fn a_store_target_is_intended_at_its_own_angle_then_the_train_then_north_up() {
        let own = Framing::StoreTarget {
            position_angle_degrees: Some(30.0),
        };
        let inherits = Framing::StoreTarget {
            position_angle_degrees: None,
        };
        let with_default = train(Some(254.0));
        assert_eq!(own.intended_position_angle(Some(&with_default)), Some(30.0));
        assert_eq!(
            inherits.intended_position_angle(Some(&with_default)),
            Some(254.0)
        );
        assert_eq!(
            inherits.intended_position_angle(Some(&train(None))),
            Some(0.0)
        );
        assert_eq!(inherits.intended_position_angle(None), Some(0.0));
    }

    #[test]
    fn a_frame_of_no_store_target_is_intended_at_the_train_default_if_any() {
        let framing = Framing::NoStoreTarget;
        assert_eq!(
            framing.intended_position_angle(Some(&train(Some(254.0)))),
            Some(254.0)
        );
        assert_eq!(framing.intended_position_angle(Some(&train(None))), None);
        assert_eq!(framing.intended_position_angle(None), None);
    }
}
