//! Auto-focus tool category: `auto_focus`, the guiding train's
//! PHD2-metric V-curve sweep (rp.md § `auto_focus` Contract).
//!
//! An imaging train has no sweep in rp — the focus-model provider's
//! `focus_train` focuses it through rp's primitives.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{tool_error, tool_success};
use crate::config::TrainPurpose;
use crate::events::EventEnvelope;
use crate::imaging;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("required" = ["train_id"]))]
pub struct AutoFocusToolParams {
    /// The guiding train's id: the sweep moves its terminal focuser and
    /// reads PHD2's per-frame HFD, so it requires an active guide loop
    /// and never captures through the guide camera. An imaging train is
    /// refused — the focus-model provider's `focus_train` focuses it.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Focuser steps between sweep grid points (positive integer).
    /// Falls back to the train's `auto_focus` config block.
    #[serde(default)]
    pub step_size: Option<i32>,
    /// Half-width of the sweep around the current focuser position
    /// (positive integer). Falls back to the train's `auto_focus`
    /// config block.
    #[serde(default)]
    pub half_width: Option<i32>,
    /// Minimum number of valid HFD samples for the fit. Default 5.
    #[serde(default)]
    pub min_fit_points: Option<usize>,
    /// How much worse than the lowest valid sweep sample the
    /// confirmation sample may measure before the focuser falls back to
    /// that sample's position. Default 0.25.
    #[serde(default)]
    pub confirmation_tolerance: Option<f64>,
}

