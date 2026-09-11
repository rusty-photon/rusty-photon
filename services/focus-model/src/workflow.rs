//! The focus workflow (docs/services/focus-model.md § Tools).
//!
//! Train resolution, the `focus_train` body with its prediction, sweep,
//! guiding handshake and put-back, the shared-focuser walk, and the
//! read and write tools over the store.
//!
//! Everything here talks to `rp` through [`FocusRig`], a trait the real
//! [`crate::mcp_client::McpClient`] implements and `mockall` mocks in
//! the unit tests. A tool body gets a [`Rig`] pair: the `active` rig,
//! whose calls the caller's cancellation reaches, and the `cleanup`
//! rig, whose calls it cannot — the put-back and the guiding resume run
//! on the latter.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::{Config, TrainConfig};
use crate::error::{FocusModelError, Result};
use crate::prediction::{predict, Bounds, Prediction};
use crate::sizing::{plan_sweep, Optics, SweepPlan};
use crate::store::{
    now_rfc3339, FocusRecord, FocusRun, FocusStore, LastGood, RunOutcome, RunSummary, TrainFacts,
};
use crate::sweep::{
    check_grid, grid_length, run_sweep, Confirmation, CurvePoint, Direction, Measurement,
    SweepFailure, SweepOps, SweepOutcome, SweepParams,
};

// ---------------------------------------------------------------------------
// What the workflow needs from rp
// ---------------------------------------------------------------------------

/// The members and optics of a train as `get_train_info` reports them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TrainInfo {
    #[serde(default)]
    pub camera_id: Option<String>,
    #[serde(default)]
    pub filter_wheel_id: Option<String>,
    /// The wheel's configured names in position order; `None` without
    /// a sole wheel.
    #[serde(default)]
    pub filters: Option<Vec<String>>,
    /// Each filter's configured wavelength, or null.
    #[serde(default)]
    pub filter_wavelengths_nm: Option<BTreeMap<String, Option<f64>>>,
    /// The focuser the train's own sweep moves; `None` without one.
    #[serde(default)]
    pub terminal_focuser_id: Option<String>,
    #[serde(default)]
    pub optics: Optics,
}

/// The focuser's backlash block as `rp` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Backlash {
    /// `"in"` or `"out"`: the direction every move arrives from.
    pub approach: String,
}

/// What `get_focuser_position` reports: where the focuser is and the
/// travel a move is held to.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FocuserPosition {
    pub position: i32,
    #[serde(default)]
    pub min_position: Option<i32>,
    #[serde(default)]
    pub max_position: Option<i32>,
    #[serde(default)]
    pub backlash: Option<Backlash>,
}

impl FocuserPosition {
    /// The travel a prediction and a grid must land inside.
    #[must_use]
    pub const fn bounds(&self) -> Bounds {
        Bounds {
            min: self.min_position,
            max: self.max_position,
        }
    }

    /// The walk order: the approach direction, ascending by default.
    #[must_use]
    pub fn direction(&self) -> Direction {
        match self.backlash.as_ref().map(|b| b.approach.as_str()) {
            Some("in") => Direction::Descending,
            _ => Direction::Ascending,
        }
    }
}

/// One step of `rp`'s `get_refocus_plan`.
#[derive(Debug, Clone, Deserialize)]
pub struct PlanStep {
    pub focuser_id: String,
    pub run_train_id: String,
    #[serde(default)]
    pub camera_id: Option<String>,
    /// `"capture"` or `"guide"`.
    pub metric: String,
}

impl PlanStep {
    /// Whether this step is the guiding train's metric sweep.
    #[must_use]
    pub fn is_guide(&self) -> bool {
        self.metric == "guide"
    }
}

/// The refocus sequence `rp` derives for a train.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RefocusPlan {
    #[serde(default)]
    pub guide_coupled: bool,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
}

/// Result of the `capture` tool.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureResult {
    pub document_id: String,
}

/// What `measure_stars` reports for one frame.
#[derive(Debug, Clone, Deserialize)]
pub struct StarMeasurement {
    #[serde(default)]
    pub median_hfr: Option<f64>,
    pub star_count: u32,
}

/// What `rp`'s own metric `auto_focus` reports for a guiding-train
/// step.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GuideFocusResult {
    #[serde(default)]
    pub final_position: Option<i32>,
    #[serde(default)]
    pub final_hfd: Option<f64>,
    #[serde(default)]
    pub confirmed: Option<bool>,
}

/// The `rp` tools the workflow drives, behind a trait so the bodies can
/// be tested against a mock rig.
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait FocusRig: Send + Sync {
    async fn get_train_info(&self, train_id: &str) -> Result<TrainInfo>;
    async fn get_refocus_plan(&self, train_id: &str) -> Result<RefocusPlan>;
    async fn get_focuser_position(&self, focuser_id: &str) -> Result<FocuserPosition>;
    /// `None` when the focuser implements no probe.
    async fn get_focuser_temperature(&self, focuser_id: &str) -> Result<Option<f64>>;
    async fn move_focuser(&self, focuser_id: &str, position: i32) -> Result<i32>;
    /// The filter now in the path; `None` when the wheel reports none.
    async fn get_filter(&self, filter_wheel_id: &str) -> Result<Option<String>>;
    async fn set_filter(&self, filter_wheel_id: &str, filter: &str) -> Result<()>;
    async fn capture(&self, train_id: &str, duration: Duration) -> Result<CaptureResult>;
    async fn measure_stars(
        &self,
        document_id: &str,
        min_area: usize,
        max_area: usize,
        threshold_sigma: Option<f64>,
    ) -> Result<StarMeasurement>;
    /// Whether the guider reports an active loop; a failed read is an
    /// error the caller turns into a skipped handshake.
    async fn guiding_active(&self) -> Result<bool>;
    async fn pause_guiding(&self) -> Result<()>;
    async fn resume_guiding(&self) -> Result<()>;
    /// `rp`'s PHD2-metric sweep on the guiding train.
    async fn auto_focus_guide_train(&self, train_id: &str) -> Result<GuideFocusResult>;
    /// Whether the caller has cancelled, checked between calls.
    fn is_cancelled(&self) -> bool;
}

/// Where a tool body reports progress.
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
/// calls it cannot.
#[derive(Clone, Copy)]
pub struct Rig<'a> {
    pub active: &'a dyn FocusRig,
    pub cleanup: &'a dyn FocusRig,
}

// ---------------------------------------------------------------------------
// Parameters and outcomes
// ---------------------------------------------------------------------------

/// `focus_train` arguments.
#[derive(Debug, Clone, Default)]
pub struct FocusTrainParams {
    pub train_id: String,
    pub filter: Option<String>,
    pub shared: bool,
}

/// One step of a `shared: true` walk, as the result reports it.
#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub focuser_id: String,
    pub run_train_id: String,
    pub metric: String,
    pub position: Option<i32>,
    pub hfr: Option<f64>,
    pub confirmed: Option<bool>,
}

/// What the store did with a run.
#[derive(Debug, Clone, Serialize)]
pub struct Recorded {
    pub last_good_updated: bool,
    pub runs: usize,
    /// Why the run was not written, on the rare call that focused and
    /// then could not say so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `focus_train` result.
#[derive(Debug, Clone, Serialize)]
pub struct FocusTrainOutcome {
    pub train_id: String,
    pub filter: Option<String>,
    pub prediction: Prediction,
    pub sweep: SweepPlan,
    pub position: i32,
    pub hfr: f64,
    pub best_position: i32,
    pub best_hfr: f64,
    pub confirmed: bool,
    pub fit_r_squared: f64,
    pub samples_used: usize,
    pub attempts: u32,
    pub wing_slope: Option<f64>,
    pub confirmation: Confirmation,
    pub curve_points: Vec<CurvePoint>,
    pub temperature_c: Option<f64>,
    pub guiding_paused: bool,
    pub recorded: Recorded,
    pub model: String,
    /// One entry per completed step of a `shared: true` walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepOutcome>>,
}

/// The `get_sweep_plan` result.
#[derive(Debug, Clone, Serialize)]
pub struct SweepPlanView {
    pub train_id: String,
    pub filter: Option<String>,
    #[serde(flatten)]
    pub plan: SweepPlan,
    pub optics: Optics,
    /// The most recent run's wing slope, in pixels per 100 steps.
    pub measured_slope: Option<f64>,
    /// The train's override block, or null.
    pub configured: Option<ConfiguredSweep>,
}

/// A train's configured sweep overrides.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConfiguredSweep {
    pub step_size: Option<i32>,
    pub half_width: Option<i32>,
}

/// The `get_focus_model` result: the model, never the history.
#[derive(Debug, Clone, Serialize)]
pub struct ModelView {
    pub train_id: String,
    pub model: String,
    pub stale: Vec<String>,
    pub focuser_id: Option<String>,
    pub camera_id: Option<String>,
    pub filters: Option<Vec<String>>,
    pub reference_filter: Option<String>,
    pub offsets: BTreeMap<String, i32>,
    pub temperature_coefficient: Option<f64>,
    pub coefficient_runs: Option<usize>,
    pub coefficient_span_c: Option<f64>,
    pub last_good: Vec<LastGood>,
    pub runs_recorded: usize,
    /// The most recent run without its samples: this tool answers with
    /// the model, never with history.
    pub last_run: Option<RunSummary>,
    pub updated_at: Option<String>,
}

/// The `get_focus_runs` result.
#[derive(Debug, Clone, Serialize)]
pub struct RunsView {
    pub train_id: String,
    pub filter: Option<String>,
    pub total: usize,
    pub runs: Vec<FocusRun>,
}

/// The `reset_focus_model` result.
#[derive(Debug, Clone, Serialize)]
pub struct ResetView {
    pub train_id: String,
    pub dropped: Vec<String>,
    pub kept: Vec<String>,
    /// The identity fields the reset took over from the live train,
    /// empty when the record already described it.
    pub adopted: Vec<String>,
    pub model: ModelView,
}

// ---------------------------------------------------------------------------
// Train resolution
// ---------------------------------------------------------------------------

/// A train as the workflow sees it.
#[derive(Debug, Clone)]
pub struct TrainContext {
    pub train_id: String,
    pub camera_id: String,
    pub focuser_id: String,
    pub filter_wheel_id: Option<String>,
    pub filters: Option<Vec<String>>,
    pub wavelengths: BTreeMap<String, Option<f64>>,
    pub optics: Optics,
}

impl TrainContext {
    /// The identity a record for this train is valid at.
    #[must_use]
    pub fn facts(&self) -> TrainFacts {
        TrainFacts {
            focuser_id: Some(self.focuser_id.clone()),
            camera_id: Some(self.camera_id.clone()),
            filters: self.filters.clone(),
        }
    }

    /// One filter's configured wavelength.
    #[must_use]
    pub fn wavelength_of(&self, filter: Option<&str>) -> Option<f64> {
        self.wavelengths.get(filter?).copied().flatten()
    }

    /// Check a requested filter against the wheel.
    ///
    /// # Errors
    ///
    /// Returns [`FocusModelError::Workflow`] when the train has no
    /// wheel, or the wheel does not have that filter.
    pub fn check_filter(&self, filter: &str) -> Result<()> {
        let Some(wheel) = &self.filter_wheel_id else {
            return Err(FocusModelError::Workflow(format!(
                "train '{}' has no filter wheel; do not pass filter",
                self.train_id
            )));
        };
        let names = self.filters.clone().unwrap_or_default();
        if names.iter().any(|name| name == filter) {
            return Ok(());
        }
        Err(FocusModelError::Workflow(format!(
            "filter '{filter}' is not on train '{}' (wheel '{wheel}' has: {})",
            self.train_id,
            names.join(", ")
        )))
    }
}

