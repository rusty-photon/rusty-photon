//! The flats workflow (docs/services/calibrator-flats.md § Tools).
//!
//! Train resolution, the proportional exposure search with its
//! brightness ladder and floor (§ Exposure search), the cover/panel
//! guard, and the bodies of `train_flats`, `take_flats` and
//! `get_flat_training`.
//!
//! Everything here talks to `rp` through [`FlatsRig`], a trait the real
//! [`crate::mcp_client::McpClient`] implements and `mockall` mocks in the
//! unit tests. A tool body gets a [`Rig`] pair: the `active` rig, whose
//! calls the caller's cancellation reaches, and the `cleanup` rig, whose
//! calls it cannot — panel off and cover restore run on the latter.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::error::{CalibratorFlatsError, Result};
use crate::store::{CameraFacts, FlatRecord, FlatStore};

// ---------------------------------------------------------------------------
// What the workflow needs from rp
// ---------------------------------------------------------------------------

/// What a capture is for: a throwaway search exposure, or a flat filed
/// as `frame_type: "Flat"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    Probe,
    Flat,
}

/// The members of a train as `rp`'s `get_train_info` reports them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrainInfo {
    #[serde(default)]
    pub camera_id: Option<String>,
    #[serde(default)]
    pub filter_wheel_id: Option<String>,
    /// The wheel's configured names in position order; `None` without a
    /// sole wheel.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
    #[serde(default)]
    pub calibrator_id: Option<String>,
}

/// What `get_camera_info` reports: the search bounds and the facts a
/// record is only valid at.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraInfo {
    pub max_adu: u32,
    #[serde(with = "humantime_serde")]
    pub exposure_min: Duration,
    #[serde(with = "humantime_serde")]
    pub exposure_max: Duration,
    pub bin_x: u32,
    pub bin_y: u32,
    #[serde(default)]
    pub gain: Option<i32>,
    #[serde(default)]
    pub offset: Option<i32>,
}

/// Result from the `capture` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureResult {
    pub image_path: String,
    pub document_id: String,
}

/// Result from the `compute_image_stats` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageStats {
    pub median_adu: u32,
}

/// The `rp` tools the workflow drives, behind a trait so the bodies can
/// be tested against a mock rig.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait FlatsRig: Send + Sync {
    async fn get_train_info(&self, train_id: &str) -> Result<TrainInfo>;
    async fn get_camera_info(&self, camera_id: &str) -> Result<CameraInfo>;
    /// The cover state name as `rp` reports it (`NotPresent` | `Closed` |
    /// `Moving` | `Open` | `Unknown` | `Error`).
    async fn get_cover_state(&self, calibrator_id: &str) -> Result<String>;
    async fn close_cover(&self, calibrator_id: &str) -> Result<()>;
    async fn open_cover(&self, calibrator_id: &str) -> Result<()>;
    /// Light the panel; returns the brightness `rp` applied (the device
    /// maximum when `brightness` is `None`).
    async fn calibrator_on(&self, calibrator_id: &str, brightness: Option<u32>) -> Result<u32>;
    async fn calibrator_off(&self, calibrator_id: &str) -> Result<()>;
    async fn set_filter(&self, filter_wheel_id: &str, filter: &str) -> Result<()>;
    async fn capture(
        &self,
        camera_id: &str,
        duration: Duration,
        frame: Frame,
    ) -> Result<CaptureResult>;
    async fn compute_image_stats(&self, image_path: &str, document_id: &str) -> Result<ImageStats>;
}

/// Where a tool body reports progress: the rmcp `notifications/progress`
/// relay in `tools.rs`, or nowhere.
#[async_trait]
pub trait Progress: Send + Sync {
    async fn tick(&self, progress: f64, total: Option<f64>, message: String);
}

/// A [`Progress`] that drops every tick.
pub struct NoProgress;

#[async_trait]
impl Progress for NoProgress {
    async fn tick(&self, _progress: f64, _total: Option<f64>, _message: String) {}
}

/// The two views of one connection a tool body works with: `active`,
/// whose calls the caller's cancellation reaches, and `cleanup`, whose
/// calls it cannot (plan D9).
#[derive(Clone, Copy)]
pub struct Rig<'a> {
    pub active: &'a dyn FlatsRig,
    pub cleanup: &'a dyn FlatsRig,
}

// ---------------------------------------------------------------------------
// Parameters and outcomes
// ---------------------------------------------------------------------------

/// `train_flats` arguments.
#[derive(Debug, Clone, Default)]
pub struct TrainFlatsParams {
    pub train_id: String,
    pub filters: Option<Vec<String>>,
    pub brightness: Option<u32>,
}

/// `take_flats` arguments.
#[derive(Debug, Clone, Default)]
pub struct TakeFlatsParams {
    pub train_id: String,
    pub count: u32,
    pub filters: Option<Vec<String>>,
}

/// One filter `train_flats` could not converge; nothing was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unconverged {
    pub filter: Option<String>,
    #[serde(with = "humantime_serde")]
    pub best_duration: Duration,
    pub median_adu: u32,
}

/// The `train_flats` result.
#[derive(Debug, Clone, Serialize)]
pub struct TrainFlatsOutcome {
    pub train_id: String,
    pub trained: Vec<FlatRecord>,
    pub unconverged: Vec<Unconverged>,
    pub cover_restored: bool,
    pub warnings: Vec<String>,
}

/// A flat whose median left the verification band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutOfRange {
    pub image_path: String,
    pub median_adu: u32,
}

/// One filter's `take_flats` result.
#[derive(Debug, Clone, Serialize)]
pub struct FilterFlats {
    pub filter: Option<String>,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub brightness: u32,
    pub frames: u32,
    pub out_of_range: Vec<OutOfRange>,
}

/// The `take_flats` result.
#[derive(Debug, Clone, Serialize)]
pub struct TakeFlatsOutcome {
    pub train_id: String,
    pub filters: Vec<FilterFlats>,
    pub total_frames: u32,
    pub cover_restored: bool,
    pub warnings: Vec<String>,
}

/// One stored record judged against the live camera.
#[derive(Debug, Clone, Serialize)]
pub struct TrainingView {
    pub record: FlatRecord,
    /// `"trained"` or `"stale"`.
    pub status: &'static str,
    /// Each changed field, `<field> changed from <recorded> to <current>`.
    pub stale: Vec<String>,
}

/// The `get_flat_training` result.
#[derive(Debug, Clone, Serialize)]
pub struct TrainingOutcome {
    pub train_id: String,
    pub camera: CameraFacts,
    pub records: Vec<TrainingView>,
}

// ---------------------------------------------------------------------------
// Train resolution
// ---------------------------------------------------------------------------

/// A train as the workflow sees it: the resolved members and the
/// capture groups (`filters`) a tool call works through.
#[derive(Debug, Clone)]
pub struct TrainContext {
    pub train_id: String,
    pub camera_id: String,
    pub calibrator_id: Option<String>,
    pub filter_wheel_id: Option<String>,
    /// The capture groups in order: the requested (or every) wheel
    /// filter, or the single `None` group of a filterless train.
    pub filters: Vec<Option<String>>,
    pub camera: CameraInfo,
}

impl TrainContext {
    /// The camera facts a record is judged against.
    #[must_use]
    pub fn camera_facts(&self) -> CameraFacts {
        CameraFacts {
            camera_id: self.camera_id.clone(),
            max_adu: self.camera.max_adu,
            bin_x: self.camera.bin_x,
            bin_y: self.camera.bin_y,
            gain: self.camera.gain,
            offset: self.camera.offset,
        }
    }