#[tool_router(router = tool_router_auto_focus, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "The guiding train's V-curve auto-focus: sweep ± half_width around its terminal focuser's position, sample each position with the median HFD of fresh PHD2 guide frames (requires active guiding; never captures through the guide camera), fit the hyperbola, move the focuser to the fitted minimum, and confirm it with one more sample set — falling back to the lowest sweep sample when the confirmation measures worse than confirmation_tolerance allows. One attempt. Sweep parameters fall back to the guiding train's auto_focus config block. Refuses an imaging train, which the focus-model provider's focus_train focuses, and a focuser an imaging train holds, which only the imaging camera focuses."
    )]
    pub(crate) async fn auto_focus(
        &self,
        Parameters(params): Parameters<AutoFocusToolParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let progress_sink = ProgressSink::from_request_context(&ctx);
        let cancel = Cancel::from_context(&ctx);
        self.auto_focus_inner(params, progress_sink, cancel).await
    }

    /// Body of the `auto_focus` MCP tool, split out so unit tests can
    /// pass `None` for the progress sink without constructing a real
    /// rmcp `Peer` (its constructor is `pub(crate)` in rmcp 1.7). The
    /// parameter rules of the contract, then the metric sweep.
    pub(crate) async fn auto_focus_inner(
        &self,
        params: AutoFocusToolParams,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(train_id) = params.train_id.as_deref() else {
            return Ok(tool_error!("missing required parameter: train_id"));
        };
        let Some(train) = self.trains.train(train_id) else {
            return Ok(tool_error!("train not found: {}", train_id));
        };
        if train.purpose != TrainPurpose::Guiding {
            return Ok(tool_error!(
                "auto_focus: train '{}' is an imaging train, which the focus-model provider's \
                 focus_train focuses; auto_focus sweeps only the guiding train",
                train_id
            ));
        }
        if let Some(tolerance) = params.confirmation_tolerance {
            if !imaging::tools::auto_focus::valid_confirmation_tolerance(tolerance) {
                return Ok(tool_error!(
                    "auto_focus: confirmation_tolerance must be a finite number of at least 0 \
                     (got {})",
                    tolerance
                ));
            }
        }
        let Some(focuser_id) = train.terminal_focuser().map(str::to_string) else {
            return Ok(tool_error!("train '{}' has no focuser", train_id));
        };
        // A focuser an imaging train holds is focused through the
        // imaging camera alone, the guide camera taken as parfocal
        // (rp.md § `auto_focus` Contract): PHD2's HFD must not move it.
        if let Some(imaging) = self.trains.imaging_train_focusing(&focuser_id) {
            return Ok(tool_error!(
                "guide-train auto_focus: focuser '{}' is in imaging train '{}', which \
                 focuses it; the guide camera is taken as parfocal with that train's camera",
                focuser_id,
                imaging.id
            ));
        }
        let block = train.auto_focus.as_ref();
        let Some(step_size) = params
            .step_size
            .or_else(|| block.map(|b| b.step_size.value()))
        else {
            return Ok(tool_error!("missing required parameter: step_size"));
        };
        let Some(half_width) = params
            .half_width
            .or_else(|| block.map(|b| b.half_width.value()))
        else {
            return Ok(tool_error!("missing required parameter: half_width"));
        };
        let sweep = GuideSweepParams {
            step_size,
            half_width,
            frames_per_step: block
                .and_then(|b| {
                    b.frames_per_step
                        .map(crate::config::optical_train::FramesPerStep::value)
                })
                .unwrap_or(3),
            min_fit_points: params
                .min_fit_points
                .or_else(|| block.and_then(|b| b.min_fit_points))
                .unwrap_or(5),
            confirmation_tolerance: params
                .confirmation_tolerance
                .or_else(|| {
                    block.and_then(|b| {
                        b.confirmation_tolerance
                            .map(crate::config::optical_train::ConfirmationTolerance::value)
                    })
                })
                .unwrap_or(imaging::tools::auto_focus::DEFAULT_CONFIRMATION_TOLERANCE),
        };

        let client = match self.require_active_guiding().await {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        match self
            .run_guide_af_sweep(train_id, &focuser_id, &sweep, client, progress_sink, cancel)
            .await
        {
            Ok(outcome) => Ok(tool_success!({
                "best_position": outcome.best_position,
                "best_hfd": outcome.best_hfd,
                "fit_r_squared": outcome.fit_r_squared,
                "confirmation": outcome.confirmation,
                "confirmed": outcome.confirmed,
                "final_position": outcome.final_position,
                "final_hfd": outcome.final_hfd,
                "samples_used": outcome.samples_used,
                "curve_points": outcome.curve_points,
                "temperature_c": outcome.temperature_c,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }
}

/// Resolved geometry of a guide-train (PHD2-metric) sweep.
#[derive(Debug, Clone)]
struct GuideSweepParams {
    step_size: i32,
    half_width: i32,
    frames_per_step: u32,
    min_fit_points: usize,
    confirmation_tolerance: f64,
}

/// Result of a guide-train metric sweep: HFD samples, and no capture
/// documents, since nothing is captured.
struct GuideAfOutcome {
    best_position: i32,
    best_hfd: f64,
    fit_r_squared: f64,
    /// `{hfd, frames_used, accepted}` — the sample set collected at
    /// the fitted position after the final move.
    confirmation: serde_json::Value,
    confirmed: bool,
    final_position: i32,
    final_hfd: f64,
    samples_used: usize,
    curve_points: Vec<serde_json::Value>,
    temperature_c: Option<f64>,
}

/// The fitted vertex of a metric sweep plus the lowest accepted
/// sample — what the confirmation stage is held to.
struct GuideFitStage {
    best_position: i32,
    best_hfd: f64,
    r_squared: f64,
    lowest_position: i32,
    lowest_hfd: f64,
}

/// Fit the metric sweep's samples and validate the vertex against the
/// positions that produced them. Pure: the caller restores the focuser
/// on failure.
fn guide_fit_stage(
    fit_samples: &[(i32, f64, u32)],
    grid: &[i32],
    min_fit_points: usize,
) -> Result<GuideFitStage, String> {
    if fit_samples.len() < min_fit_points {
        return Err(format!(
            "not enough valid guide samples: {} of {} positions produced an HFD, \
             min_fit_points is {}",
            fit_samples.len(),
            grid.len(),
            min_fit_points
        ));
    }
    // The hyperbola, the defocus curve itself, which the focus-model
    // provider's capture sweep fits too (docs/crates/rp-focus-curve.md).
    let fit = rp_focus_curve::fit_hyperbola(fit_samples).map_err(|e| e.to_string())?;
    let best_position = imaging::tools::auto_focus::nearest_step(fit.curve.centre);
    // Bounded by the positions that produced an HFD, not by the grid:
    // the grid's null edges are where the star was lost, and a centre
    // there is one wing extrapolated past its samples.
    let (Some(sampled_min), Some(sampled_max)) = (
        fit_samples.iter().map(|(position, _, _)| *position).min(),
        fit_samples.iter().map(|(position, _, _)| *position).max(),
    ) else {
        return Err("monotonic curve: no valid sample to bound the minimum with".to_string());
    };
    if best_position < sampled_min || best_position > sampled_max {
        return Err(format!(
            "monotonic curve: fitted minimum {best_position} lies outside the sampled \
             range [{sampled_min}, {sampled_max}]"
        ));
    }
    let Some((lowest_position, lowest_hfd, _)) =
        imaging::tools::auto_focus::lowest_sample(fit_samples)
    else {
        return Err("monotonic curve: no valid sample to confirm against".to_string());
    };
    Ok(GuideFitStage {
        best_position,
        best_hfd: fit.curve.minimum,
        r_squared: fit.r_squared,
        lowest_position,
        lowest_hfd,
    })
}

/// Ceiling per awaited guide frame during a metric sweep. Guide
/// exposures are seconds; a frame that takes longer than this means
/// PHD2 stopped producing them and the sweep must fail rather than
/// hang.
const GUIDE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll cadence against the guider metrics window during a sweep.
const GUIDE_METRICS_POLL: Duration = Duration::from_millis(500);

impl McpHandler {
    /// The active-guiding precondition of the sweep: PHD2 emits HFD
    /// only while it guides.
    async fn require_active_guiding(
        &self,
    ) -> Result<std::sync::Arc<dyn rp_guider::GuiderClient>, String> {
        let what = "guide-train auto_focus";
        let Some(client) = self.guider.clone() else {
            return Err(format!(
                "{what} requires active guiding (guider not configured)"
            ));
        };
        match client.guiding_stats().await {
            Ok(stats) if stats.guiding => Ok(client),
            Ok(stats) => Err(format!(
                "{what} requires active guiding (PHD2 state: {})",
                stats.app_state
            )),
            Err(e) => Err(format!(
                "{what} requires active guiding (stats unavailable: {e})"
            )),
        }
    }

    /// The guide-train sweep enters the reservation table for its
    /// length, put-back included, and is refused at once when a train
    /// lease holds its camera or focuser (rp.md § Train Lease, "rp's own
    /// sweeps").
    fn enter_guide_sweep(
        &self,
        train_id: &str,
        focuser_id: &str,
    ) -> Result<crate::train_lease::SweepGuard, String> {
        let camera_id = self
            .trains
            .train(train_id)
            .and_then(crate::equipment::trains::Train::camera_id)
            .unwrap_or_default();
        self.leases.enter_sweep(
            format!("the auto_focus sweep of focuser '{focuser_id}' in guiding train '{train_id}'"),
            camera_id,
            focuser_id,
        )
    }

    /// One full PHD2-metric V-curve run on the guiding train's
    /// terminal focuser, the per-position sample being the median HFD
    /// of `frames_per_step` fresh guide frames. Corrections stay active
    /// for the whole sweep — pausing them would stop the `GuideStep`
    /// stream this sweep reads.
    async fn run_guide_af_sweep(
        &self,
        train_id: &str,
        focuser_id: &str,
        sweep: &GuideSweepParams,
        client: std::sync::Arc<dyn rp_guider::GuiderClient>,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<GuideAfOutcome, String> {
        let _sweep = self.enter_guide_sweep(train_id, focuser_id)?;
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let (foc, bounds) = foc_entry
            .device_with_travel_bounds()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;

        let starting_position = foc
            .position()
            .await
            .map_err(|e| format!("failed to read focuser position: {e}"))?;
        let temperature_c: Option<f64> = foc.temperature().await.ok();

        let mut grid = imaging::tools::auto_focus::build_grid(
            starting_position,
            sweep.step_size,
            sweep.half_width,
            bounds,
        );
        if grid.len() < sweep.min_fit_points {
            return Err(format!(
                "sweep grid has {} positions after clamping, fewer than min_fit_points {}",
                grid.len(),
                sweep.min_fit_points
            ));
        }
        // Every sample reached from the focuser's backlash approach
        // side, as the final move arrives.
        if sweep_direction_for(&foc_entry.config)
            == imaging::tools::auto_focus::SweepDirection::Descending
        {
            grid.reverse();
        }
        let grid = grid;

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus.emit_operation(EventEnvelope::started(
            "focus",
            &operation_id,
            started_at,
            serde_json::json!({
                "camera_id": serde_json::Value::Null,
                "focuser_id": focuser_id,
                "train_id": train_id,
                "position": starting_position,
                "temperature": temperature_c,
                "method": "phd2_hfd",
            }),
        ));

        let emitter = progress_sink.as_ref().map(ProgressSink::as_emitter);
        let result = self
            .guide_sweep_body(
                focuser_id,
                sweep,
                &client,
                &grid,
                starting_position,
                emitter,
                &cancel,
            )
            .await
            .map(|outcome| GuideAfOutcome {
                temperature_c,
                ..outcome
            });

        match result {
            Ok(outcome) => {
                self.event_bus.emit_operation(EventEnvelope::complete(
                    "focus",
                    &operation_id,
                    started_at,
                    serde_json::json!({
                        "camera_id": serde_json::Value::Null,
                        "focuser_id": focuser_id,
                        "train_id": train_id,
                        "position": outcome.final_position,
                        "hfd": outcome.final_hfd,
                        "best_position": outcome.best_position,
                        "best_hfd": outcome.best_hfd,
                        "confirmed": outcome.confirmed,
                        "fit_r_squared": outcome.fit_r_squared,
                        "samples_used": outcome.samples_used,
                        "method": "phd2_hfd",
                    }),
                ));
                Ok(outcome)
            }
            Err(e) => {
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "focus",
                    &operation_id,
                    started_at,
                    &e,
                ));
                Err(e)
            }
        }
    }

    /// Collect one metric-sweep sample: poll the guider metrics
    /// window until `frames_per_step` frames above `watermark` have
    /// arrived, then report the median HFD of the valid ones among
    /// **exactly the earliest `frames_per_step` fresh frames** — a
    /// slow poll never inflates the sample set past the documented
    /// size. `None` when every considered frame was invalid (star
    /// lost or no HFD), the expected bracket shape at deep defocus.
    /// Also returns the highest considered frame number, the next
    /// position's fallback watermark.
    async fn collect_guide_sample(
        &self,
        client: &dyn rp_guider::GuiderClient,
        watermark: u64,
        frames_per_step: u32,
        cancel: &Cancel,
    ) -> Result<(Option<f64>, u32, u64), String> {
        // Saturating budget; a clock-overflowing deadline degrades to
        // already-expired (immediate timeout), not a panic.
        let now = tokio::time::Instant::now();
        let deadline = now
            .checked_add(GUIDE_FRAME_TIMEOUT.saturating_mul(frames_per_step.max(1)))
            .unwrap_or(now);
        loop {
            let metrics = client
                .guiding_metrics()
                .await
                .map_err(|e| format!("failed to read guider metrics: {e}"))?;
            if !metrics.guiding {
                // No fresh frames will ever arrive — fail now instead
                // of burning the per-position ceiling.
                return Err(
                    "guiding stopped during the metric sweep (PHD2 is no longer guiding)"
                        .to_string(),
                );
            }
            let mut fresh: Vec<&rp_guider::FrameMetrics> = metrics
                .frames
                .iter()
                .filter(|f| f.frame > watermark)
                .collect();
            if fresh.len() >= usize::try_from(frames_per_step).unwrap_or(usize::MAX) {
                fresh.sort_by_key(|f| f.frame);
                // The returned watermark covers the FULL fresh set —
                // frames beyond the sample-set truncation below were
                // exposed at this position and must not leak into the
                // next one should its refresh read fail.
                let max_frame = fresh.iter().map(|f| f.frame).max().unwrap_or(watermark);
                fresh.truncate(usize::try_from(frames_per_step).unwrap_or(usize::MAX));
                let mut valid: Vec<f64> = fresh
                    .iter()
                    .filter(|f| !f.star_lost)
                    .filter_map(|f| f.hfd)
                    .collect();
                let frames_used = u32::try_from(valid.len()).unwrap_or(u32::MAX);
                if valid.is_empty() {
                    return Ok((None, 0, max_frame));
                }
                valid.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                // `len / 2` is in bounds for the non-empty set checked above.
                let median = valid
                    .get(valid.len() / 2)
                    .copied()
                    .ok_or_else(|| "median index out of bounds".to_string())?;
                return Ok((Some(median), frames_used, max_frame));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "timeout waiting for {frames_per_step} fresh guide frames \
                     (got {} within the per-position ceiling)",
                    fresh.len()
                ));
            }
            // No stop-class counterpart: the guider keeps guiding
            // (rp.md § In-Flight Tool Calls).
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(cancel.error()),
                () = tokio::time::sleep(GUIDE_METRICS_POLL) => {}
            }
        }
    }

    /// One metric-sweep sample at the focuser's current position:
    /// refresh the freshness watermark *after* the move settles, so
    /// frames exposed during the motion — at a stale focus — never
    /// count, then collect. Returns the sample, its valid-frame count,
    /// and the advanced watermark. A failed refresh read errors the
    /// run: without a fresh watermark, frames exposed during the move
    /// (or, at the first position, before the sweep) would pass as
    /// fresh, and the collect loop fails on a read error anyway.
    async fn guide_sample_here(
        &self,
        client: &std::sync::Arc<dyn rp_guider::GuiderClient>,
        watermark: u64,
        frames_per_step: u32,
        cancel: &Cancel,
    ) -> Result<(Option<f64>, u32, u64), String> {
        let refreshed = client.guiding_metrics().await.map_err(|e| {
            format!("failed to read guider metrics for the freshness watermark: {e}")
        })?;
        let watermark = latest_frame(Some(&refreshed)).max(watermark);
        let (sample, frames_used, max_frame) = self
            .collect_guide_sample(client.as_ref(), watermark, frames_per_step, cancel)
            .await?;
        Ok((sample, frames_used, max_frame.max(watermark)))
    }

    /// The sweep body of `run_guide_af_sweep`, isolated so the caller
    /// emits exactly one of `focus_complete` / `focus_failed` for
    /// whatever it returns.
    #[expect(
        clippy::too_many_arguments,
        reason = "the sweep's inputs are all distinct: geometry, guider, grid, origin, and the two per-request handles"
    )]
    async fn guide_sweep_body(
        &self,
        focuser_id: &str,
        sweep: &GuideSweepParams,
        client: &std::sync::Arc<dyn rp_guider::GuiderClient>,
        grid: &[i32],
        starting_position: i32,
        emitter: Option<&dyn ProgressEmitter>,
        cancel: &Cancel,
    ) -> Result<GuideAfOutcome, String> {
        let mut watermark = 0;
        let mut curve_points = Vec::with_capacity(grid.len());
        let mut fit_samples: Vec<(i32, f64, u32)> = Vec::new();
        for &position in grid {
            self.do_move_focuser_blocking(focuser_id, position, emitter, cancel)
                .await?;
            let (sample, frames_used, max_frame) = self
                .guide_sample_here(client, watermark, sweep.frames_per_step, cancel)
                .await?;
            watermark = max_frame;
            if let Some(hfd) = sample {
                // Weight by the valid-frame count behind the
                // median — a capture sweep's star-count weighting,
                // one metric over: a position where most frames
                // were invalid contributes a noisier median and
                // should pull the fit less.
                fit_samples.push((position, hfd, frames_used));
            }
            curve_points.push(serde_json::json!({
                "position": position,
                "hfd": sample,
                "frames_used": frames_used,
            }));
        }

        let stage = match guide_fit_stage(&fit_samples, grid, sweep.min_fit_points) {
            Ok(stage) => stage,
            Err(e) => {
                // Best effort, never masking the fit error: the
                // focuser must not be left at the far end of the grid.
                if let Err(restore) = self
                    .do_move_focuser_blocking(focuser_id, starting_position, emitter, cancel)
                    .await
                {
                    debug!(
                        error = %restore,
                        starting_position,
                        "guide-train auto_focus could not restore the starting position"
                    );
                }
                return Err(e);
            }
        };

        let moved_to = self
            .do_move_focuser_blocking(focuser_id, stage.best_position, emitter, cancel)
            .await?
            .position;
        let (sample, frames_used, _) = self
            .guide_sample_here(client, watermark, sweep.frames_per_step, cancel)
            .await?;
        let confirmed = imaging::tools::auto_focus::confirmation_accepted(
            sample,
            stage.lowest_hfd,
            sweep.confirmation_tolerance,
        );
        debug!(
            best_position = stage.best_position,
            confirmation_hfd = ?sample,
            frames_used,
            lowest_position = stage.lowest_position,
            lowest_hfd = stage.lowest_hfd,
            confirmed,
            "guide-train auto_focus confirmation sample collected"
        );
        let (final_position, final_hfd) = if let (true, Some(hfd)) = (confirmed, sample) {
            (moved_to, hfd)
        } else {
            let position = self
                .do_move_focuser_blocking(focuser_id, stage.lowest_position, emitter, cancel)
                .await?
                .position;
            (position, stage.lowest_hfd)
        };
        Ok(GuideAfOutcome {
            best_position: stage.best_position,
            best_hfd: stage.best_hfd,
            fit_r_squared: stage.r_squared,
            confirmation: serde_json::json!({
                "hfd": sample,
                "frames_used": frames_used,
                "accepted": confirmed,
            }),
            confirmed,
            final_position,
            final_hfd,
            samples_used: fit_samples.len(),
            curve_points,
            // Stamped by the caller, which read the thermistor once.
            temperature_c: None,
        })
    }
}

/// The highest frame number in a metrics window, 0 when absent — the
/// initial freshness watermark.
fn latest_frame(metrics: Option<&rp_guider::GuidingMetrics>) -> u64 {
    metrics.map_or(0, |m| m.frames.iter().map(|f| f.frame).max().unwrap_or(0))
}

/// The sweep walk order for a focuser: descending when its backlash
/// block says every move arrives travelling inward, ascending
/// otherwise (rp.md § Focuser Tool Details).
fn sweep_direction_for(
    config: &crate::config::FocuserConfig,
) -> imaging::tools::auto_focus::SweepDirection {
    use crate::config::focuser::BacklashApproach;
    use imaging::tools::auto_focus::SweepDirection;

    match config.backlash.map(|backlash| backlash.approach) {
        Some(BacklashApproach::In) => SweepDirection::Descending,
        Some(BacklashApproach::Out) | None => SweepDirection::Ascending,
    }
}