/// Resolve `train_id` through `rp`.
///
/// # Errors
///
/// Returns [`FocusModelError::Workflow`] if the train has no terminal
/// focuser or no camera, and the `rp` error for an unknown train.
pub async fn resolve_train(rig: &dyn FocusRig, train_id: &str) -> Result<TrainContext> {
    let info = rig.get_train_info(train_id).await?;
    let focuser_id = info.terminal_focuser_id.ok_or_else(|| {
        FocusModelError::Workflow(format!("train '{train_id}' has no terminal focuser"))
    })?;
    let camera_id = info
        .camera_id
        .ok_or_else(|| FocusModelError::Workflow(format!("train '{train_id}' has no camera")))?;
    Ok(TrainContext {
        train_id: train_id.to_owned(),
        camera_id,
        focuser_id,
        filter_wheel_id: info.filter_wheel_id,
        filters: info.filters,
        wavelengths: info.filter_wavelengths_nm.unwrap_or_default(),
        optics: info.optics,
    })
}

/// How a record reads against the train right now.
fn model_label(record: Option<&FocusRecord>, stale: &[String]) -> String {
    match record {
        None => "empty".to_owned(),
        Some(_) if stale.is_empty() => "fresh".to_owned(),
        Some(_) => format!("stale: {}", stale.join("; ")),
    }
}

/// The record for a train and how it reads: `None` when the store has
/// none, and the stale fields when the identity moved on.
async fn load_record(
    store: &FocusStore,
    ctx: &TrainContext,
) -> Result<(Option<FocusRecord>, Vec<String>)> {
    let record = store.get(&ctx.train_id).await?;
    let stale = stale_fields(record.as_ref(), ctx);
    Ok((record, stale))
}

// ---------------------------------------------------------------------------
// The sweep adapter
// ---------------------------------------------------------------------------

/// One grid point's frames, folded into the sample the fit consumes.
struct RigSweep<'a> {
    rig: &'a dyn FocusRig,
    train_id: String,
    focuser_id: String,
    train: TrainConfig,
    progress: &'a dyn Progress,
    frames: Mutex<f64>,
    total: f64,
}

/// The median of a sorted-by-value list, the lower of the two middles
/// on an even count.
fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values.get(lower_middle(values.len())).copied()
}

fn median_u32(mut values: Vec<u32>) -> u32 {
    values.sort_unstable();
    values.get(lower_middle(values.len())).copied().unwrap_or(0)
}

/// The index of the lower of the two middles, for an even count.
const fn lower_middle(len: usize) -> usize {
    len.saturating_sub(1) / 2
}

#[async_trait]
impl SweepOps for RigSweep<'_> {
    async fn move_focuser(&self, position: i32) -> std::result::Result<i32, FocusModelError> {
        self.rig.move_focuser(&self.focuser_id, position).await
    }

    async fn measure(&self) -> std::result::Result<Measurement, FocusModelError> {
        let mut hfrs = Vec::new();
        let mut counts = Vec::new();
        let mut document_id = String::new();
        for _ in 0..self.train.frames_per_step.get() {
            let frame = self
                .rig
                .capture(&self.train_id, self.train.duration)
                .await?;
            let measured = self
                .rig
                .measure_stars(
                    &frame.document_id,
                    self.train.min_area,
                    self.train.max_area,
                    self.train.threshold_sigma,
                )
                .await?;
            if let Some(hfr) = measured.median_hfr.filter(|hfr| hfr.is_finite()) {
                hfrs.push(hfr);
            }
            counts.push(measured.star_count);
            document_id = frame.document_id;
        }
        Ok(Measurement {
            hfr: median(hfrs),
            star_count: median_u32(counts),
            document_id,
        })
    }

    fn check_cancelled(&self) -> std::result::Result<(), FocusModelError> {
        if self.rig.is_cancelled() {
            return Err(FocusModelError::Cancelled(
                "the caller cancelled the focus run".to_owned(),
            ));
        }
        Ok(())
    }

    async fn tick(&self, position: i32, measurement: &Measurement) {
        let done = {
            let mut frames = self
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *frames += 1.0;
            *frames
        };
        let hfr = measurement
            .hfr
            .map_or_else(|| "no stars".to_owned(), |hfr| format!("HFR {hfr:.2}"));
        self.progress
            .tick(
                done,
                Some(self.total),
                format!("position {position}: {hfr}"),
            )
            .await;
    }
}

// ---------------------------------------------------------------------------
// focus_train
// ---------------------------------------------------------------------------

/// What the body needs to put back if the sweep does not succeed.
struct Guard {
    focuser_id: String,
    started_at: i32,
    guiding_paused: bool,
}

impl Guard {
    /// Move the focuser back and resume guiding, on the cleanup rig.
    /// Nothing here fails the call: a failed put-back is named in the
    /// error the caller already has.
    async fn put_back(&self, rig: &dyn FocusRig) -> Option<String> {
        let mut note = None;
        match self.restore(rig).await {
            Ok(()) => debug!(
                focuser_id = %self.focuser_id,
                position = self.started_at,
                "focuser restored to the position the call started at"
            ),
            Err(e) => {
                warn!(error = %e, "could not restore the starting position");
                note = Some(format!(
                    "the focuser could not be restored to {}: {}",
                    self.started_at,
                    e.tool_message()
                ));
            }
        }
        if self.guiding_paused {
            if let Err(e) = rig.resume_guiding().await {
                warn!(error = %e, "could not resume guiding after the failed sweep");
                note = Some(note.map_or_else(
                    || format!("guiding could not be resumed: {e}"),
                    |first| format!("{first}; guiding could not be resumed: {e}"),
                ));
            }
        }
        note
    }
}

impl Guard {
    /// Move back to where the call found the focuser, and read it back
    /// to be sure.
    ///
    /// The read is not ceremony: the sweep's own move is often still in
    /// flight when a cancellation ends the walk, and a move `rp`
    /// abandoned part way can carry the focuser on after the put-back
    /// has landed. One more move settles it, and a focuser still
    /// somewhere else is named rather than assumed home.
    async fn restore(&self, rig: &dyn FocusRig) -> Result<()> {
        for attempt in 0..2 {
            rig.move_focuser(&self.focuser_id, self.started_at).await?;
            let seen = rig.get_focuser_position(&self.focuser_id).await?.position;
            if seen == self.started_at {
                return Ok(());
            }
            debug!(
                focuser_id = %self.focuser_id,
                seen,
                wanted = self.started_at,
                attempt,
                "the focuser did not settle where the call started; moving back again"
            );
        }
        Err(FocusModelError::Workflow(format!(
            "the focuser did not settle at {}",
            self.started_at
        )))
    }
}

/// The state a sweep starts from: where the focuser is, how warm, and
/// what it may not leave.
struct StartState {
    position: FocuserPosition,
    temperature_c: Option<f64>,
    filter: Option<String>,
}

/// Read the focuser and the wheel before anything moves.
async fn read_start(rig: &dyn FocusRig, ctx: &TrainContext) -> Result<StartState> {
    let position = rig.get_focuser_position(&ctx.focuser_id).await?;
    // The temperature is an optional term of the prediction, so a
    // focuser with no probe — or one whose probe just hiccupped — is
    // a sweep without a temperature correction, not a failed call.
    let temperature_c = match rig.get_focuser_temperature(&ctx.focuser_id).await {
        Ok(reading) => reading,
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => {
            debug!(error = %e, "the focuser reported no temperature; predicting without it");
            None
        }
    };
    let filter = match &ctx.filter_wheel_id {
        Some(wheel) => rig.get_filter(wheel).await?,
        None => None,
    };
    Ok(StartState {
        position,
        temperature_c,
        filter,
    })
}

/// Pause guide corrections when the focuser is guide-coupled and the
/// guider reports an active loop.
///
/// The coupling is read first, and a plan that cannot be read fails
/// the call: it is the only thing that says whether the guiding train
/// shares this focuser, and sweeping one it does share while
/// corrections run is what the handshake exists to prevent. A failed
/// guider read is the documented skip; a failed plan read is not.
async fn pause_for_sweep(rig: &dyn FocusRig, ctx: &TrainContext) -> Result<bool> {
    let plan = rig.get_refocus_plan(&ctx.train_id).await?;
    pause_if_coupled(rig, plan.guide_coupled).await
}

/// The handshake itself, for a coupling the caller has already read.
async fn pause_if_coupled(rig: &dyn FocusRig, guide_coupled: bool) -> Result<bool> {
    if !guide_coupled {
        return Ok(false);
    }
    match rig.guiding_active().await {
        Ok(true) => {}
        Ok(false) => {
            debug!("the guider reports no active loop; skipping the handshake");
            return Ok(false);
        }
        // The caller giving up is not a guider that could not be
        // read: it ends the call here rather than moving anything.
        Err(e) if e.is_cancelled() => return Err(e),
        Err(e) => {
            warn!(error = %e, "the guider stats could not be read; skipping the handshake");
            return Ok(false);
        }
    }
    rig.pause_guiding().await?;
    Ok(true)
}

/// The sweep parameters for one train, from its config and the plan.
fn sweep_params(train: &TrainConfig, plan: &SweepPlan, start: &FocuserPosition) -> SweepParams {
    SweepParams {
        step_size: plan.step_size,
        half_width: plan.half_width,
        min_fit_points: train.min_fit_points.get(),
        min_star_fraction: train.min_star_fraction.get(),
        confirmation_tolerance: train.confirmation_tolerance.get(),
        max_attempts: train.max_attempts.get(),
        direction: start.direction(),
        min_position: start.min_position,
        max_position: start.max_position,
    }
}

/// The smallest prediction move worth making: half a critical focus
/// zone when the optics know one, the configured floor otherwise.
fn min_prediction_move(config: &Config, plan: &SweepPlan) -> i32 {
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "`f64` to `i32` has no total spelling; `as` saturates at the rails and the max below lifts a zero or NaN to the configured floor"
    )]
    let half_cfz = plan
        .cfz_steps
        .map_or(0, |steps| (steps / 2.0).round() as i32);
    half_cfz.max(config.min_prediction_move.get())
}

/// The record a run is written into: the held one, or a fresh record
/// when the identity moved on.
fn record_for_write(
    record: Option<FocusRecord>,
    stale: &[String],
    ctx: &TrainContext,
) -> (FocusRecord, bool) {
    match record {
        Some(record) if stale.is_empty() => (record, false),
        Some(_) | None => (
            FocusRecord::new(
                &ctx.train_id,
                Some(&ctx.focuser_id),
                Some(&ctx.camera_id),
                ctx.filters.clone(),
            ),
            !stale.is_empty(),
        ),
    }
}

/// Append `run` to the train's record and report what the store did.
///
/// The record is re-read inside the store's write lock rather than
/// reused from the one the call loaded before its sweep: minutes pass
/// between the two, and another call's run must not be overwritten.
async fn record_run(
    store: &FocusStore,
    config: &Config,
    ctx: &TrainContext,
    run: FocusRun,
) -> Result<(Recorded, String)> {
    let last_good_updated = run.last_good().is_some();
    let kept = config.runs_kept.get();
    let (record, model) = store
        .update(&ctx.train_id, move |held| {
            let stale = stale_fields(held.as_ref(), ctx);
            let (mut record, was_reset) = record_for_write(held, &stale, ctx);
            record.push_run(run, kept);
            let model = if was_reset {
                format!("reset: {}", stale.join("; "))
            } else if stale.is_empty() {
                "fresh".to_owned()
            } else {
                format!("stale: {}", stale.join("; "))
            };
            Ok::<_, FocusModelError>((record, model))
        })
        .await?;
    Ok((
        Recorded {
            last_good_updated,
            runs: record.runs.len(),
            error: None,
        },
        model,
    ))
}