    /// The cover calibrator an actuating tool needs.
    fn calibrator(&self) -> Result<&str> {
        self.calibrator_id.as_deref().ok_or_else(|| {
            CalibratorFlatsError::Workflow(format!(
                "train '{}' has no cover calibrator",
                self.train_id
            ))
        })
    }
}

/// Resolve `train_id` through `rp` and select the capture groups
/// (docs/services/calibrator-flats.md § Filter selection). Reads only:
/// `get_train_info` and `get_camera_info`.
///
/// # Errors
///
/// Returns [`CalibratorFlatsError::Workflow`] if the train has no
/// camera, if `requested` is given for a filterless train, is empty, or
/// names a filter the wheel does not have, or if the wheel has no
/// configured names; and the `rp` error for an unknown train.
pub async fn resolve_train(
    rig: &dyn FlatsRig,
    train_id: &str,
    requested: Option<&[String]>,
) -> Result<TrainContext> {
    let info = rig.get_train_info(train_id).await?;
    let camera_id = info.camera_id.ok_or_else(|| {
        CalibratorFlatsError::Workflow(format!("train '{train_id}' has no camera"))
    })?;

    let filters: Vec<Option<String>> = match (&info.filter_wheel_id, requested) {
        (None, Some(_)) => {
            return Err(CalibratorFlatsError::Workflow(format!(
                "train '{train_id}' has no filter wheel; do not pass filters"
            )));
        }
        (None, None) => vec![None],
        (Some(wheel), requested) => {
            let names = info.filters.clone().unwrap_or_default();
            if names.is_empty() {
                return Err(CalibratorFlatsError::Workflow(format!(
                    "the filter wheel '{wheel}' of train '{train_id}' has no configured filters"
                )));
            }
            match requested {
                None => names.into_iter().map(Some).collect(),
                Some([]) => {
                    return Err(CalibratorFlatsError::Workflow(
                        "filters must name at least one filter".to_owned(),
                    ));
                }
                Some(list) => {
                    if let Some(unknown) = list.iter().find(|f| !names.contains(f)) {
                        return Err(CalibratorFlatsError::Workflow(format!(
                            "filter '{unknown}' is not on train '{train_id}' (wheel '{wheel}' has: {})",
                            names.join(", ")
                        )));
                    }
                    list.iter().cloned().map(Some).collect()
                }
            }
        }
    };

    let camera = rig.get_camera_info(&camera_id).await?;
    Ok(TrainContext {
        train_id: train_id.to_owned(),
        camera_id,
        calibrator_id: info.calibrator_id,
        filter_wheel_id: info.filter_wheel_id,
        filters,
        camera,
    })
}

/// How a capture group is named in messages.
fn filter_label(filter: Option<&str>) -> String {
    filter.map_or_else(|| "(no filter)".to_owned(), str::to_owned)
}

// ---------------------------------------------------------------------------
// The exposure search
// ---------------------------------------------------------------------------

/// The 50 % target for a camera.
fn target_adu(max_adu: u32) -> Result<u32> {
    let target = max_adu / 2;
    if target == 0 {
        return Err(CalibratorFlatsError::Workflow(format!(
            "the camera reports max_adu {max_adu}; the 50 % target is 0"
        )));
    }
    Ok(target)
}

/// The proportional step: `current * (target / median)`, doubling when
/// the median is zero. Unclamped — the caller applies the floor and the
/// ceiling, because a step *below* the floor is information (plan D8).
fn proposed_duration(current: Duration, target_adu: u32, last_median: u32) -> Duration {
    if last_median == 0 {
        return current.saturating_mul(2);
    }
    let ratio = f64::from(target_adu) / f64::from(last_median);
    Duration::try_from_secs_f64(current.as_secs_f64() * ratio).unwrap_or(Duration::MAX)
}

/// Fractional deviation of a measured median from the target.
fn deviation(target_adu: u32, median: u32) -> f64 {
    (f64::from(median) - f64::from(target_adu)).abs() / f64::from(target_adu)
}

/// One pass of the search, at one brightness level.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchOutcome {
    /// The converged exposure, or the best guess when not.
    duration: Duration,
    median: u32,
    iterations: u32,
    converged: bool,
    /// The pass ended pinned over the target — by saturation, or by a
    /// step that wanted less than the floor — so dimming the panel is
    /// the way down.
    over_bright: bool,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the search bounds are distinct facts (config floor, camera ceiling, pass start) and a struct would only rename them"
)]
async fn search_pass(
    rig: &dyn FlatsRig,
    camera_id: &str,
    config: &Config,
    target: u32,
    floor: Duration,
    ceiling: Duration,
    start: Duration,
    progress: &dyn Progress,
    ticks: &mut u32,
    label: &str,
) -> Result<SearchOutcome> {
    let mut duration = start.max(floor).min(ceiling);
    let mut last_median = 0u32;

    for iteration in 1..=config.max_iterations {
        let captured = rig.capture(camera_id, duration, Frame::Probe).await?;
        let stats = rig
            .compute_image_stats(&captured.image_path, &captured.document_id)
            .await?;
        last_median = stats.median_adu;
        let dev = deviation(target, last_median);
        *ticks = ticks.saturating_add(1);
        progress
            .tick(
                f64::from(*ticks),
                None,
                format!(
                    "{label}: {} → median {last_median} ADU (target {target})",
                    humantime::format_duration(duration)
                ),
            )
            .await;
        debug!(
            filter = %label,
            iteration,
            duration = %humantime::format_duration(duration),
            median_adu = last_median,
            target_adu = target,
            deviation = format!("{:.1}%", dev * 100.0),
            "exposure iteration"
        );

        if dev <= config.tolerance {
            return Ok(SearchOutcome {
                duration,
                median: last_median,
                iterations: iteration,
                converged: true,
                over_bright: false,
            });
        }

        let wanted = proposed_duration(duration, target, last_median);
        if wanted < floor {
            debug!(
                filter = %label,
                wanted = %humantime::format_duration(wanted),
                floor = %humantime::format_duration(floor),
                "the step wants less than the floor: over-bright"
            );
            return Ok(SearchOutcome {
                duration,
                median: last_median,
                iterations: iteration,
                converged: false,
                over_bright: true,
            });
        }
        duration = wanted.min(ceiling);
    }

    Ok(SearchOutcome {
        duration,
        median: last_median,
        iterations: config.max_iterations,
        converged: false,
        over_bright: last_median > target,
    })
}

/// The search with its brightness ladder (docs/services/calibrator-flats.md
/// § Exposure search): a pass that ends over-bright halves the panel and
/// runs again from its last duration, down to a brightness floor of 1.
/// `brightness` is updated in place so the level that worked persists to
/// the next group.
#[expect(
    clippy::too_many_arguments,
    reason = "the ladder threads the pass inputs plus the two accumulators (brightness, ticks) through one call site per group"
)]
async fn search_with_ladder(
    rig: &dyn FlatsRig,
    ctx: &TrainContext,
    config: &Config,
    target: u32,
    brightness: &mut u32,
    progress: &dyn Progress,
    ticks: &mut u32,
    filter: Option<&str>,
) -> Result<SearchOutcome> {
    let label = filter_label(filter);
    let floor = config.min_exposure.max(ctx.camera.exposure_min);
    let ceiling = ctx.camera.exposure_max;
    let mut start = config.initial_duration;
    let mut total_iterations = 0u32;

    loop {
        let mut pass = search_pass(
            rig,
            &ctx.camera_id,
            config,
            target,
            floor,
            ceiling,
            start,
            progress,
            ticks,
            &label,
        )
        .await?;
        total_iterations = total_iterations.saturating_add(pass.iterations);
        pass.iterations = total_iterations;
        if pass.converged {
            return Ok(pass);
        }

        let halved = *brightness / 2;
        if !pass.over_bright || halved == 0 {
            return Ok(pass);
        }
        info!(
            filter = %label,
            brightness = halved,
            median_adu = pass.median,
            target_adu = target,
            "panel over-bright, stepping brightness down"
        );
        let applied = rig.calibrator_on(ctx.calibrator()?, Some(halved)).await?;
        if applied >= *brightness {
            warn!(
                filter = %label,
                asked = halved,
                applied,
                "the panel did not dim; ending the search"
            );
            return Ok(pass);
        }
        *brightness = applied;
        start = pass.duration;
    }
}