/// How a held record reads against the live train, as messages.
fn stale_fields(record: Option<&FocusRecord>, ctx: &TrainContext) -> Vec<String> {
    record.map_or_else(Vec::new, |record| {
        record
            .stale_fields(&ctx.facts())
            .iter()
            .map(ToString::to_string)
            .collect()
    })
}

/// The sweep's filter: the argument when given, else what the wheel
/// reports now.
fn sweep_filter(
    ctx: &TrainContext,
    params: &FocusTrainParams,
    start: &StartState,
) -> Result<Option<String>> {
    match &params.filter {
        Some(filter) => {
            ctx.check_filter(filter)?;
            Ok(Some(filter.clone()))
        }
        None => Ok(start.filter.clone()),
    }
}

/// The services one focus call reads, writes and puts back through.
#[derive(Clone, Copy)]
struct Session<'a> {
    rig: Rig<'a>,
    store: &'a FocusStore,
    config: &'a Config,
}

/// Who owns the guiding handshake around a sweep.
#[derive(Debug, Clone, Copy)]
enum Guiding {
    /// This call pauses before its own first move and resumes after
    /// its last one.
    Own,
    /// The caller paused before the walk and resumes after it: a
    /// shared walk holds one pause across every capture step.
    Held { paused: bool },
}

/// The `focus_train` body (docs/services/focus-model.md § `focus_train`).
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], the sizing error
/// of a train with incomplete optics and no configured sweep, the
/// filter errors of [`TrainContext::check_filter`], and — after the
/// put-back — the sweep's own failure, an `rp` error or the caller's
/// cancellation.
pub async fn focus_train(
    rig: Rig<'_>,
    store: &FocusStore,
    config: &Config,
    params: &FocusTrainParams,
    progress: &dyn Progress,
) -> Result<FocusTrainOutcome> {
    focus_one(
        Session { rig, store, config },
        params,
        progress,
        Guiding::Own,
    )
    .await
}

/// One train's sweep: prepare, approach, walk, record. Every exit past
/// the preparation puts the focuser back and writes the run, whether
/// the sweep fitted, the rig failed or the caller cancelled.
async fn focus_one(
    session: Session<'_>,
    params: &FocusTrainParams,
    progress: &dyn Progress,
    guiding: Guiding,
) -> Result<FocusTrainOutcome> {
    let prepared = prepare(session.rig.active, session.store, session.config, params).await?;
    // The run exists before the handshake: a guider that will not
    // pause, a focuser that refuses the predicted start and a sweep
    // that fails to fit are all failed runs, and the history is where
    // the morning after reads them.
    let run_base = FocusRun::new(
        now_rfc3339(),
        prepared.filter.clone(),
        RunOutcome::Error,
        prepared.plan.step_size,
        prepared.plan.half_width,
        prepared.plan.source,
    );
    // A focuser parked outside its configured travel cannot be put
    // back where the call found it — `rp` would refuse the move — so
    // the call is refused before it starts rather than after.
    if let Err(e) = within_travel(&prepared.start.position) {
        return Err(refuse(session, &prepared, run_base, e).await);
    }
    // And the grid itself, around the centre the sweep will use: a
    // grid too wide or clamped too small is an error before any
    // motion, which it would not be if `run_sweep` found out after
    // the move to the predicted start.
    let params = sweep_params(&prepared.train, &prepared.plan, &prepared.start.position);
    if let Err(failure) = check_grid(planned_centre(&prepared), params) {
        let e = FocusModelError::Workflow(failure.to_string());
        return Err(refuse(session, &prepared, run_base, e).await);
    }
    let guiding_paused = match guiding {
        Guiding::Own => match pause_for_sweep(session.rig.active, &prepared.ctx).await {
            Ok(paused) => paused,
            // Nothing has moved and nothing was paused, so there is
            // nothing to put back — only the attempt to record.
            Err(e) => return Err(refuse(session, &prepared, run_base, e).await),
        },
        Guiding::Held { paused } => paused,
    };
    let guard = Guard {
        focuser_id: prepared.ctx.focuser_id.clone(),
        started_at: prepared.start.position.position,
        guiding_paused: guiding_paused && matches!(guiding, Guiding::Own),
    };

    let centre = match approach(session.rig, &prepared).await {
        Ok(centre) => centre,
        Err(e) => return Err(abandon(session, &guard, &prepared, run_base, e).await),
    };

    let sweeper = RigSweep {
        rig: session.rig.active,
        train_id: prepared.ctx.train_id.clone(),
        focuser_id: prepared.ctx.focuser_id.clone(),
        train: prepared.train.clone(),
        progress,
        frames: Mutex::new(0.0),
        // The grid this sweep will actually walk, bounds included,
        // and the confirmation frame after it. A retry walks another
        // grid and pushes progress past the total, which is what a
        // repeated sweep looks like from outside.
        total: f64::from(grid_length(centre, params).saturating_add(1)),
    };
    match run_sweep(&sweeper, centre, params).await {
        Ok(outcome) => finish_success(session, &guard, prepared, run_base, &outcome).await,
        Err(failure) => {
            let mut run = run_base;
            run.temperature_c = prepared.start.temperature_c;
            run.prediction = Some(prepared.prediction.clone());
            let error = failure_error(&failure, &mut run, &prepared.prediction);
            Err(put_back_and_record(session, &guard, &prepared.ctx, run, error).await)
        }
    }
}

/// Where the sweep will be centred: the prediction when there is one
/// worth moving to, otherwise where the focuser already is.
const fn planned_centre(prepared: &Prepared) -> i32 {
    match (prepared.prediction.moved, prepared.prediction.start) {
        (true, Some(start)) => start,
        _ => prepared.start.position.position,
    }
}

/// Whether the put-back could reach the position the call started at.
///
/// A bound tightened under a focuser that is already past it makes the
/// starting position unreachable: the sweep's grid clamps into range,
/// and the move back does not.
fn within_travel(position: &FocuserPosition) -> Result<()> {
    if position.bounds().contains(position.position) {
        return Ok(());
    }
    Err(FocusModelError::Workflow(format!(
        "the focuser is at {}, outside its configured travel {}; nothing was moved",
        position.position,
        position.bounds().describe()
    )))
}

/// Move to the predicted start and put the requested filter in the
/// path: everything between the reads and the walk.
async fn approach(rig: Rig<'_>, prepared: &Prepared) -> Result<i32> {
    let ctx = &prepared.ctx;
    let centre = match (prepared.prediction.moved, prepared.prediction.start) {
        (true, Some(start)) => rig.active.move_focuser(&ctx.focuser_id, start).await?,
        _ => prepared.start.position.position,
    };
    if let (Some(wheel), Some(name)) = (&ctx.filter_wheel_id, prepared.filter.as_deref()) {
        if prepared.start.filter.as_deref() != Some(name) {
            rig.active.set_filter(wheel, name).await?;
        }
    }
    Ok(centre)
}

/// A sweep that produced a position: resume guiding, record the run,
/// answer. A resume that fails is the caller's error, but the run is
/// recorded first — the focus happened and the model learns from it.
async fn finish_success(
    session: Session<'_>,
    guard: &Guard,
    prepared: Prepared,
    run_base: FocusRun,
    outcome: &SweepOutcome,
) -> Result<FocusTrainOutcome> {
    let mut run = run_base.with_outcome(outcome);
    run.temperature_c = prepared.start.temperature_c;
    run.prediction = Some(prepared.prediction.clone());
    let resumed = resume_after(session.rig, guard, outcome.position).await;
    if let Err(e) = &resumed {
        run.error = Some(e.tool_message());
    }
    let recorded = record_run(session.store, session.config, &prepared.ctx, run).await;
    // The guider outranks the store: corrections left paused is what
    // the caller has to act on tonight, a write that failed is what
    // the log is for.
    if let Err(resume) = resumed {
        if let Err(e) = recorded {
            warn!(train_id = %prepared.ctx.train_id, error = %e, "the run could not be recorded either");
        }
        return Err(resume);
    }
    let (recorded, model) = recorded_or_not(recorded, &prepared.ctx.train_id);
    Ok(succeeded(
        Completed {
            ctx: prepared.ctx,
            filter: prepared.filter,
            prediction: prepared.prediction,
            plan: prepared.plan,
            temperature_c: prepared.start.temperature_c,
            guiding_paused: guard.guiding_paused,
        },
        outcome,
        recorded,
        model,
    ))
}

/// What a successful call reports when the store could not take its
/// run: the focus stands — the focuser is at it — and the record does
/// not. Failing the call over this would have `rp` emit `focus_failed`
/// for a train that is in focus, and a document retry a sweep that
/// worked.
fn recorded_or_not(written: Result<(Recorded, String)>, train_id: &str) -> (Recorded, String) {
    match written {
        Ok(both) => both,
        Err(e) => {
            warn!(
                train_id,
                error = %e,
                "the run could not be recorded; the focus itself stands"
            );
            (
                Recorded {
                    last_good_updated: false,
                    runs: 0,
                    error: Some(e.tool_message()),
                },
                "unrecorded".to_owned(),
            )
        }
    }
}

/// Resume the guiding this call paused, on the cleanup rig: a
/// cancellation arriving after the last frame must not cancel the
/// resume with it.
async fn resume_after(rig: Rig<'_>, guard: &Guard, position: i32) -> Result<()> {
    if !guard.guiding_paused {
        return Ok(());
    }
    rig.cleanup.resume_guiding().await.map_err(|e| {
        FocusModelError::Workflow(format!(
            "the sweep finished at {position} but guiding could not be resumed: {}",
            e.tool_message()
        ))
    })
}

/// A call that failed before the sweep: mark the run, put back, record.
async fn abandon(
    session: Session<'_>,
    guard: &Guard,
    prepared: &Prepared,
    run: FocusRun,
    error: FocusModelError,
) -> FocusModelError {
    let run = mark_failed(run, &error, prepared);
    put_back_and_record(session, guard, &prepared.ctx, run, error).await
}

/// A call that failed before anything moved: nothing to put back, but
/// the attempt still belongs in the history.
async fn refuse(
    session: Session<'_>,
    prepared: &Prepared,
    run: FocusRun,
    error: FocusModelError,
) -> FocusModelError {
    let run = mark_failed(run, &error, prepared);
    record_failure(session, &prepared.ctx, run).await;
    error
}

/// Stamp a failure onto the run the call will be remembered by.
fn mark_failed(mut run: FocusRun, error: &FocusModelError, prepared: &Prepared) -> FocusRun {
    run.outcome = if error.is_cancelled() {
        RunOutcome::Cancelled
    } else {
        RunOutcome::Error
    };
    run.error = Some(error.tool_message());
    run.temperature_c = prepared.start.temperature_c;
    run.prediction = Some(prepared.prediction.clone());
    run
}

/// Write a failed run, logging a store that cannot take it: the
/// caller is already holding the failure it needs to read.
async fn record_failure(session: Session<'_>, ctx: &TrainContext, run: FocusRun) {
    if let Err(e) = record_run(session.store, session.config, ctx, run).await {
        warn!(train_id = %ctx.train_id, error = %e, "the failed run could not be recorded");
    }
}

/// Put the focuser back, record the failed run and hand the failure to
/// the caller. A store that cannot take the run is logged, never
/// substituted for the failure the caller is waiting to read.
async fn put_back_and_record(
    session: Session<'_>,
    guard: &Guard,
    ctx: &TrainContext,
    run: FocusRun,
    error: FocusModelError,
) -> FocusModelError {
    let note = guard.put_back(session.rig.cleanup).await;
    record_failure(session, ctx, run).await;
    append_note(error, note)
}

/// What the call resolved before anything moved.
struct Prepared {
    ctx: TrainContext,
    start: StartState,
    filter: Option<String>,
    train: TrainConfig,
    plan: SweepPlan,
    prediction: Prediction,
}

/// Resolve the train, read the rig, load the record, size the sweep
/// and predict its start. Reads only: nothing here moves.
async fn prepare(
    rig: &dyn FocusRig,
    store: &FocusStore,
    config: &Config,
    params: &FocusTrainParams,
) -> Result<Prepared> {
    let ctx = resolve_train(rig, &params.train_id).await?;
    let start = read_start(rig, &ctx).await?;
    let filter = sweep_filter(&ctx, params, &start)?;
    let (record, stale) = load_record(store, &ctx).await?;
    let usable = record.as_ref().filter(|_| stale.is_empty());

    let train = config.train(&ctx.train_id);
    let plan = plan_sweep(
        &ctx.train_id,
        &ctx.optics,
        &train,
        &config.sweep,
        ctx.wavelength_of(filter.as_deref()),
        usable
            .and_then(|record| record.last_good_for(filter.as_deref()))
            .map(|entry| entry.hfr),
    )?;
    let prediction = predict(
        usable,
        filter.as_deref(),
        start.position.position,
        start.temperature_c,
        start.position.bounds(),
        min_prediction_move(config, &plan),
    );

    info!(
        train_id = %ctx.train_id,
        focuser_id = %ctx.focuser_id,
        filter = ?filter,
        step_size = plan.step_size,
        half_width = plan.half_width,
        source = ?plan.source,
        predicted_start = ?prediction.start,
        "focusing"
    );

    Ok(Prepared {
        ctx,
        start,
        filter,
        train,
        plan,
        prediction,
    })
}

/// What the call knew before the sweep, carried into its result.
struct Completed {
    ctx: TrainContext,
    filter: Option<String>,
    prediction: Prediction,
    plan: SweepPlan,
    temperature_c: Option<f64>,
    guiding_paused: bool,
}

/// The result of a sweep that produced a position.
fn succeeded(
    call: Completed,
    outcome: &SweepOutcome,
    recorded: Recorded,
    model: String,
) -> FocusTrainOutcome {
    FocusTrainOutcome {
        train_id: call.ctx.train_id,
        filter: call.filter,
        prediction: call.prediction,
        sweep: call.plan,
        position: outcome.position,
        hfr: outcome.hfr,
        best_position: outcome.best_position,
        best_hfr: outcome.best_hfr,
        confirmed: outcome.confirmed,
        fit_r_squared: outcome.fit_r_squared,
        samples_used: outcome.samples_used,
        attempts: outcome.attempts,
        wing_slope: outcome.wing_slope,
        confirmation: outcome.confirmation.clone(),
        curve_points: outcome.curve_points.clone(),
        temperature_c: call.temperature_c,
        guiding_paused: call.guiding_paused,
        recorded,
        model,
        steps: None,
    }
}

/// Fold the sweep's failure into the run being recorded and the error
/// the caller sees.
fn failure_error(
    failure: &SweepFailure,
    run: &mut FocusRun,
    prediction: &Prediction,
) -> FocusModelError {
    match failure {
        SweepFailure::Fit {
            error,
            attempts,
            curve_points,
        } => {
            run.outcome = match error.outcome() {
                "monotonic_curve" => RunOutcome::MonotonicCurve,
                _ => RunOutcome::NotEnoughStars,
            };
            run.attempts = Some(*attempts);
            run.curve_points.clone_from(curve_points);
            let points = serde_json::to_string(curve_points).unwrap_or_else(|_| "[]".to_owned());
            let prediction_json =
                serde_json::to_string(prediction).unwrap_or_else(|_| "null".to_owned());
            run.error = Some(error.to_string());
            FocusModelError::Workflow(format!(
                "{error}; attempts: {attempts}; prediction: {prediction_json}; \
                 curve_points: {points}"
            ))
        }
        SweepFailure::Grid(message) => {
            run.outcome = RunOutcome::Error;
            run.error = Some(message.clone());
            FocusModelError::Workflow(message.clone())
        }
        SweepFailure::Rig {
            error,
            curve_points,
        } => {
            run.outcome = if error.is_cancelled() {
                RunOutcome::Cancelled
            } else {
                RunOutcome::Error
            };
            run.error = Some(error.tool_message());
            run.curve_points.clone_from(curve_points);
            FocusModelError::Workflow(error.tool_message())
        }
    }
}

/// Name a failed put-back beside the error that caused it.
fn append_note(error: FocusModelError, note: Option<String>) -> FocusModelError {
    match note {
        None => error,
        Some(note) => FocusModelError::Workflow(format!("{}; {note}", error.tool_message())),
    }
}

// ---------------------------------------------------------------------------
// The shared walk
// ---------------------------------------------------------------------------

/// Walk `rp`'s refocus plan for the train: a full sweep per capture
/// step in the train where that focuser is terminal, the guiding step
/// last through `rp`'s own metric sweep.
///
/// One guiding pause covers every capture step — the steps move the
/// same shared focuser, and resuming between them would let
/// corrections run into the next sweep — and is released before the
/// guide step, which needs the loop running to measure.
///
/// # Errors
///
/// Returns the plan's own error, a [`FocusModelError::Workflow`] for a
/// plan with no capture step, and the first failing step's error with
/// that step's focuser put back; the steps before it completed and are
/// left where they focused.
pub async fn focus_shared(
    rig: Rig<'_>,
    store: &FocusStore,
    config: &Config,
    params: &FocusTrainParams,
    progress: &dyn Progress,
) -> Result<FocusTrainOutcome> {
    let session = Session { rig, store, config };
    let plan = rig.active.get_refocus_plan(&params.train_id).await?;
    // Refused before the guide step actuates anything: its result has
    // nowhere to go without a capture step to report.
    if !plan.steps.iter().any(|step| !step.is_guide()) {
        return Err(no_capture_step(&params.train_id));
    }
    let mut paused = pause_if_coupled(rig.active, plan.guide_coupled).await?;
    let walked_paused = paused;

    let walked = walk_plan(session, &plan, params, progress, &mut paused).await;
    let resumed = release_guiding(rig, &mut paused, "after the shared walk").await;

    let (steps, mut outcome) = match walked {
        Ok(walked) => walked,
        Err(e) => {
            return Err(match resumed {
                Ok(()) => e,
                Err(resume) => append_note(e, Some(resume.tool_message())),
            })
        }
    };
    resumed?;
    outcome.train_id.clone_from(&params.train_id);
    outcome.guiding_paused = walked_paused;
    outcome.steps = Some(steps);
    Ok(outcome)
}

/// Run the plan's steps in order under the walk's guiding pause.
async fn walk_plan(
    session: Session<'_>,
    plan: &RefocusPlan,
    params: &FocusTrainParams,
    progress: &dyn Progress,
    paused: &mut bool,
) -> Result<(Vec<StepOutcome>, FocusTrainOutcome)> {
    let mut steps = Vec::new();
    let mut last: Option<FocusTrainOutcome> = None;
    for step in &plan.steps {
        if step.is_guide() {
            release_guiding(session.rig, paused, "before the guide step").await?;
            let guide = session
                .rig
                .active
                .auto_focus_guide_train(&step.run_train_id)
                .await?;
            steps.push(StepOutcome {
                focuser_id: step.focuser_id.clone(),
                run_train_id: step.run_train_id.clone(),
                metric: step.metric.clone(),
                position: guide.final_position,
                hfr: guide.final_hfd,
                confirmed: guide.confirmed,
            });
            continue;
        }
        let step_params = FocusTrainParams {
            train_id: step.run_train_id.clone(),
            filter: params.filter.clone(),
            shared: false,
        };
        let outcome = focus_one(
            session,
            &step_params,
            progress,
            Guiding::Held { paused: *paused },
        )
        .await?;
        steps.push(StepOutcome {
            focuser_id: step.focuser_id.clone(),
            run_train_id: step.run_train_id.clone(),
            metric: step.metric.clone(),
            position: Some(outcome.position),
            hfr: Some(outcome.hfr),
            confirmed: Some(outcome.confirmed),
        });
        last = Some(outcome);
    }
    let last = last.ok_or_else(|| no_capture_step(&params.train_id))?;
    Ok((steps, last))
}

/// Resume the walk's guiding pause, once, on the cleanup rig. The
/// pause is only cleared when the resume lands, so a walk whose first
/// attempt failed tries once more on its way out.
async fn release_guiding(rig: Rig<'_>, paused: &mut bool, when: &str) -> Result<()> {
    if !*paused {
        return Ok(());
    }
    let resumed = rig.cleanup.resume_guiding().await;
    *paused = resumed.is_err();
    resumed.map_err(|e| {
        FocusModelError::Workflow(format!(
            "guiding could not be resumed {when}: {}",
            e.tool_message()
        ))
    })
}

/// A plan whose only step is the guiding metric sweep.
fn no_capture_step(train_id: &str) -> FocusModelError {
    FocusModelError::Workflow(format!("train '{train_id}' has no capture step to focus"))
}

// ---------------------------------------------------------------------------
// The reads and the store writes
// ---------------------------------------------------------------------------

/// The `get_sweep_plan` body: the sweep a `focus_train` would run.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], the filter
/// errors of [`TrainContext::check_filter`], and the sizing error of a
/// train with incomplete optics and no configured sweep.
pub async fn get_sweep_plan(
    rig: &dyn FocusRig,
    store: &FocusStore,
    config: &Config,
    train_id: &str,
    filter: Option<&str>,
) -> Result<SweepPlanView> {
    let ctx = resolve_train(rig, train_id).await?;
    // The sweep `focus_train` would run: with no argument it sweeps
    // the filter in the path, so the preview reads the wheel too.
    let filter = match filter {
        Some(filter) => {
            ctx.check_filter(filter)?;
            Some(filter.to_owned())
        }
        None => match &ctx.filter_wheel_id {
            Some(wheel) => rig.get_filter(wheel).await?,
            None => None,
        },
    };
    let filter = filter.as_deref();
    let (record, stale) = load_record(store, &ctx).await?;
    let usable = record.as_ref().filter(|_| stale.is_empty());
    let train = config.train(train_id);
    let plan = plan_sweep(
        train_id,
        &ctx.optics,
        &train,
        &config.sweep,
        ctx.wavelength_of(filter),
        usable
            .and_then(|record| record.last_good_for(filter))
            .map(|entry| entry.hfr),
    )?;
    let measured_slope = usable
        .and_then(|record| record.recent_runs(1, filter).first().copied())
        .and_then(|run| run.wing_slope);
    Ok(SweepPlanView {
        train_id: ctx.train_id.clone(),
        filter: filter.map(str::to_owned),
        plan,
        optics: ctx.optics,
        measured_slope,
        configured: configured_sweep(&train),
    })
}

/// A train's overrides, or null when it sets neither.
fn configured_sweep(train: &TrainConfig) -> Option<ConfiguredSweep> {
    let step_size = train.step_size.map(crate::config::Steps::get);
    let half_width = train.half_width.map(crate::config::Steps::get);
    if step_size.is_none() && half_width.is_none() {
        return None;
    }
    Some(ConfiguredSweep {
        step_size,
        half_width,
    })
}