// ---------------------------------------------------------------------------
// The cover / panel guard
// ---------------------------------------------------------------------------

/// The cover and panel for the length of a tool body: the cover's
/// initial state is read before anything moves, the cover is closed,
/// and [`PanelSession::finish`] puts things back on every exit.
pub struct PanelSession {
    calibrator_id: String,
    started_open: bool,
}

/// What cleanup did.
#[derive(Debug, Default)]
pub struct Cleanup {
    /// The cover started open and was reopened.
    pub cover_restored: bool,
    pub warnings: Vec<String>,
}

impl PanelSession {
    /// Read the cover state, then close the cover. A failed read aborts
    /// with nothing to clean up.
    ///
    /// # Errors
    ///
    /// Returns the `rp` error of `get_cover_state` or `close_cover`.
    pub async fn start(rig: &dyn FlatsRig, calibrator_id: &str) -> Result<Self> {
        let initial = rig.get_cover_state(calibrator_id).await?;
        debug!(calibrator_id, initial_cover = %initial, "recorded the initial cover state");
        rig.close_cover(calibrator_id).await?;
        Ok(Self {
            calibrator_id: calibrator_id.to_owned(),
            started_open: initial == "Open",
        })
    }

    /// Light the panel; returns the brightness applied.
    ///
    /// # Errors
    ///
    /// Returns the `rp` error of `calibrator_on`.
    pub async fn light(&self, rig: &dyn FlatsRig, brightness: Option<u32>) -> Result<u32> {
        rig.calibrator_on(&self.calibrator_id, brightness).await
    }

    /// Panel off; cover reopened only if it started open. Nothing here
    /// fails: every problem is a warning, and a reopen `rp` refuses for
    /// safety says the cover correctly stays closed.
    pub async fn finish(self, rig: &dyn FlatsRig) -> Cleanup {
        let mut cleanup = Cleanup::default();
        if let Err(e) = rig.calibrator_off(&self.calibrator_id).await {
            warn!(error = %e, "failed to turn the panel off during cleanup");
            cleanup.warnings.push(format!(
                "calibrator_off failed during cleanup: {}",
                e.tool_message()
            ));
        }
        if !self.started_open {
            debug!("the cover did not start open; leaving it closed");
            return cleanup;
        }
        match rig.open_cover(&self.calibrator_id).await {
            Ok(()) => cleanup.cover_restored = true,
            Err(CalibratorFlatsError::SafetyRefused(message)) => {
                warn!(%message, "open_cover refused for safety; the cover stays closed");
                cleanup.warnings.push(format!(
                    "open_cover was refused — conditions are unsafe, the cover stays closed: {message}"
                ));
            }
            Err(e) => {
                warn!(error = %e, "failed to reopen the cover during cleanup");
                cleanup.warnings.push(format!(
                    "open_cover failed during cleanup — the cover stays closed: {}",
                    e.tool_message()
                ));
            }
        }
        cleanup
    }
}

// ---------------------------------------------------------------------------
// train_flats
// ---------------------------------------------------------------------------

/// The `train_flats` body (docs/services/calibrator-flats.md § `train_flats`).
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a
/// [`CalibratorFlatsError::Workflow`] for a train without a calibrator,
/// and — after cleanup — any `rp`, store or cancellation error of the
/// run itself.
pub async fn train_flats(
    rig: Rig<'_>,
    store: &FlatStore,
    config: &Config,
    params: &TrainFlatsParams,
    progress: &dyn Progress,
) -> Result<TrainFlatsOutcome> {
    let ctx = resolve_train(rig.active, &params.train_id, params.filters.as_deref()).await?;
    let calibrator_id = ctx.calibrator()?.to_owned();
    let target = target_adu(ctx.camera.max_adu)?;
    info!(
        train_id = %ctx.train_id,
        camera_id = %ctx.camera_id,
        target_adu = target,
        groups = ctx.filters.len(),
        "training flats"
    );

    let session = PanelSession::start(rig.active, &calibrator_id).await?;
    let outcome = async {
        let mut brightness = session.light(rig.active, params.brightness).await?;
        train_body(
            rig.active,
            store,
            config,
            &ctx,
            target,
            &mut brightness,
            progress,
        )
        .await
    }
    .await;
    let cleanup = session.finish(rig.cleanup).await;

    let (trained, unconverged) = outcome?;
    Ok(TrainFlatsOutcome {
        train_id: ctx.train_id,
        trained,
        unconverged,
        cover_restored: cleanup.cover_restored,
        warnings: cleanup.warnings,
    })
}

async fn train_body(
    rig: &dyn FlatsRig,
    store: &FlatStore,
    config: &Config,
    ctx: &TrainContext,
    target: u32,
    brightness: &mut u32,
    progress: &dyn Progress,
) -> Result<(Vec<FlatRecord>, Vec<Unconverged>)> {
    let mut trained = Vec::new();
    let mut unconverged = Vec::new();
    let mut ticks = 0u32;
    let facts = ctx.camera_facts();

    for filter in &ctx.filters {
        if let (Some(wheel), Some(name)) = (&ctx.filter_wheel_id, filter) {
            debug!(filter = %name, "switching filter");
            rig.set_filter(wheel, name).await?;
        }
        let pass = search_with_ladder(
            rig,
            ctx,
            config,
            target,
            brightness,
            progress,
            &mut ticks,
            filter.as_deref(),
        )
        .await?;
        let label = filter_label(filter.as_deref());
        if pass.converged {
            info!(
                filter = %label,
                duration = %humantime::format_duration(pass.duration),
                median_adu = pass.median,
                brightness = *brightness,
                iterations = pass.iterations,
                "exposure converged; recording"
            );
            let record = FlatRecord {
                train_id: ctx.train_id.clone(),
                filter: filter.clone(),
                duration: pass.duration,
                brightness: *brightness,
                median_adu: pass.median,
                max_adu: facts.max_adu,
                bin_x: facts.bin_x,
                bin_y: facts.bin_y,
                gain: facts.gain,
                offset: facts.offset,
                camera_id: facts.camera_id.clone(),
                trained_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            };
            store.put(record.clone()).await?;
            trained.push(record);
        } else {
            warn!(
                filter = %label,
                duration = %humantime::format_duration(pass.duration),
                median_adu = pass.median,
                iterations = pass.iterations,
                "exposure did not converge; nothing recorded"
            );
            unconverged.push(Unconverged {
                filter: filter.clone(),
                best_duration: pass.duration,
                median_adu: pass.median,
            });
        }
    }
    Ok((trained, unconverged))
}

// ---------------------------------------------------------------------------
// take_flats
// ---------------------------------------------------------------------------