/// The model view of a record, judged against the live train.
fn model_view(train_id: &str, record: Option<&FocusRecord>, stale: Vec<String>) -> ModelView {
    let model = model_label(record, &stale);
    let Some(record) = record else {
        return ModelView {
            train_id: train_id.to_owned(),
            model,
            stale,
            focuser_id: None,
            camera_id: None,
            filters: None,
            reference_filter: None,
            offsets: BTreeMap::new(),
            temperature_coefficient: None,
            coefficient_runs: None,
            coefficient_span_c: None,
            last_good: Vec::new(),
            runs_recorded: 0,
            last_run: None,
            updated_at: None,
        };
    };
    ModelView {
        train_id: train_id.to_owned(),
        model,
        stale,
        focuser_id: record.focuser_id.clone(),
        camera_id: record.camera_id.clone(),
        filters: record.filters.clone(),
        reference_filter: record.reference_filter.clone(),
        offsets: record.offsets.clone(),
        temperature_coefficient: record.temperature_coefficient,
        coefficient_runs: record.coefficient_runs,
        coefficient_span_c: record.coefficient_span_c,
        last_good: record.last_good.clone(),
        runs_recorded: record.runs.len(),
        last_run: record.runs.last().map(RunSummary::from),
        updated_at: Some(record.updated_at.clone()),
    }
}

/// The `get_focus_model` body.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`] and the store's.
pub async fn get_focus_model(
    rig: &dyn FocusRig,
    store: &FocusStore,
    train_id: &str,
) -> Result<ModelView> {
    let ctx = resolve_train(rig, train_id).await?;
    let (record, stale) = load_record(store, &ctx).await?;
    Ok(model_view(train_id, record.as_ref(), stale))
}

/// The `get_focus_runs` body.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a
/// [`FocusModelError::Workflow`] for a `limit` of 0, and the store's.
pub async fn get_focus_runs(
    rig: &dyn FocusRig,
    store: &FocusStore,
    train_id: &str,
    limit: usize,
    filter: Option<&str>,
) -> Result<RunsView> {
    if limit == 0 {
        return Err(FocusModelError::Workflow(
            "limit must be at least 1".to_owned(),
        ));
    }
    let ctx = resolve_train(rig, train_id).await?;
    let record = store.get(train_id).await?;
    // A filter the wheel no longer holds is still a filter the history
    // has runs for, and reading them back is the point of this tool
    // after a wheel change; only a name neither side knows is refused.
    if let Some(filter) = filter {
        if !recorded_filter(record.as_ref(), filter) {
            ctx.check_filter(filter)?;
        }
    }
    let (total, runs) = record.as_ref().map_or((0, Vec::new()), |record| {
        (
            record.run_count(filter),
            record
                .recent_runs(limit, filter)
                .into_iter()
                .cloned()
                .collect(),
        )
    });
    Ok(RunsView {
        train_id: train_id.to_owned(),
        filter: filter.map(str::to_owned),
        total,
        runs,
    })
}

/// Whether the record knows a filter name, as one of the wheel's at
/// write time or as the filter of a run it holds.
fn recorded_filter(record: Option<&FocusRecord>, filter: &str) -> bool {
    record.is_some_and(|record| {
        record
            .filters
            .as_ref()
            .is_some_and(|names| names.iter().any(|name| name == filter))
            || record
                .runs
                .iter()
                .any(|run| run.filter.as_deref() == Some(filter))
    })
}

/// The `set_focus_offsets` body: hand-entered offsets, validated
/// against the wheel.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], the filter
/// errors of [`TrainContext::check_filter`], and the store's.
pub async fn set_focus_offsets(
    rig: &dyn FocusRig,
    store: &FocusStore,
    train_id: &str,
    reference: &str,
    offsets: BTreeMap<String, i32>,
) -> Result<ModelView> {
    let ctx = resolve_train(rig, train_id).await?;
    ctx.check_filter(reference)?;
    for name in offsets.keys() {
        ctx.check_filter(name)?;
    }
    let (record, ()) = store
        .update(train_id, move |held| {
            let stale = stale_fields(held.as_ref(), &ctx);
            let (mut record, _) = record_for_write(held, &stale, &ctx);
            record.set_offsets(Some(reference), offsets);
            Ok::<_, FocusModelError>((record, ()))
        })
        .await?;
    Ok(model_view(train_id, Some(&record), Vec::new()))
}