/// The `take_flats` body (docs/services/calibrator-flats.md § `take_flats`).
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a
/// [`CalibratorFlatsError::Workflow`] for a zero `count`, a train without
/// a calibrator, or any untrained / stale filter — all before anything
/// is actuated — and, after cleanup, any `rp` or cancellation error of
/// the run itself.
pub async fn take_flats(
    rig: Rig<'_>,
    store: &FlatStore,
    config: &Config,
    params: &TakeFlatsParams,
    progress: &dyn Progress,
) -> Result<TakeFlatsOutcome> {
    if params.count == 0 {
        return Err(CalibratorFlatsError::Workflow(
            "count must be at least 1".to_owned(),
        ));
    }
    let ctx = resolve_train(rig.active, &params.train_id, params.filters.as_deref()).await?;
    let calibrator_id = ctx.calibrator()?.to_owned();
    let target = target_adu(ctx.camera.max_adu)?;

    // Pre-flight: every filter trained and current, or nothing moves.
    let facts = ctx.camera_facts();
    let mut plan: Vec<(Option<String>, FlatRecord)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for filter in &ctx.filters {
        let label = filter_label(filter.as_deref());
        match store.get(&ctx.train_id, filter.as_deref()).await? {
            None => problems.push(format!("{label} untrained")),
            Some(record) => {
                let stale = record.stale_fields(&facts);
                if stale.is_empty() {
                    plan.push((filter.clone(), record));
                } else {
                    let changes: Vec<String> = stale.iter().map(ToString::to_string).collect();
                    problems.push(format!("{label} stale ({})", changes.join("; ")));
                }
            }
        }
    }
    if !problems.is_empty() {
        return Err(CalibratorFlatsError::Workflow(format!(
            "train '{}' is not ready for take_flats: {} — run train_flats first",
            ctx.train_id,
            problems.join("; ")
        )));
    }
    let total = params
        .count
        .saturating_mul(u32::try_from(plan.len()).unwrap_or(u32::MAX));
    info!(
        train_id = %ctx.train_id,
        camera_id = %ctx.camera_id,
        count = params.count,
        groups = plan.len(),
        "taking flats"
    );

    let session = PanelSession::start(rig.active, &calibrator_id).await?;
    let outcome = take_body(
        rig.active,
        config,
        &ctx,
        &plan,
        params.count,
        target,
        &session,
        progress,
        total,
    )
    .await;
    let cleanup = session.finish(rig.cleanup).await;

    let (filters, mut warnings) = outcome?;
    warnings.extend(cleanup.warnings);
    let total_frames = filters
        .iter()
        .map(|f| f.frames)
        .fold(0u32, u32::saturating_add);
    Ok(TakeFlatsOutcome {
        train_id: ctx.train_id,
        filters,
        total_frames,
        cover_restored: cleanup.cover_restored,
        warnings,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the capture loop's inputs are the resolved train, the plan and the caller's knobs; bundling them adds a struct for one call site"
)]
async fn take_body(
    rig: &dyn FlatsRig,
    config: &Config,
    ctx: &TrainContext,
    plan: &[(Option<String>, FlatRecord)],
    count: u32,
    target: u32,
    session: &PanelSession,
    progress: &dyn Progress,
    total: u32,
) -> Result<(Vec<FilterFlats>, Vec<String>)> {
    let mut filters = Vec::new();
    let mut warnings = Vec::new();
    let mut done = 0u32;

    for (filter, record) in plan {
        let label = filter_label(filter.as_deref());
        if let (Some(wheel), Some(name)) = (&ctx.filter_wheel_id, filter) {
            debug!(filter = %name, "switching filter");
            rig.set_filter(wheel, name).await?;
        }
        let applied = session.light(rig, Some(record.brightness)).await?;
        if applied != record.brightness {
            warnings.push(format!(
                "{label}: the panel lit at brightness {applied} instead of the trained {}",
                record.brightness
            ));
        }

        let mut out_of_range = Vec::new();
        // Frame n's statistics run while frame n + 1 exposes (plan D7).
        let mut pending: Option<CaptureResult> = None;
        for index in 1..=count {
            debug!(filter = %label, frame = index, total = count, "capturing flat");
            let capture = rig.capture(&ctx.camera_id, record.duration, Frame::Flat);
            let captured = match pending.take() {
                Some(previous) => {
                    let (captured, stats) = tokio::join!(
                        capture,
                        rig.compute_image_stats(&previous.image_path, &previous.document_id)
                    );
                    verify_frame(
                        &previous,
                        stats,
                        target,
                        config.flat_warn_tolerance,
                        &mut out_of_range,
                        &mut warnings,
                    );
                    captured?
                }
                None => capture.await?,
            };
            done = done.saturating_add(1);
            progress
                .tick(
                    f64::from(done),
                    Some(f64::from(total)),
                    format!("{label}: frame {index} of {count}"),
                )
                .await;
            pending = Some(captured);
        }
        if let Some(last) = pending.take() {
            let stats = rig
                .compute_image_stats(&last.image_path, &last.document_id)
                .await;
            verify_frame(
                &last,
                stats,
                target,
                config.flat_warn_tolerance,
                &mut out_of_range,
                &mut warnings,
            );
        }

        filters.push(FilterFlats {
            filter: filter.clone(),
            duration: record.duration,
            brightness: record.brightness,
            frames: count,
            out_of_range,
        });
    }
    Ok((filters, warnings))
}

/// Judge one captured flat (plan D7): a median outside the band is a
/// warning and an `out_of_range` entry; a failed measurement is a
/// warning alone (there is no median to report). Neither is a failure.
fn verify_frame(
    frame: &CaptureResult,
    stats: Result<ImageStats>,
    target: u32,
    tolerance: f64,
    out_of_range: &mut Vec<OutOfRange>,
    warnings: &mut Vec<String>,
) {
    match stats {
        Ok(stats) => {
            let dev = deviation(target, stats.median_adu);
            if dev > tolerance {
                warn!(
                    image_path = %frame.image_path,
                    median_adu = stats.median_adu,
                    target_adu = target,
                    "flat median outside the verification band"
                );
                warnings.push(format!(
                    "{}: median {} ADU is outside {target} ± {:.0} %",
                    frame.image_path,
                    stats.median_adu,
                    tolerance * 100.0
                ));
                out_of_range.push(OutOfRange {
                    image_path: frame.image_path.clone(),
                    median_adu: stats.median_adu,
                });
            }
        }
        Err(e) => {
            warn!(image_path = %frame.image_path, error = %e, "flat could not be verified");
            warnings.push(format!(
                "{}: could not verify: {}",
                frame.image_path,
                e.tool_message()
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// get_flat_training
// ---------------------------------------------------------------------------

/// The `get_flat_training` body: the train's records (or one filter's),
/// each judged against the live camera. Reads only.
///
/// # Errors
///
/// Returns the `rp` error of `get_train_info` / `get_camera_info`, a
/// [`CalibratorFlatsError::Workflow`] for a train without a camera, or a
/// store error.
pub async fn get_flat_training(
    rig: &dyn FlatsRig,
    store: &FlatStore,
    train_id: &str,
    filter: Option<&str>,
) -> Result<TrainingOutcome> {
    let ctx = resolve_train(rig, train_id, None).await?;
    let facts = ctx.camera_facts();
    let records = match filter {
        Some(filter) => store
            .get(train_id, Some(filter))
            .await?
            .into_iter()
            .collect(),
        None => store.list(train_id).await?,
    };
    let records = records
        .into_iter()
        .map(|record| {
            let stale: Vec<String> = record
                .stale_fields(&facts)
                .iter()
                .map(ToString::to_string)
                .collect();
            TrainingView {
                status: if stale.is_empty() { "trained" } else { "stale" },
                stale,
                record,
            }
        })
        .collect();
    Ok(TrainingOutcome {
        train_id: train_id.to_owned(),
        camera: facts,
        records,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::parse_config;

    const MIN: Duration = Duration::from_micros(10);
    const MAX: Duration = Duration::from_hours(1);
    const TARGET: u32 = 32_767;

    fn camera() -> CameraInfo {
        CameraInfo {
            max_adu: 65_535,
            exposure_min: MIN,
            exposure_max: MAX,
            bin_x: 1,
            bin_y: 1,
            gain: Some(100),
            offset: Some(10),
        }
    }

    fn config(json: &str) -> Config {
        let mut base = serde_json::json!({
            "mcp_server_url": "http://localhost:1/mcp",
            "initial_duration": "1s",
            "min_exposure": "0ms",
            "max_iterations": 5,
            "tolerance": 0.05
        });
        let overrides: serde_json::Value = serde_json::from_str(json).unwrap();
        for (k, v) in overrides.as_object().unwrap() {
            base[k] = v.clone();
        }
        parse_config(&base.to_string(), "test").unwrap()
    }

    fn train_info(wheel: bool, calibrator: bool) -> TrainInfo {
        TrainInfo {
            camera_id: Some("main-cam".into()),
            filter_wheel_id: wheel.then(|| "main-fw".to_owned()),
            filters: wheel.then(|| vec!["Luminance".to_owned(), "Red".to_owned()]),
            calibrator_id: calibrator.then(|| "flat-panel".to_owned()),
        }
    }

    /// A rig that resolves `main` to `info` and answers the camera.
    fn rig_resolving(info: TrainInfo) -> MockFlatsRig {
        let mut rig = MockFlatsRig::new();
        rig.expect_get_train_info()
            .withf(|id| id == "main")
            .returning(move |_| {
                let info = info.clone();
                Box::pin(async move { Ok(info) })
            });
        rig.expect_get_camera_info()
            .returning(|_| Box::pin(async { Ok(camera()) }));
        rig
    }

    /// Answer the search's captures and statistics from a queue of
    /// medians (the last one repeats).
    fn expect_measurements(rig: &mut MockFlatsRig, medians: &[u32]) -> Arc<Mutex<Vec<Duration>>> {
        let durations = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&durations);
        rig.expect_capture().returning(move |_, duration, _| {
            seen.lock().unwrap().push(duration);
            Box::pin(async move {
                Ok(CaptureResult {
                    image_path: format!("/tmp/{}.fits", duration.as_millis()),
                    document_id: "doc".into(),
                })
            })
        });
        let queue = Arc::new(Mutex::new(
            medians.iter().copied().collect::<VecDeque<u32>>(),
        ));
        let last = *medians.last().unwrap();
        rig.expect_compute_image_stats().returning(move |_, _| {
            let median = queue.lock().unwrap().pop_front().unwrap_or(last);
            Box::pin(async move { Ok(ImageStats { median_adu: median }) })
        });
        durations
    }

    /// A cleanup rig expecting the panel off and, if `reopen`, the cover
    /// reopened.
    fn cleanup_rig(reopen: bool) -> MockFlatsRig {
        let mut rig = MockFlatsRig::new();
        rig.expect_calibrator_off()
            .times(1)
            .returning(|_| Box::pin(async { Ok(()) }));
        rig.expect_open_cover()
            .times(usize::from(reopen))
            .returning(|_| Box::pin(async { Ok(()) }));
        rig
    }

    /// The active-rig cover/panel guard expectations for a cover that
    /// starts in `initial`.
    fn expect_guard(rig: &mut MockFlatsRig, initial: &'static str) {
        rig.expect_get_cover_state()
            .times(1)
            .returning(move |_| Box::pin(async move { Ok(initial.to_owned()) }));
        rig.expect_close_cover()
            .times(1)
            .returning(|_| Box::pin(async { Ok(()) }));
    }

    async fn temp_store() -> (FlatStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FlatStore::open(dir.path().join("flats.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    fn record(filter: Option<&str>) -> FlatRecord {
        FlatRecord {
            train_id: "main".into(),
            filter: filter.map(str::to_owned),
            duration: Duration::from_millis(800),
            brightness: 127,
            median_adu: 32_000,
            max_adu: 65_535,
            bin_x: 1,
            bin_y: 1,
            gain: Some(100),
            offset: Some(10),
            camera_id: "main-cam".into(),
            trained_at: "2026-09-05T19:02:11Z".into(),
        }
    }

    // --- the proportional step ---------------------------------------

    #[test]
    fn proposed_duration_doubles_for_half_signal() {
        assert_eq!(
            proposed_duration(Duration::from_secs(1), 32_000, 16_000),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn proposed_duration_halves_for_double_signal() {
        assert_eq!(
            proposed_duration(Duration::from_secs(1), 32_000, 64_000),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn proposed_duration_doubles_when_zero_signal() {
        assert_eq!(
            proposed_duration(Duration::from_millis(500), 32_000, 0),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn proposed_duration_preserves_microsecond_precision() {
        assert_eq!(
            proposed_duration(Duration::from_micros(50), 32_000, 32_000),
            Duration::from_micros(50)
        );
    }

    #[test]
    fn proposed_duration_saturates_instead_of_overflowing() {
        assert_eq!(proposed_duration(Duration::MAX, 1, 0), Duration::MAX);
        assert_eq!(proposed_duration(Duration::MAX, 60_000, 1), Duration::MAX);
    }

    #[test]
    fn deviation_is_symmetric_and_zero_on_target() {
        assert_eq!(deviation(32_000, 32_000), 0.0);
        assert_eq!(deviation(32_000, 16_000), 0.5);
        assert_eq!(deviation(32_000, 48_000), 0.5);
    }

    #[test]
    fn the_target_is_half_the_well_and_never_zero() {
        assert_eq!(target_adu(65_535).unwrap(), 32_767);
        assert!(target_adu(1)
            .unwrap_err()
            .to_string()
            .contains("target is 0"));
    }

    // --- train resolution --------------------------------------------

    #[tokio::test]
    async fn resolution_defaults_to_every_wheel_filter_in_order() {
        let rig = rig_resolving(train_info(true, true));
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        assert_eq!(ctx.camera_id, "main-cam");
        assert_eq!(ctx.calibrator_id.as_deref(), Some("flat-panel"));
        assert_eq!(
            ctx.filters,
            vec![Some("Luminance".to_owned()), Some("Red".to_owned())]
        );
        assert_eq!(ctx.camera_facts().gain, Some(100));
    }

    #[tokio::test]
    async fn resolution_keeps_the_requested_order_and_names_an_unknown_filter() {
        let rig = rig_resolving(train_info(true, true));
        let ctx = resolve_train(&rig, "main", Some(&["Red".to_owned()]))
            .await
            .unwrap();
        assert_eq!(ctx.filters, vec![Some("Red".to_owned())]);

        let err = resolve_train(&rig, "main", Some(&["Ha".to_owned()]))
            .await
            .unwrap_err();
        assert_eq!(
            err.tool_message(),
            "filter 'Ha' is not on train 'main' (wheel 'main-fw' has: Luminance, Red)"
        );

        let err = resolve_train(&rig, "main", Some(&[])).await.unwrap_err();
        assert!(err.tool_message().contains("at least one filter"));
    }

    #[tokio::test]
    async fn a_filterless_train_is_one_group_and_refuses_filters() {
        let rig = rig_resolving(train_info(false, true));
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        assert_eq!(ctx.filters, vec![None]);
        assert!(ctx.filter_wheel_id.is_none());

        let err = resolve_train(&rig, "main", Some(&["Luminance".to_owned()]))
            .await
            .unwrap_err();
        assert_eq!(
            err.tool_message(),
            "train 'main' has no filter wheel; do not pass filters"
        );
    }

    #[tokio::test]
    async fn a_train_without_a_camera_or_calibrator_is_named() {
        let mut no_camera = train_info(true, true);
        no_camera.camera_id = None;
        let rig = rig_resolving(no_camera);
        let err = resolve_train(&rig, "main", None).await.unwrap_err();
        assert_eq!(err.tool_message(), "train 'main' has no camera");

        let rig = rig_resolving(train_info(true, false));
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        assert_eq!(
            ctx.calibrator().unwrap_err().tool_message(),
            "train 'main' has no cover calibrator"
        );
    }

    #[tokio::test]
    async fn an_rp_error_resolving_the_train_propagates() {
        let mut rig = MockFlatsRig::new();
        rig.expect_get_train_info().returning(|_| {
            Box::pin(async {
                Err(CalibratorFlatsError::ToolCall(
                    "train not found: nope".into(),
                ))
            })
        });
        let err = resolve_train(&rig, "nope", None).await.unwrap_err();
        assert_eq!(err.tool_message(), "train not found: nope");
    }

    // --- the search --------------------------------------------------

    async fn run_pass(
        rig: &MockFlatsRig,
        cfg: &Config,
        floor: Duration,
        start: Duration,
    ) -> SearchOutcome {
        let mut ticks = 0;
        search_pass(
            rig,
            "main-cam",
            cfg,
            TARGET,
            floor,
            MAX,
            start,
            &NoProgress,
            &mut ticks,
            "L",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_pass_converges_on_the_first_exposure_within_tolerance() {
        let mut rig = MockFlatsRig::new();
        expect_measurements(&mut rig, &[TARGET]);
        let pass = run_pass(&rig, &config("{}"), MIN, Duration::from_secs(1)).await;
        assert!(pass.converged);
        assert_eq!(pass.iterations, 1);
        assert_eq!(pass.duration, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_pass_adjusts_proportionally_then_converges() {
        let mut rig = MockFlatsRig::new();
        let durations = expect_measurements(&mut rig, &[TARGET / 2, TARGET]);
        let pass = run_pass(&rig, &config("{}"), MIN, Duration::from_secs(1)).await;
        assert!(pass.converged);
        assert_eq!(pass.iterations, 2);
        // Half the signal doubles the exposure (to the f64 rounding).
        let second = durations.lock().unwrap()[1];
        assert!((second.as_secs_f64() - 2.0).abs() < 1e-3, "{second:?}");
    }

    #[tokio::test]
    async fn a_pass_that_never_converges_reports_the_last_median_and_is_not_over_bright_when_dim() {
        let mut rig = MockFlatsRig::new();
        expect_measurements(&mut rig, &[1_000]);
        let pass = run_pass(
            &rig,
            &config(r#"{"max_iterations": 3}"#),
            MIN,
            Duration::from_secs(1),
        )
        .await;
        assert!(!pass.converged);
        assert!(!pass.over_bright);
        assert_eq!(pass.iterations, 3);
        assert_eq!(pass.median, 1_000);
    }

    #[tokio::test]
    async fn a_saturated_pass_is_over_bright() {
        let mut rig = MockFlatsRig::new();
        expect_measurements(&mut rig, &[65_535]);
        let pass = run_pass(
            &rig,
            &config(r#"{"max_iterations": 2}"#),
            MIN,
            Duration::from_secs(1),
        )
        .await;
        assert!(!pass.converged);
        assert!(pass.over_bright);
    }

    #[tokio::test]
    async fn a_start_below_the_floor_is_raised_to_the_floor() {
        let mut rig = MockFlatsRig::new();
        let durations = expect_measurements(&mut rig, &[TARGET]);
        let floor = Duration::from_millis(250);
        run_pass(&rig, &config("{}"), floor, Duration::from_millis(100)).await;
        assert_eq!(durations.lock().unwrap()[0], floor);
    }

    #[tokio::test]
    async fn a_step_below_the_floor_ends_the_pass_over_bright_at_once() {
        // 250 ms reads 50 % over the target: the step wants ~167 ms,
        // under the 250 ms floor — the pass stops after one exposure,
        // over-bright, with iterations to spare.
        let mut rig = MockFlatsRig::new();
        let durations = expect_measurements(&mut rig, &[TARGET + TARGET / 2]);
        let floor = Duration::from_millis(250);
        let pass = run_pass(
            &rig,
            &config(r#"{"max_iterations": 5}"#),
            floor,
            Duration::from_millis(250),
        )
        .await;
        assert!(!pass.converged);
        assert!(pass.over_bright);
        assert_eq!(pass.iterations, 1);
        assert_eq!(pass.duration, floor, "the best guess stays at the floor");
        assert_eq!(durations.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_effective_floor_is_the_larger_of_min_exposure_and_the_camera_minimum() {
        // The camera's exposure_min (2 s) beats min_exposure (250 ms):
        // the ladder's first exposure is 2 s.
        let mut rig = rig_resolving(train_info(false, true));
        let mut cam = camera();
        cam.exposure_min = Duration::from_secs(2);
        rig.checkpoint();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(false, true)) }));
        rig.expect_get_camera_info().returning(move |_| {
            let cam = cam.clone();
            Box::pin(async move { Ok(cam) })
        });
        let durations = expect_measurements(&mut rig, &[TARGET]);
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        let cfg = config(r#"{"min_exposure": "250ms", "initial_duration": "100ms"}"#);
        let mut brightness = 255;
        let mut ticks = 0;
        search_with_ladder(
            &rig,
            &ctx,
            &cfg,
            TARGET,
            &mut brightness,
            &NoProgress,
            &mut ticks,
            None,
        )
        .await
        .unwrap();
        assert_eq!(durations.lock().unwrap()[0], Duration::from_secs(2));
    }

    #[tokio::test]
    async fn the_ladder_steps_brightness_down_until_convergence() {
        // Saturated for a whole pass at 255, converges at once at 127.
        let mut rig = rig_resolving(train_info(false, true));
        expect_measurements(&mut rig, &[65_535, 65_535, TARGET]);
        rig.expect_calibrator_on()
            .times(1)
            .withf(|id, b| id == "flat-panel" && *b == Some(127))
            .returning(|_, b| Box::pin(async move { Ok(b.unwrap()) }));
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        let cfg = config(r#"{"max_iterations": 2}"#);
        let mut brightness = 255;
        let mut ticks = 0;
        let pass = search_with_ladder(
            &rig,
            &ctx,
            &cfg,
            TARGET,
            &mut brightness,
            &NoProgress,
            &mut ticks,
            None,
        )
        .await
        .unwrap();
        assert!(pass.converged);
        assert_eq!(
            brightness, 127,
            "the working level persists to the next group"
        );
        assert_eq!(pass.iterations, 3, "2 saturated + 1 converging");
        assert_eq!(ticks, 3, "one progress tick per exposure");
    }

    #[tokio::test]
    async fn the_ladder_does_not_engage_under_bright_and_stops_at_its_floor() {
        // Under-bright: no dimming, best effort.
        let mut rig = rig_resolving(train_info(false, true));
        expect_measurements(&mut rig, &[1_000]);
        rig.expect_calibrator_on().times(0);
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        let cfg = config(r#"{"max_iterations": 2}"#);
        let mut brightness = 255;
        let mut ticks = 0;
        let pass = search_with_ladder(
            &rig,
            &ctx,
            &cfg,
            TARGET,
            &mut brightness,
            &NoProgress,
            &mut ticks,
            None,
        )
        .await
        .unwrap();
        assert!(!pass.converged);
        assert_eq!(brightness, 255);

        // Over-bright at brightness 1: halving would reach 0, so the
        // ladder gives up rather than turning the panel off.
        let mut rig = rig_resolving(train_info(false, true));
        expect_measurements(&mut rig, &[65_535]);
        rig.expect_calibrator_on().times(0);
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        let mut brightness = 1;
        let pass = search_with_ladder(
            &rig,
            &ctx,
            &cfg,
            TARGET,
            &mut brightness,
            &NoProgress,
            &mut ticks,
            None,
        )
        .await
        .unwrap();
        assert!(!pass.converged);
        assert_eq!(brightness, 1);
    }

    #[tokio::test]
    async fn a_panel_that_will_not_dim_ends_the_ladder() {
        let mut rig = rig_resolving(train_info(false, true));
        expect_measurements(&mut rig, &[65_535]);
        // The device answers the old level back.
        rig.expect_calibrator_on()
            .times(1)
            .returning(|_, _| Box::pin(async { Ok(255) }));
        let ctx = resolve_train(&rig, "main", None).await.unwrap();
        let cfg = config(r#"{"max_iterations": 1}"#);
        let mut brightness = 255;
        let mut ticks = 0;
        let pass = search_with_ladder(
            &rig,
            &ctx,
            &cfg,
            TARGET,
            &mut brightness,
            &NoProgress,
            &mut ticks,
            None,
        )
        .await
        .unwrap();
        assert!(!pass.converged);
        assert_eq!(brightness, 255);
    }

    // --- train_flats -------------------------------------------------

    #[tokio::test]
    async fn train_flats_records_a_converged_filter_with_the_camera_facts_and_restores_the_cover() {
        let (store, _dir) = temp_store().await;
        let mut active = rig_resolving(train_info(true, true));
        expect_guard(&mut active, "Open");
        active
            .expect_calibrator_on()
            .times(1)
            .withf(|_, b| b.is_none())
            .returning(|_, _| Box::pin(async { Ok(255) }));
        active
            .expect_set_filter()
            .times(1)
            .withf(|wheel, f| wheel == "main-fw" && f == "Red")
            .returning(|_, _| Box::pin(async { Ok(()) }));
        expect_measurements(&mut active, &[TARGET]);
        let cleanup = cleanup_rig(true);

        let params = TrainFlatsParams {
            train_id: "main".into(),
            filters: Some(vec!["Red".into()]),
            brightness: None,
        };
        let outcome = train_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &params,
            &NoProgress,
        )
        .await
        .unwrap();

        assert!(outcome.cover_restored);
        assert!(outcome.warnings.is_empty());
        assert!(outcome.unconverged.is_empty());
        assert_eq!(outcome.trained.len(), 1);
        let stored = store.get("main", Some("Red")).await.unwrap().unwrap();
        assert_eq!(stored, outcome.trained[0]);
        assert_eq!(stored.brightness, 255);
        assert_eq!(stored.camera_id, "main-cam");
        assert_eq!(stored.gain, Some(100));
        assert_eq!(stored.duration, Duration::from_secs(1));
        assert!(stored.trained_at.ends_with('Z'), "{}", stored.trained_at);
    }

    #[tokio::test]
    async fn an_unconverged_filter_writes_nothing_and_keeps_an_earlier_record() {
        let (store, _dir) = temp_store().await;
        store.put(record(Some("Red"))).await.unwrap();
        let mut active = rig_resolving(train_info(true, true));
        expect_guard(&mut active, "Closed");
        active
            .expect_calibrator_on()
            .returning(|_, _| Box::pin(async { Ok(255) }));
        active
            .expect_set_filter()
            .returning(|_, _| Box::pin(async { Ok(()) }));
        expect_measurements(&mut active, &[1_000]);
        let cleanup = cleanup_rig(false);

        let params = TrainFlatsParams {
            train_id: "main".into(),
            filters: Some(vec!["Red".into()]),
            brightness: Some(255),
        };
        let outcome = train_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(r#"{"max_iterations": 2}"#),
            &params,
            &NoProgress,
        )
        .await
        .unwrap();

        assert!(
            !outcome.cover_restored,
            "a closed cover has nothing to restore"
        );
        assert!(outcome.trained.is_empty());
        assert_eq!(outcome.unconverged.len(), 1);
        assert_eq!(outcome.unconverged[0].filter.as_deref(), Some("Red"));
        assert_eq!(outcome.unconverged[0].median_adu, 1_000);
        assert_eq!(
            store.get("main", Some("Red")).await.unwrap(),
            Some(record(Some("Red"))),
            "the earlier record is untouched"
        );
    }

    #[tokio::test]
    async fn train_flats_cleans_up_on_a_mid_run_error_and_returns_it() {
        let (store, _dir) = temp_store().await;
        let mut active = rig_resolving(train_info(false, true));
        expect_guard(&mut active, "Open");
        active
            .expect_calibrator_on()
            .returning(|_, _| Box::pin(async { Ok(255) }));
        active.expect_capture().returning(|_, _, _| {
            Box::pin(async {
                Err(CalibratorFlatsError::ToolCall(
                    "capture: exposure aborted".into(),
                ))
            })
        });
        let mut cleanup = cleanup_rig(true);

        let err = train_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TrainFlatsParams {
                train_id: "main".into(),
                ..Default::default()
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert_eq!(err.tool_message(), "capture: exposure aborted");
        cleanup.checkpoint();
    }

    #[tokio::test]
    async fn train_flats_refuses_a_train_without_a_calibrator_before_touching_anything() {
        let (store, _dir) = temp_store().await;
        let active = rig_resolving(train_info(true, false));
        let cleanup = MockFlatsRig::new();
        let err = train_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TrainFlatsParams {
                train_id: "main".into(),
                ..Default::default()
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert_eq!(err.tool_message(), "train 'main' has no cover calibrator");
    }

    // --- take_flats --------------------------------------------------

    #[tokio::test]
    async fn take_flats_refuses_untrained_and_stale_filters_before_actuating() {
        let (store, _dir) = temp_store().await;
        let mut stale = record(Some("Red"));
        stale.gain = Some(50);
        store.put(stale).await.unwrap();
        // No cover, panel, wheel or capture expectations: any actuation
        // panics the mock.
        let active = rig_resolving(train_info(true, true));
        let cleanup = MockFlatsRig::new();

        let err = take_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TakeFlatsParams {
                train_id: "main".into(),
                count: 3,
                filters: None,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.tool_message(),
            "train 'main' is not ready for take_flats: Luminance untrained; Red stale (gain changed from 50 to 100) — run train_flats first"
        );
    }

    #[tokio::test]
    async fn take_flats_refuses_a_zero_count() {
        let (store, _dir) = temp_store().await;
        let active = MockFlatsRig::new();
        let cleanup = MockFlatsRig::new();
        let err = take_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TakeFlatsParams {
                train_id: "main".into(),
                count: 0,
                filters: None,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert_eq!(err.tool_message(), "count must be at least 1");
    }

    /// Counts progress ticks and keeps the last total.
    struct CountingProgress(Mutex<(u32, Option<f64>)>);

    #[async_trait]
    impl Progress for CountingProgress {
        async fn tick(&self, _progress: f64, total: Option<f64>, _message: String) {
            let mut guard = self.0.lock().unwrap();
            guard.0 = guard.0.saturating_add(1);
            guard.1 = total;
        }
    }

    #[tokio::test]
    async fn take_flats_captures_at_the_trained_timing_and_flags_out_of_range_frames() {
        let (store, _dir) = temp_store().await;
        store.put(record(Some("Red"))).await.unwrap();
        let mut active = rig_resolving(train_info(true, true));
        expect_guard(&mut active, "Open");
        active
            .expect_set_filter()
            .times(1)
            .returning(|_, _| Box::pin(async { Ok(()) }));
        active
            .expect_calibrator_on()
            .times(1)
            .withf(|_, b| *b == Some(127))
            .returning(|_, b| Box::pin(async move { Ok(b.unwrap()) }));
        active
            .expect_capture()
            .times(3)
            .withf(|_, duration, frame| {
                *duration == Duration::from_millis(800) && *frame == Frame::Flat
            })
            .returning(|_, _, _| {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    Ok(CaptureResult {
                        image_path: format!("/flats/{n}.fits"),
                        document_id: format!("doc-{n}"),
                    })
                })
            });
        // Frame 1 on target, frame 2 far off, frame 3 unmeasurable.
        active.expect_compute_image_stats().returning(|path, _| {
            let path = path.to_owned();
            Box::pin(async move {
                match path.as_str() {
                    "/flats/0.fits" => Ok(ImageStats { median_adu: TARGET }),
                    "/flats/1.fits" => Ok(ImageStats { median_adu: 10_000 }),
                    _ => Err(CalibratorFlatsError::ToolCall("stats: no such file".into())),
                }
            })
        });
        let cleanup = cleanup_rig(true);
        let progress = CountingProgress(Mutex::new((0, None)));

        let outcome = take_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(r#"{"flat_warn_tolerance": 0.1}"#),
            &TakeFlatsParams {
                train_id: "main".into(),
                count: 3,
                filters: Some(vec!["Red".into()]),
            },
            &progress,
        )
        .await
        .unwrap();

        assert_eq!(outcome.total_frames, 3);
        assert!(outcome.cover_restored);
        assert_eq!(outcome.filters.len(), 1);
        let red = &outcome.filters[0];
        assert_eq!(red.filter.as_deref(), Some("Red"));
        assert_eq!(red.frames, 3);
        assert_eq!(red.duration, Duration::from_millis(800));
        assert_eq!(red.brightness, 127);
        assert_eq!(
            red.out_of_range,
            vec![OutOfRange {
                image_path: "/flats/1.fits".into(),
                median_adu: 10_000
            }]
        );
        assert_eq!(outcome.warnings.len(), 2, "{:?}", outcome.warnings);
        assert!(
            outcome.warnings[0].contains("/flats/1.fits: median 10000 ADU is outside 32767 ± 10 %")
        );
        assert!(outcome.warnings[1].contains("/flats/2.fits: could not verify"));
        assert_eq!(*progress.0.lock().unwrap(), (3, Some(3.0)));
        assert!(
            store.get("main", Some("Red")).await.unwrap().is_some(),
            "take_flats never writes"
        );
    }

    #[tokio::test]
    async fn a_cancelled_take_flats_cleans_up_on_the_cleanup_rig_and_reports_the_cancellation() {
        let (store, _dir) = temp_store().await;
        store.put(record(None)).await.unwrap();
        let mut active = rig_resolving(train_info(false, true));
        expect_guard(&mut active, "Open");
        active
            .expect_calibrator_on()
            .returning(|_, b| Box::pin(async move { Ok(b.unwrap()) }));
        active.expect_capture().returning(|_, _, _| {
            Box::pin(async {
                Err(CalibratorFlatsError::Cancelled(
                    "the caller cancelled the flats run".into(),
                ))
            })
        });
        // Nothing on the active rig after the cancellation: cleanup
        // goes to the other view.
        active.expect_calibrator_off().times(0);
        active.expect_open_cover().times(0);
        let mut cleanup = cleanup_rig(true);

        let err = take_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TakeFlatsParams {
                train_id: "main".into(),
                count: 5,
                filters: None,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CalibratorFlatsError::Cancelled(_)), "{err:?}");
        assert_eq!(
            err.tool_message(),
            "cancelled: the caller cancelled the flats run"
        );
        cleanup.checkpoint();
    }

    #[tokio::test]
    async fn a_refused_reopen_is_a_warning_and_the_flats_still_succeed() {
        let (store, _dir) = temp_store().await;
        store.put(record(None)).await.unwrap();
        let mut active = rig_resolving(train_info(false, true));
        expect_guard(&mut active, "Open");
        active
            .expect_calibrator_on()
            .returning(|_, b| Box::pin(async move { Ok(b.unwrap()) }));
        active.expect_capture().returning(|_, _, _| {
            Box::pin(async {
                Ok(CaptureResult {
                    image_path: "/flats/a.fits".into(),
                    document_id: "doc".into(),
                })
            })
        });
        active
            .expect_compute_image_stats()
            .returning(|_, _| Box::pin(async { Ok(ImageStats { median_adu: TARGET }) }));
        let mut cleanup = MockFlatsRig::new();
        cleanup
            .expect_calibrator_off()
            .times(1)
            .returning(|_| Box::pin(async { Ok(()) }));
        cleanup.expect_open_cover().times(1).returning(|_| {
            Box::pin(async {
                Err(CalibratorFlatsError::SafetyRefused(
                    "open_cover: safety: conditions are unsafe".into(),
                ))
            })
        });

        let outcome = take_flats(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config("{}"),
            &TakeFlatsParams {
                train_id: "main".into(),
                count: 1,
                filters: None,
            },
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(outcome.total_frames, 1);
        assert!(!outcome.cover_restored);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(
            outcome.warnings[0].starts_with(
                "open_cover was refused — conditions are unsafe, the cover stays closed"
            ),
            "{}",
            outcome.warnings[0]
        );
    }

    #[tokio::test]
    async fn a_failed_calibrator_off_is_a_warning_too() {
        let mut cleanup = MockFlatsRig::new();
        cleanup.expect_calibrator_off().returning(|_| {
            Box::pin(async {
                Err(CalibratorFlatsError::ToolCall(
                    "calibrator_off: gone".into(),
                ))
            })
        });
        cleanup.expect_open_cover().returning(|_| {
            Box::pin(async { Err(CalibratorFlatsError::ToolCall("open_cover: gone".into())) })
        });
        let session = PanelSession {
            calibrator_id: "flat-panel".into(),
            started_open: true,
        };
        let result = session.finish(&cleanup).await;
        assert!(!result.cover_restored);
        assert_eq!(
            result.warnings,
            vec![
                "calibrator_off failed during cleanup: calibrator_off: gone",
                "open_cover failed during cleanup — the cover stays closed: open_cover: gone"
            ]
        );
    }

    // --- get_flat_training -------------------------------------------

    #[tokio::test]
    async fn get_flat_training_judges_each_record_against_the_live_camera() {
        let (store, _dir) = temp_store().await;
        store.put(record(Some("Luminance"))).await.unwrap();
        let mut stale = record(Some("Red"));
        stale.camera_id = "old-cam".into();
        stale.bin_x = 2;
        store.put(stale).await.unwrap();
        let rig = rig_resolving(train_info(true, true));

        let outcome = get_flat_training(&rig, &store, "main", None).await.unwrap();
        assert_eq!(outcome.camera.camera_id, "main-cam");
        assert_eq!(outcome.records.len(), 2);
        assert_eq!(outcome.records[0].status, "trained");
        assert!(outcome.records[0].stale.is_empty());
        assert_eq!(outcome.records[1].status, "stale");
        assert_eq!(
            outcome.records[1].stale,
            vec![
                "camera_id changed from old-cam to main-cam",
                "bin_x changed from 2 to 1"
            ]
        );

        let one = get_flat_training(&rig, &store, "main", Some("Red"))
            .await
            .unwrap();
        assert_eq!(one.records.len(), 1);
        assert_eq!(one.records[0].record.filter.as_deref(), Some("Red"));

        let none = get_flat_training(&rig, &store, "main", Some("Blue"))
            .await
            .unwrap();
        assert!(none.records.is_empty());
    }
}