/// The `reset_focus_model` body: forget the measurements, keep the
/// offsets.
///
/// The reset also adopts the train as it stands now — that is what a
/// re-seated focuser or a swapped camera needs — and reports every
/// identity field it took over, so an operator sees that the offsets
/// it kept were measured on the old one.
///
/// # Errors
///
/// Returns the resolution errors of [`resolve_train`], a
/// [`FocusModelError::Workflow`] for a train with no record, and the
/// store's.
pub async fn reset_focus_model(
    rig: &dyn FocusRig,
    store: &FocusStore,
    train_id: &str,
) -> Result<ResetView> {
    let ctx = resolve_train(rig, train_id).await?;
    let (record, adopted) = store
        .update(train_id, move |held| {
            let mut record = held.ok_or_else(|| {
                FocusModelError::Workflow(format!("train '{train_id}' has no focus model"))
            })?;
            let adopted = stale_fields(Some(&record), &ctx);
            record.reset_measurements();
            record.focuser_id = Some(ctx.focuser_id.clone());
            record.camera_id = Some(ctx.camera_id.clone());
            record.filters.clone_from(&ctx.filters);
            Ok::<_, FocusModelError>((record, adopted))
        })
        .await?;
    Ok(ResetView {
        train_id: train_id.to_owned(),
        dropped: vec![
            "runs".to_owned(),
            "last_good".to_owned(),
            "temperature_coefficient".to_owned(),
        ],
        kept: vec!["reference_filter".to_owned(), "offsets".to_owned()],
        adopted,
        model: model_view(train_id, Some(&record), Vec::new()),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::store::LastGood;

    /// A train the sweep can be configured for without any optics: the
    /// config block sets both sweep numbers.
    const CONFIGURED: &str = r#"{
        "mcp_server_url": "http://127.0.0.1:1/mcp",
        "trains": {
            "main": {
                "duration": "10ms", "step_size": 10, "half_width": 40,
                "min_fit_points": 3, "max_attempts": 1
            }
        }
    }"#;

    fn config(json: &str) -> Config {
        crate::config::parse_config(json, "test").unwrap()
    }

    fn train_info(wheel: bool, focuser: bool, camera: bool) -> TrainInfo {
        TrainInfo {
            camera_id: camera.then(|| "main-cam".to_owned()),
            filter_wheel_id: wheel.then(|| "main-fw".to_owned()),
            filters: wheel.then(|| vec!["Luminance".to_owned(), "Ha".to_owned()]),
            filter_wavelengths_nm: wheel.then(|| {
                [
                    ("Luminance".to_owned(), None),
                    ("Ha".to_owned(), Some(656.0)),
                ]
                .into()
            }),
            terminal_focuser_id: focuser.then(|| "main-focuser".to_owned()),
            optics: Optics::default(),
        }
    }

    fn wheel_filters() -> Vec<String> {
        vec!["Luminance".to_owned(), "Ha".to_owned()]
    }

    async fn temp_store() -> (FocusStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FocusStore::open(dir.path().join("focus.redb"))
            .await
            .unwrap();
        (store, dir)
    }

    /// Where the mock rig's focuser is, shared by the move and measure
    /// expectations so a scripted curve can answer for the position.
    #[derive(Clone)]
    struct Position(Arc<Mutex<i32>>);

    impl Position {
        fn new(at: i32) -> Self {
            Self(Arc::new(Mutex::new(at)))
        }

        fn get(&self) -> i32 {
            *self.0.lock().unwrap()
        }

        fn set(&self, to: i32) {
            *self.0.lock().unwrap() = to;
        }
    }

    /// A rig that resolves the reference train and answers the reads a
    /// sweep starts with, its focuser uncoupled from any guiding train.
    /// The caller adds the sweep's own answers.
    fn rig(position: &Position) -> MockFocusRig {
        rig_with_plan(position, RefocusPlan::default())
    }

    /// The same rig, answering `get_refocus_plan` with `plan`.
    fn rig_with_plan(position: &Position, plan: RefocusPlan) -> MockFocusRig {
        let mut rig = MockFocusRig::new();
        rig.expect_get_refocus_plan().returning(move |_| {
            let plan = plan.clone();
            Box::pin(async move { Ok(plan) })
        });
        base_rig(rig, position)
    }

    /// A rig whose refocus plan `rp` cannot answer.
    fn rig_without_a_plan(position: &Position) -> MockFocusRig {
        let mut rig = MockFocusRig::new();
        rig.expect_get_refocus_plan().returning(|_| {
            Box::pin(async { Err(FocusModelError::ToolCall("rp cannot answer".to_owned())) })
        });
        base_rig(rig, position)
    }

    /// The reads every rig answers. `mockall` takes the first matching
    /// expectation, so a test that pre-registers one of these on the
    /// mock it passes in keeps its own answer.
    fn base_rig(mut rig: MockFocusRig, position: &Position) -> MockFocusRig {
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(true, true, true)) }));
        let at = position.clone();
        rig.expect_get_focuser_position().returning(move |_| {
            let position = at.get();
            Box::pin(async move {
                Ok(FocuserPosition {
                    position,
                    ..FocuserPosition::default()
                })
            })
        });
        rig.expect_get_focuser_temperature()
            .returning(|_| Box::pin(async { Ok(Some(11.0)) }));
        rig.expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Luminance".to_owned())) }));
        rig.expect_is_cancelled().returning(|| false);
        let at = position.clone();
        rig.expect_move_focuser().returning(move |_, to| {
            at.set(to);
            Box::pin(async move { Ok(to) })
        });
        rig.expect_capture().returning(|_, _| {
            Box::pin(async {
                Ok(CaptureResult {
                    document_id: "doc".to_owned(),
                })
            })
        });
        rig
    }

    /// Measure a clean V centred on `vertex`.
    fn measures_a_v(rig: &mut MockFocusRig, position: &Position, vertex: i32) {
        let at = position.clone();
        rig.expect_measure_stars().returning(move |_, _, _, _| {
            let dx = f64::from(at.get() - vertex);
            let hfr = 1.0 + dx * dx / 400.0;
            Box::pin(async move {
                Ok(StarMeasurement {
                    median_hfr: Some(hfr),
                    star_count: 100,
                })
            })
        });
    }

    /// Measure starless frames, as the simulator does.
    fn measures_nothing(rig: &mut MockFocusRig) {
        rig.expect_measure_stars().returning(|_, _, _, _| {
            Box::pin(async {
                Ok(StarMeasurement {
                    median_hfr: None,
                    star_count: 0,
                })
            })
        });
    }

    /// A cleanup rig that expects the put-back to `restore_to` and
    /// answers the read-back that confirms it.
    fn cleanup_rig(position: &Position, restore_to: i32) -> MockFocusRig {
        let mut rig = MockFocusRig::new();
        let at = position.clone();
        rig.expect_move_focuser()
            .withf(move |_, to| *to == restore_to)
            .times(1)
            .returning(move |_, to| {
                at.set(to);
                Box::pin(async move { Ok(to) })
            });
        let at = position.clone();
        rig.expect_get_focuser_position().returning(move |_| {
            let position = at.get();
            Box::pin(async move {
                Ok(FocuserPosition {
                    position,
                    ..FocuserPosition::default()
                })
            })
        });
        rig
    }

    fn params(filter: Option<&str>) -> FocusTrainParams {
        FocusTrainParams {
            train_id: "main".to_owned(),
            filter: filter.map(str::to_owned),
            shared: false,
        }
    }

    // --- resolution -----------------------------------------------------

    #[tokio::test]
    async fn a_train_without_a_focuser_or_a_camera_is_named() {
        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(true, false, true)) }));
        let err = resolve_train(&rig, "main").await.unwrap_err();
        assert_eq!(err.tool_message(), "train 'main' has no terminal focuser");

        let mut rig = MockFocusRig::new();
        rig.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(true, true, false)) }));
        let err = resolve_train(&rig, "main").await.unwrap_err();
        assert_eq!(err.tool_message(), "train 'main' has no camera");
    }

    #[tokio::test]
    async fn a_filter_is_checked_against_the_wheel() {
        let position = Position::new(25_000);
        let rig = rig(&position);
        let ctx = resolve_train(&rig, "main").await.unwrap();
        ctx.check_filter("Ha").unwrap();
        let err = ctx.check_filter("OIII").unwrap_err();
        assert_eq!(
            err.tool_message(),
            "filter 'OIII' is not on train 'main' (wheel 'main-fw' has: Luminance, Ha)"
        );

        let mut bare = MockFocusRig::new();
        bare.expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(false, true, true)) }));
        let ctx = resolve_train(&bare, "main").await.unwrap();
        let err = ctx.check_filter("Ha").unwrap_err();
        assert_eq!(
            err.tool_message(),
            "train 'main' has no filter wheel; do not pass filter"
        );
    }

    // --- focus_train ----------------------------------------------------

    #[tokio::test]
    async fn a_confirmed_sweep_is_recorded_as_the_filters_last_good_focus() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_a_v(&mut active, &position, 25_010);
        let cleanup = MockFocusRig::new();

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap();

        assert!(outcome.confirmed, "{outcome:?}");
        assert_eq!(outcome.position, 25_010);
        assert_eq!(outcome.best_position, 25_010);
        assert_eq!(outcome.filter.as_deref(), Some("Luminance"));
        assert_eq!(outcome.sweep.source, crate::sizing::SweepSource::Configured);
        assert_eq!(outcome.model, "fresh");
        assert!(outcome.recorded.last_good_updated);
        assert_eq!(outcome.recorded.runs, 1);
        assert!(!outcome.guiding_paused);
        assert_eq!(
            outcome.prediction.start, None,
            "an empty record predicts nothing"
        );

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(
            record.last_good_for(Some("Luminance")).unwrap().position,
            25_010
        );
        assert_eq!(record.runs[0].outcome, RunOutcome::Confirmed);
        assert_eq!(record.runs[0].curve_points.len(), 9);
        assert_eq!(record.runs[0].temperature_c, Some(11.0));
    }

    #[tokio::test]
    async fn a_remembered_focus_moves_the_focuser_before_the_sweep() {
        let (store, _dir) = temp_store().await;
        let mut seeded = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        seeded.set_last_good(LastGood {
            filter: Some("Luminance".to_owned()),
            position: 25_100,
            temperature_c: Some(11.0),
            hfr: 1.0,
            at: "2026-09-10T20:00:00Z".to_owned(),
        });
        store.put(seeded).await.unwrap();

        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_a_v(&mut active, &position, 25_100);
        let cleanup = MockFocusRig::new();

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap();

        assert_eq!(outcome.prediction.start, Some(25_100));
        assert!(outcome.prediction.moved);
        assert_eq!(outcome.prediction.terms.last_good, Some(25_100));
        assert_eq!(
            outcome.prediction.missing,
            ["temperature_coefficient"],
            "no coefficient is recorded yet"
        );
        assert_eq!(outcome.position, 25_100);
    }

    #[tokio::test]
    async fn a_failed_sweep_puts_the_focuser_back_and_records_the_failure() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_nothing(&mut active);
        let cleanup = cleanup_rig(&position, 25_000);

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();

        let message = err.tool_message();
        assert!(message.contains("not enough stars"), "{message}");
        assert!(message.contains("attempts: 1"), "{message}");
        assert!(message.contains("curve_points"), "{message}");
        assert!(message.contains("prediction"), "{message}");
        assert_eq!(
            position.get(),
            25_000,
            "the focuser is back where it started"
        );

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.runs[0].outcome, RunOutcome::NotEnoughStars);
        assert_eq!(record.runs[0].curve_points.len(), 9);
        assert!(record.last_good.is_empty(), "a failure teaches no position");
    }

    #[tokio::test]
    async fn a_cancelled_sweep_puts_the_focuser_back_and_records_the_cancellation() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = MockFocusRig::new();
        active
            .expect_get_train_info()
            .returning(|_| Box::pin(async { Ok(train_info(true, true, true)) }));
        active
            .expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        active.expect_get_focuser_position().returning(|_| {
            Box::pin(async {
                Ok(FocuserPosition {
                    position: 25_000,
                    ..FocuserPosition::default()
                })
            })
        });
        active
            .expect_get_focuser_temperature()
            .returning(|_| Box::pin(async { Ok(None) }));
        active
            .expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Luminance".to_owned())) }));
        // The caller goes away before the first move.
        active.expect_is_cancelled().returning(|| true);
        active.expect_move_focuser().times(0);
        let mut cleanup = cleanup_rig(&position, 25_000);

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("cancelled"), "{err}");

        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.runs[0].outcome, RunOutcome::Cancelled);
        cleanup.checkpoint();
    }

    #[tokio::test]
    async fn a_named_filter_moves_the_wheel_and_names_the_run() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_a_v(&mut active, &position, 25_010);
        active
            .expect_set_filter()
            .withf(|wheel, filter| wheel == "main-fw" && filter == "Ha")
            .times(1)
            .returning(|_, _| Box::pin(async { Ok(()) }));
        let cleanup = MockFocusRig::new();

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(Some("Ha")),
            &NoProgress,
        )
        .await
        .unwrap();

        assert_eq!(outcome.filter.as_deref(), Some("Ha"));
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.runs[0].filter.as_deref(), Some("Ha"));
        active.checkpoint();
    }

    #[tokio::test]
    async fn a_stale_record_predicts_nothing_and_the_run_resets_it() {
        let (store, _dir) = temp_store().await;
        let mut seeded = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("retired-cam"),
            Some(wheel_filters()),
        );
        seeded.set_last_good(LastGood {
            filter: Some("Luminance".to_owned()),
            position: 25_100,
            temperature_c: Some(11.0),
            hfr: 1.0,
            at: "2026-09-10T20:00:00Z".to_owned(),
        });
        store.put(seeded).await.unwrap();

        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_a_v(&mut active, &position, 25_010);
        let cleanup = MockFocusRig::new();

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap();

        assert_eq!(outcome.prediction.start, None);
        assert_eq!(
            outcome.model,
            "reset: camera_id changed from retired-cam to main-cam"
        );
        let record = store.get("main").await.unwrap().unwrap();
        assert_eq!(record.camera_id.as_deref(), Some("main-cam"));
        assert_eq!(
            record.runs.len(),
            1,
            "the reset record holds this run alone"
        );
    }

    // --- guiding --------------------------------------------------------

    #[tokio::test]
    async fn a_guide_coupled_sweep_pauses_and_resumes_around_the_walk() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: Vec::new(),
            },
        );
        measures_a_v(&mut active, &position, 25_010);
        active
            .expect_guiding_active()
            .returning(|| Box::pin(async { Ok(true) }));
        active
            .expect_pause_guiding()
            .times(1)
            .returning(|| Box::pin(async { Ok(()) }));
        active.expect_resume_guiding().times(0);
        let mut cleanup = MockFocusRig::new();
        cleanup
            .expect_resume_guiding()
            .times(1)
            .returning(|| Box::pin(async { Ok(()) }));

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap();
        assert!(outcome.guiding_paused);
        active.checkpoint();
        cleanup.checkpoint();
    }

    #[tokio::test]
    async fn the_handshake_is_skipped_when_the_guider_is_idle_or_unreachable() {
        for (label, reachable) in [("idle", true), ("unreachable", false)] {
            let (store, _dir) = temp_store().await;
            let position = Position::new(25_000);
            let mut active = rig_with_plan(
                &position,
                RefocusPlan {
                    guide_coupled: true,
                    steps: Vec::new(),
                },
            );
            measures_a_v(&mut active, &position, 25_010);
            active.expect_guiding_active().returning(move || {
                Box::pin(async move {
                    if reachable {
                        Ok(false)
                    } else {
                        Err(FocusModelError::ToolCall("no guider".to_owned()))
                    }
                })
            });
            active.expect_pause_guiding().times(0);
            active.expect_resume_guiding().times(0);
            let cleanup = MockFocusRig::new();

            let outcome = focus_train(
                Rig {
                    active: &active,
                    cleanup: &cleanup,
                },
                &store,
                &config(CONFIGURED),
                &params(None),
                &NoProgress,
            )
            .await
            .unwrap();
            assert!(!outcome.guiding_paused, "{label}");
            active.checkpoint();
        }
    }

    // --- the shared walk ------------------------------------------------

    #[tokio::test]
    async fn a_shared_walk_runs_every_capture_step_and_reports_them() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: false,
                steps: vec![PlanStep {
                    focuser_id: "main-focuser".to_owned(),
                    run_train_id: "main".to_owned(),
                    camera_id: Some("main-cam".to_owned()),
                    metric: "capture".to_owned(),
                }],
            },
        );
        measures_a_v(&mut active, &position, 25_010);
        let cleanup = MockFocusRig::new();

        let outcome = focus_shared(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &FocusTrainParams {
                train_id: "main".to_owned(),
                filter: None,
                shared: true,
            },
            &NoProgress,
        )
        .await
        .unwrap();

        let steps = outcome.steps.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].focuser_id, "main-focuser");
        assert_eq!(steps[0].position, Some(25_010));
        assert_eq!(steps[0].confirmed, Some(true));
    }

    #[tokio::test]
    async fn a_shared_walk_stops_at_the_failing_step() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: false,
                steps: vec![
                    PlanStep {
                        focuser_id: "main-focuser".to_owned(),
                        run_train_id: "main".to_owned(),
                        camera_id: Some("main-cam".to_owned()),
                        metric: "capture".to_owned(),
                    },
                    PlanStep {
                        focuser_id: "guide-focuser".to_owned(),
                        run_train_id: "guide".to_owned(),
                        camera_id: None,
                        metric: "guide".to_owned(),
                    },
                ],
            },
        );
        measures_nothing(&mut active);
        // The guide step is never reached.
        active.expect_auto_focus_guide_train().times(0);
        let cleanup = cleanup_rig(&position, 25_000);

        let err = focus_shared(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &FocusTrainParams {
                train_id: "main".to_owned(),
                filter: None,
                shared: true,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("not enough stars"), "{err}");
        active.checkpoint();
    }

    // --- the reads and the store writes ---------------------------------

    #[tokio::test]
    async fn the_model_reads_empty_then_fresh_then_stale() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);

        let view = get_focus_model(&rig, &store, "main").await.unwrap();
        assert_eq!(view.model, "empty");
        assert_eq!(view.runs_recorded, 0);
        assert!(view.last_run.is_none());

        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(vec!["Luminance".to_owned(), "Ha".to_owned()]),
        );
        record.set_offsets(Some("Luminance"), [("Ha".to_owned(), 46)].into());
        store.put(record.clone()).await.unwrap();
        let view = get_focus_model(&rig, &store, "main").await.unwrap();
        assert_eq!(view.model, "fresh");
        assert_eq!(view.offsets.get("Ha"), Some(&46));

        record.focuser_id = Some("other-focuser".to_owned());
        store.put(record).await.unwrap();
        let view = get_focus_model(&rig, &store, "main").await.unwrap();
        assert_eq!(
            view.model,
            "stale: focuser_id changed from other-focuser to main-focuser"
        );
        assert_eq!(view.stale.len(), 1);
    }

    #[tokio::test]
    async fn the_offsets_are_validated_before_they_are_written() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);

        let err = set_focus_offsets(
            &rig,
            &store,
            "main",
            "Luminance",
            [("OIII".to_owned(), 12)].into(),
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("filter 'OIII'"), "{err}");
        assert!(
            store.get("main").await.unwrap().is_none(),
            "a refused call writes nothing"
        );

        let view = set_focus_offsets(
            &rig,
            &store,
            "main",
            "Luminance",
            [("Ha".to_owned(), 46)].into(),
        )
        .await
        .unwrap();
        assert_eq!(view.reference_filter.as_deref(), Some("Luminance"));
        assert_eq!(view.offsets.get("Ha"), Some(&46));
        assert_eq!(view.offsets.get("Luminance"), Some(&0));
    }

    #[tokio::test]
    async fn a_reset_needs_a_record_and_keeps_the_offsets() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);

        let err = reset_focus_model(&rig, &store, "main").await.unwrap_err();
        assert_eq!(err.tool_message(), "train 'main' has no focus model");

        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        record.set_offsets(Some("Luminance"), [("Ha".to_owned(), 46)].into());
        record.set_temperature_coefficient(Some(-7.4), 6, 4.5);
        record.set_last_good(LastGood {
            filter: None,
            position: 25_100,
            temperature_c: None,
            hfr: 1.0,
            at: "2026-09-10T20:00:00Z".to_owned(),
        });
        store.put(record).await.unwrap();

        let view = reset_focus_model(&rig, &store, "main").await.unwrap();
        assert_eq!(view.dropped.len(), 3);
        assert_eq!(view.kept, ["reference_filter", "offsets"]);
        assert!(view.model.last_good.is_empty());
        assert_eq!(view.model.temperature_coefficient, None);
        assert_eq!(view.model.offsets.get("Ha"), Some(&46));
    }

    #[tokio::test]
    async fn the_runs_read_back_newest_first_and_a_zero_limit_is_refused() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);

        let err = get_focus_runs(&rig, &store, "main", 0, None)
            .await
            .unwrap_err();
        assert_eq!(err.tool_message(), "limit must be at least 1");

        let view = get_focus_runs(&rig, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(view.total, 0, "a train with no record has no runs");

        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        for (at, filter, position) in [
            ("2026-09-08T22:00:00Z", "Luminance", 1),
            ("2026-09-09T22:00:00Z", "Ha", 2),
            ("2026-09-10T22:00:00Z", "Luminance", 3),
        ] {
            let mut run = FocusRun::new(
                at.to_owned(),
                Some(filter.to_owned()),
                RunOutcome::Confirmed,
                10,
                40,
                crate::sizing::SweepSource::Configured,
            );
            run.position = Some(position);
            run.hfr = Some(1.0);
            record.push_run(run, 50);
        }
        store.put(record).await.unwrap();

        let view = get_focus_runs(&rig, &store, "main", 2, None).await.unwrap();
        assert_eq!(view.total, 3);
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0].position, Some(3));

        let view = get_focus_runs(&rig, &store, "main", 20, Some("Ha"))
            .await
            .unwrap();
        assert_eq!(view.total, 1);
        assert_eq!(view.runs[0].position, Some(2));
    }

    #[tokio::test]
    async fn the_sweep_plan_reports_the_configured_override_and_the_measured_slope() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);

        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        let mut run = FocusRun::new(
            "2026-09-10T22:00:00Z".to_owned(),
            Some("Luminance".to_owned()),
            RunOutcome::Confirmed,
            10,
            40,
            crate::sizing::SweepSource::Configured,
        );
        run.wing_slope = Some(3.9);
        record.push_run(run, 50);
        store.put(record).await.unwrap();

        let view = get_sweep_plan(&rig, &store, &config(CONFIGURED), "main", None)
            .await
            .unwrap();
        assert_eq!(view.plan.step_size, 10);
        assert_eq!(view.plan.half_width, 40);
        assert_eq!(view.measured_slope, Some(3.9));
        assert_eq!(view.configured.unwrap().step_size, Some(10));
    }

    #[tokio::test]
    async fn a_train_without_optics_or_a_configured_sweep_names_the_missing_fact() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);
        let bare = config(r#"{ "mcp_server_url": "http://127.0.0.1:1/mcp" }"#);

        let err = get_sweep_plan(&rig, &store, &bare, "main", None)
            .await
            .unwrap_err();
        assert!(
            err.tool_message().contains("focal_length_mm is unknown"),
            "{err}"
        );
        assert!(
            err.tool_message().contains("trains.main.step_size"),
            "{err}"
        );
    }

    // --- the failure paths the record must not lose ---------------------

    /// The coupling is the only thing that says whether the sweep will
    /// move a focuser the guiding train shares, so a plan `rp` cannot
    /// answer fails the call instead of sweeping uncoupled.
    #[tokio::test]
    async fn a_plan_that_cannot_be_read_fails_the_call_before_anything_moves() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_without_a_plan(&position);
        active.expect_measure_stars().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("rp cannot answer"), "{err}");
        assert_eq!(position.get(), 25_000, "nothing moved");
        active.checkpoint();
    }

    /// A focuser that refuses the predicted start is a failed run like
    /// any other: put back, recorded, readable the morning after.
    #[tokio::test]
    async fn a_move_that_fails_before_the_sweep_is_recorded_as_a_failed_run() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        record.set_last_good(LastGood {
            filter: Some("Luminance".to_owned()),
            position: 25_200,
            temperature_c: Some(11.0),
            hfr: 1.1,
            at: "2026-09-10T22:00:00Z".to_owned(),
        });
        store.put(record).await.unwrap();

        let mut active = MockFocusRig::new();
        active.expect_move_focuser().returning(|_, _| {
            Box::pin(async { Err(FocusModelError::ToolCall("focuser jammed".to_owned())) })
        });
        let mut active = base_rig(active, &position);
        active
            .expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        active.expect_measure_stars().times(0);
        let cleanup = cleanup_rig(&position, 25_000);

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("focuser jammed"), "{err}");

        let runs = get_focus_runs(&active, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.total, 1, "the failed call is in the history");
        assert_eq!(runs.runs[0].outcome, RunOutcome::Error);
        assert_eq!(
            runs.runs[0].error.as_deref(),
            Some("focuser jammed"),
            "the run names what failed"
        );
        active.checkpoint();
    }

    /// The sweep found focus; only the guider did not come back. The
    /// run is written before the error surfaces, and the resume runs
    /// on the client a cancellation cannot reach.
    #[tokio::test]
    async fn a_resume_that_fails_after_a_good_sweep_still_records_the_run() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: Vec::new(),
            },
        );
        measures_a_v(&mut active, &position, 25_010);
        active
            .expect_guiding_active()
            .returning(|| Box::pin(async { Ok(true) }));
        active
            .expect_pause_guiding()
            .times(1)
            .returning(|| Box::pin(async { Ok(()) }));
        active.expect_resume_guiding().times(0);
        let mut cleanup = MockFocusRig::new();
        cleanup.expect_resume_guiding().times(1).returning(|| {
            Box::pin(async { Err(FocusModelError::ToolCall("phd2 is gone".to_owned())) })
        });

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(
            err.tool_message().contains("guiding could not be resumed"),
            "{err}"
        );

        let runs = get_focus_runs(&active, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.total, 1, "the sweep that worked is recorded");
        assert_eq!(runs.runs[0].position, Some(25_010));
        assert!(runs.runs[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("phd2 is gone")));
        active.checkpoint();
        cleanup.checkpoint();
    }

    /// One pause covers every capture step of a shared walk, and is
    /// released before the guide step, which needs the loop running.
    #[tokio::test]
    async fn a_shared_walk_pauses_once_and_releases_before_the_guide_step() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: vec![
                    PlanStep {
                        focuser_id: "shared-focuser".to_owned(),
                        run_train_id: "main".to_owned(),
                        camera_id: Some("main-cam".to_owned()),
                        metric: "capture".to_owned(),
                    },
                    PlanStep {
                        focuser_id: "main-focuser".to_owned(),
                        run_train_id: "main".to_owned(),
                        camera_id: Some("main-cam".to_owned()),
                        metric: "capture".to_owned(),
                    },
                    PlanStep {
                        focuser_id: "guide-focuser".to_owned(),
                        run_train_id: "guide".to_owned(),
                        camera_id: None,
                        metric: "guide".to_owned(),
                    },
                ],
            },
        );
        measures_a_v(&mut active, &position, 25_010);
        active
            .expect_guiding_active()
            .returning(|| Box::pin(async { Ok(true) }));
        active
            .expect_pause_guiding()
            .times(1)
            .returning(|| Box::pin(async { Ok(()) }));
        active.expect_resume_guiding().times(0);
        active
            .expect_auto_focus_guide_train()
            .times(1)
            .returning(|_| {
                Box::pin(async {
                    Ok(GuideFocusResult {
                        final_position: Some(9_000),
                        final_hfd: Some(2.2),
                        confirmed: Some(true),
                    })
                })
            });
        let mut cleanup = MockFocusRig::new();
        cleanup
            .expect_resume_guiding()
            .times(1)
            .returning(|| Box::pin(async { Ok(()) }));

        let outcome = focus_shared(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &FocusTrainParams {
                train_id: "main".to_owned(),
                filter: None,
                shared: true,
            },
            &NoProgress,
        )
        .await
        .unwrap();

        let steps = outcome.steps.unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2].metric, "guide");
        assert_eq!(steps[2].position, Some(9_000));
        assert!(outcome.guiding_paused, "the walk held the pause");
        active.checkpoint();
        cleanup.checkpoint();
    }

    /// A plan that is the guiding metric sweep alone has nowhere to
    /// report a result, so it is refused before anything actuates.
    #[tokio::test]
    async fn a_guide_only_plan_is_refused_before_the_guide_step_runs() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: false,
                steps: vec![PlanStep {
                    focuser_id: "guide-focuser".to_owned(),
                    run_train_id: "guide".to_owned(),
                    camera_id: None,
                    metric: "guide".to_owned(),
                }],
            },
        );
        active.expect_auto_focus_guide_train().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_shared(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &FocusTrainParams {
                train_id: "guide".to_owned(),
                filter: None,
                shared: true,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(
            err.tool_message().contains("has no capture step to focus"),
            "{err}"
        );
        active.checkpoint();
    }

    /// `get_sweep_plan` previews the sweep `focus_train` would run, so
    /// with no filter argument it sizes for the one in the path.
    #[tokio::test]
    async fn the_sweep_plan_sizes_for_the_filter_in_the_path() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut rig = MockFocusRig::new();
        rig.expect_get_filter()
            .returning(|_| Box::pin(async { Ok(Some("Ha".to_owned())) }));
        rig.expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        let rig = base_rig(rig, &position);

        let view = get_sweep_plan(&rig, &store, &config(CONFIGURED), "main", None)
            .await
            .unwrap();
        assert_eq!(view.filter.as_deref(), Some("Ha"));
        assert_eq!(
            view.plan.wavelength_nm, 656.0,
            "the wheel's filter, not the 550 nm default"
        );
    }

    /// The reset adopts the train as it stands and says which fields
    /// it took over, because the offsets it keeps were measured on the
    /// old one.
    #[tokio::test]
    async fn a_reset_reports_the_identity_it_adopted() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("old-cam"),
            Some(wheel_filters()),
        );
        record.set_offsets(Some("Luminance"), [("Ha".to_owned(), 40)].into());
        store.put(record).await.unwrap();

        let view = reset_focus_model(&rig, &store, "main").await.unwrap();
        assert_eq!(
            view.adopted,
            vec!["camera_id changed from old-cam to main-cam".to_owned()]
        );
        assert_eq!(view.model.offsets.get("Ha"), Some(&40), "the offsets stay");
        assert_eq!(view.model.model, "fresh");
    }

    /// A store that cannot take a run does not turn a focused train
    /// into a failed call: the focuser is at focus, and the result
    /// says the record is not.
    #[test]
    fn a_run_that_could_not_be_written_is_reported_not_raised() {
        let (recorded, model) = recorded_or_not(
            Err(FocusModelError::Workflow("the disk is full".to_owned())),
            "main",
        );
        assert_eq!(model, "unrecorded");
        assert_eq!(recorded.runs, 0);
        assert!(!recorded.last_good_updated);
        assert_eq!(recorded.error.as_deref(), Some("the disk is full"));
    }

    /// A grid that cannot be walked is an error before anything moves,
    /// which means before the move to the predicted start, not after
    /// it inside the sweep.
    #[tokio::test]
    async fn an_unwalkable_grid_is_refused_before_the_predicted_move() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        record.set_last_good(LastGood {
            filter: Some("Luminance".to_owned()),
            position: 25_400,
            temperature_c: Some(11.0),
            hfr: 1.1,
            at: "2026-09-10T22:00:00Z".to_owned(),
        });
        store.put(record).await.unwrap();

        // step_size 1 over a half width of 4000 is 8001 positions.
        let wide = config(
            r#"{
                "mcp_server_url": "http://127.0.0.1:1/mcp",
                "trains": { "main": {
                    "duration": "10ms", "step_size": 1, "half_width": 4000,
                    "min_fit_points": 3, "max_attempts": 1
                } }
            }"#,
        );
        let mut active = rig(&position);
        active.expect_measure_stars().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &wide,
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("more than the cap"), "{err}");
        assert_eq!(position.get(), 25_000, "the predicted move never happened");
        active.checkpoint();
    }

    /// A focuser parked past a bound cannot be put back there, so the
    /// call is refused before it sweeps into range and strands it.
    #[tokio::test]
    async fn a_focuser_outside_its_travel_is_refused_before_it_moves() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(61_000);
        let mut active = MockFocusRig::new();
        let at = position.clone();
        active.expect_get_focuser_position().returning(move |_| {
            let position = at.get();
            Box::pin(async move {
                Ok(FocuserPosition {
                    position,
                    min_position: Some(0),
                    max_position: Some(60_000),
                    backlash: None,
                })
            })
        });
        let mut active = base_rig(active, &position);
        active
            .expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        active.expect_measure_stars().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(
            err.tool_message()
                .contains("outside its configured travel [0, 60000]"),
            "{err}"
        );
        assert_eq!(position.get(), 61_000, "nothing moved");

        let runs = get_focus_runs(&active, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.total, 1, "the refusal is in the history");
        active.checkpoint();
    }

    /// A caller who gives up while the guider is being read has
    /// cancelled the call, not left an unreadable guider behind: the
    /// handshake ends the call rather than sweeping on.
    #[tokio::test]
    async fn a_cancellation_during_the_guider_read_stops_the_call() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: Vec::new(),
            },
        );
        active.expect_guiding_active().returning(|| {
            Box::pin(async {
                Err(FocusModelError::Cancelled(
                    "the caller cancelled the focus run".to_owned(),
                ))
            })
        });
        active.expect_pause_guiding().times(0);
        active.expect_measure_stars().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("cancelled"), "{err}");
        assert_eq!(position.get(), 25_000, "nothing moved");

        let runs = get_focus_runs(&active, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.runs[0].outcome, RunOutcome::Cancelled);
        active.checkpoint();
    }

    /// A guider that will not pause stops the call before anything
    /// moves — and the attempt is still in the history, because that
    /// is where the morning after looks for why the train never
    /// focused.
    #[tokio::test]
    async fn a_guider_that_will_not_pause_is_recorded_as_a_failed_run() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: Vec::new(),
            },
        );
        active
            .expect_guiding_active()
            .returning(|| Box::pin(async { Ok(true) }));
        active.expect_pause_guiding().returning(|| {
            Box::pin(async { Err(FocusModelError::ToolCall("phd2 will not pause".to_owned())) })
        });
        active.expect_measure_stars().times(0);
        let cleanup = MockFocusRig::new();

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("phd2 will not pause"), "{err}");
        assert_eq!(position.get(), 25_000, "nothing moved");

        let runs = get_focus_runs(&active, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.total, 1);
        assert_eq!(runs.runs[0].outcome, RunOutcome::Error);
        assert_eq!(
            runs.runs[0].error.as_deref(),
            Some("phd2 will not pause"),
            "the run names what refused"
        );
        active.checkpoint();
    }

    /// A move `rp` abandoned for the cancellation can carry the
    /// focuser on after the put-back has landed, so the put-back reads
    /// back and goes again rather than reporting a position it never
    /// confirmed.
    #[tokio::test]
    async fn a_put_back_the_focuser_drifts_out_of_is_repeated() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig(&position);
        measures_nothing(&mut active);

        let mut cleanup = MockFocusRig::new();
        let at = position.clone();
        let moves = Arc::new(Mutex::new(0_u32));
        let counted = Arc::clone(&moves);
        cleanup.expect_move_focuser().returning(move |_, to| {
            let mut count = counted.lock().unwrap();
            *count += 1;
            // The abandoned sweep move lands after the first put-back.
            at.set(if *count == 1 { 24_991 } else { to });
            Box::pin(async move { Ok(to) })
        });
        let at = position.clone();
        cleanup.expect_get_focuser_position().returning(move |_| {
            let position = at.get();
            Box::pin(async move {
                Ok(FocuserPosition {
                    position,
                    ..FocuserPosition::default()
                })
            })
        });

        let err = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(err.tool_message().contains("not enough stars"), "{err}");
        assert!(
            !err.tool_message().contains("could not be restored"),
            "the second move put it back: {err}"
        );
        assert_eq!(position.get(), 25_000);
        assert_eq!(*moves.lock().unwrap(), 2, "one retry, not a loop");
    }

    /// A focuser whose probe hiccups still focuses: the temperature is
    /// a term of the prediction, not a precondition of the sweep.
    #[tokio::test]
    async fn a_temperature_read_that_fails_is_no_reading_not_a_failed_call() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = MockFocusRig::new();
        active.expect_get_focuser_temperature().returning(|_| {
            Box::pin(async { Err(FocusModelError::ToolCall("thermistor busy".to_owned())) })
        });
        active
            .expect_get_refocus_plan()
            .returning(|_| Box::pin(async { Ok(RefocusPlan::default()) }));
        let mut active = base_rig(active, &position);
        measures_a_v(&mut active, &position, 25_010);
        let cleanup = MockFocusRig::new();

        let outcome = focus_train(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &params(None),
            &NoProgress,
        )
        .await
        .unwrap();
        assert_eq!(outcome.temperature_c, None);
        assert_eq!(outcome.position, 25_010, "the sweep ran anyway");
    }

    /// After a wheel change the old filter's runs are exactly what an
    /// operator wants to read, so the history takes a name the record
    /// knows even when the wheel no longer does.
    #[tokio::test]
    async fn the_history_answers_for_a_filter_the_wheel_has_lost() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(vec!["Luminance".to_owned(), "OIII".to_owned()]),
        );
        record.push_run(
            FocusRun::new(
                "2026-09-10T22:00:00Z".to_owned(),
                Some("OIII".to_owned()),
                RunOutcome::Confirmed,
                10,
                40,
                crate::sizing::SweepSource::Configured,
            ),
            50,
        );
        store.put(record).await.unwrap();

        let view = get_focus_runs(&rig, &store, "main", 20, Some("OIII"))
            .await
            .unwrap();
        assert_eq!(view.total, 1, "the wheel has Luminance and Ha now");

        let err = get_focus_runs(&rig, &store, "main", 20, Some("SII"))
            .await
            .unwrap_err();
        assert!(
            err.tool_message().contains("is not on train 'main'"),
            "a name neither side knows is still refused: {err}"
        );
    }

    /// A resume that fails before the guide step leaves the walk's
    /// pause held, so the way out tries once more.
    #[tokio::test]
    async fn a_failed_resume_is_retried_on_the_way_out() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let mut active = rig_with_plan(
            &position,
            RefocusPlan {
                guide_coupled: true,
                steps: vec![
                    PlanStep {
                        focuser_id: "main-focuser".to_owned(),
                        run_train_id: "main".to_owned(),
                        camera_id: Some("main-cam".to_owned()),
                        metric: "capture".to_owned(),
                    },
                    PlanStep {
                        focuser_id: "guide-focuser".to_owned(),
                        run_train_id: "guide".to_owned(),
                        camera_id: None,
                        metric: "guide".to_owned(),
                    },
                ],
            },
        );
        measures_a_v(&mut active, &position, 25_010);
        active
            .expect_guiding_active()
            .returning(|| Box::pin(async { Ok(true) }));
        active
            .expect_pause_guiding()
            .returning(|| Box::pin(async { Ok(()) }));
        active.expect_auto_focus_guide_train().times(0);
        let mut cleanup = MockFocusRig::new();
        let attempts = Arc::new(Mutex::new(0_u32));
        let seen = Arc::clone(&attempts);
        cleanup.expect_resume_guiding().times(2).returning(move || {
            let mut count = seen.lock().unwrap();
            *count += 1;
            let first = *count == 1;
            Box::pin(async move {
                if first {
                    Err(FocusModelError::ToolCall("phd2 said no".to_owned()))
                } else {
                    Ok(())
                }
            })
        });

        let err = focus_shared(
            Rig {
                active: &active,
                cleanup: &cleanup,
            },
            &store,
            &config(CONFIGURED),
            &FocusTrainParams {
                train_id: "main".to_owned(),
                filter: None,
                shared: true,
            },
            &NoProgress,
        )
        .await
        .unwrap_err();
        assert!(
            err.tool_message().contains("guiding could not be resumed"),
            "{err}"
        );
        assert_eq!(*attempts.lock().unwrap(), 2, "the way out tried again");
        active.checkpoint();
        cleanup.checkpoint();
    }

    /// The model read answers with the model: the last run is a
    /// summary, and its samples are `get_focus_runs`.
    #[tokio::test]
    async fn the_model_reports_the_last_run_without_its_samples() {
        let (store, _dir) = temp_store().await;
        let position = Position::new(25_000);
        let rig = rig(&position);
        let mut record = FocusRecord::new(
            "main",
            Some("main-focuser"),
            Some("main-cam"),
            Some(wheel_filters()),
        );
        let mut run = FocusRun::new(
            "2026-09-10T22:00:00Z".to_owned(),
            Some("Luminance".to_owned()),
            RunOutcome::Confirmed,
            10,
            40,
            crate::sizing::SweepSource::Configured,
        );
        run.curve_points = vec![
            crate::sweep::CurvePoint {
                position: 24_990,
                hfr: Some(2.0),
                star_count: 100,
                document_id: "a".to_owned(),
                rejected: None,
            },
            crate::sweep::CurvePoint {
                position: 25_010,
                hfr: Some(1.0),
                star_count: 100,
                document_id: "b".to_owned(),
                rejected: None,
            },
        ];
        record.push_run(run, 50);
        store.put(record).await.unwrap();

        let view = get_focus_model(&rig, &store, "main").await.unwrap();
        let last = view.last_run.unwrap();
        assert_eq!(last.curve_points_recorded, 2);
        assert_eq!(last.outcome, RunOutcome::Confirmed);

        let runs = get_focus_runs(&rig, &store, "main", 20, None)
            .await
            .unwrap();
        assert_eq!(runs.runs[0].curve_points.len(), 2, "the samples live here");
    }
}
