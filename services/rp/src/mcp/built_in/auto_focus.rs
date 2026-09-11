//! Auto-focus tool category: the `auto_focus` V-curve compound tool
//! and the train-aware `refocus_train` expansion (rp.md § Optical
//! Trains, §`auto_focus` Contract, §`refocus_train` Contract).

use std::collections::HashSet;
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
use super::super::internals::{CaptureRequest, ResolvedParams};
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{tool_error, tool_success};
use super::camera::DEFAULT_BINNING;
use crate::config::optical_train::SweepBinning;
use crate::config::{TrainAutoFocusConfig, TrainPurpose};
use crate::events::EventEnvelope;
use crate::imaging;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["camera_id", "focuser_id"]}, {"required": ["train_id"]}]))]
pub struct AutoFocusToolParams {
    /// Camera that captures each sweep frame. Pass together with
    /// `focuser_id`; mutually exclusive with `train_id`.
    #[serde(default)]
    pub camera_id: Option<String>,
    /// Focuser to sweep.
    #[serde(default)]
    pub focuser_id: Option<String>,
    /// Optical-train id: resolves the train's terminal camera and
    /// focuser, with the sweep parameters falling back to the train's
    /// `auto_focus` config block. Mutually exclusive with the explicit
    /// pair. The guiding train selects the PHD2-metric sweep
    /// (requires an active guide loop; never captures through the
    /// guide camera).
    #[serde(default)]
    pub train_id: Option<String>,
    /// Per-frame exposure (humantime string).
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub duration: Option<Duration>,
    /// Focuser steps between sweep grid points (positive integer).
    #[serde(default)]
    pub step_size: Option<i32>,
    /// Half-width of the sweep around the current focuser position
    /// (positive integer).
    #[serde(default)]
    pub half_width: Option<i32>,
    /// Minimum component pixel area for `measure_basic` (no default —
    /// rig-dependent).
    #[serde(default)]
    pub min_area: Option<usize>,
    /// Maximum component pixel area for `measure_basic` (no default —
    /// rig-dependent; donut PSFs at extreme defocus can span many
    /// hundreds of pixels).
    #[serde(default)]
    pub max_area: Option<usize>,
    /// Binning for every sweep frame, `"AxB"`. Default `"1x1"`.
    /// Focus compares frames of one sweep with each other, so binning
    /// costs it nothing and saves readout and download on every point.
    #[serde(default)]
    pub binning: Option<rp_vocabulary::Binning>,
    /// Per-frame `measure_basic` threshold (sigma units). Default 5.0.
    #[serde(default)]
    pub threshold_sigma: Option<f64>,
    /// Minimum number of accepted HFR samples for the parabolic fit.
    /// Default 5.
    #[serde(default)]
    pub min_fit_points: Option<usize>,
    /// Sparse-sample gate: a sweep sample whose `star_count` is below
    /// this fraction of the sweep's largest `star_count` is rejected
    /// before the fit. Default 0.1; 0 disables. Capture sweeps only.
    #[serde(default)]
    pub min_star_fraction: Option<f64>,
    /// How much worse than the lowest accepted sweep sample the
    /// confirmation measurement at the fitted position may be before
    /// the focuser falls back to that sample's position. Default 0.25.
    #[serde(default)]
    pub confirmation_tolerance: Option<f64>,
    /// How many sweeps the run may make before it errors: a failed fit
    /// is repeated with the same parameters, the grid shifted toward
    /// the lowest sample after a monotonic curve. Default 2, at most
    /// 5. Capture sweeps only.
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("required" = ["train_id"]))]
pub struct RefocusTrainParams {
    /// The train whose refocus trigger to expand.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Free-form trigger description recorded on `refocus_started`
    /// and the result (e.g. "`temperature_drift`"). Default "manual".
    #[serde(default)]
    pub reason: Option<String>,
}

/// One fully-resolved AF step of a `refocus_train` expansion: a
/// capture-based V-curve run in an imaging train, or a PHD2-metric
/// run in the guiding train.
enum PlannedStep {
    Capture {
        focuser_id: String,
        train_id: String,
        camera_id: String,
        binning: rp_vocabulary::Binning,
        af_params: imaging::tools::auto_focus::AutoFocusParams,
    },
    Metric {
        focuser_id: String,
        train_id: String,
        sweep: GuideSweepParams,
    },
}

impl PlannedStep {
    fn focuser_id(&self) -> &str {
        match self {
            Self::Capture { focuser_id, .. } | Self::Metric { focuser_id, .. } => focuser_id,
        }
    }

    fn train_id(&self) -> &str {
        match self {
            Self::Capture { train_id, .. } | Self::Metric { train_id, .. } => train_id,
        }
    }
}

#[tool_router(router = tool_router_auto_focus, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "V-curve auto-focus: sweep ± half_width around the focuser's current position, capture and run measure_basic at each step, drop samples whose star count collapsed (min_star_fraction of the sweep's best frame), fit a parabola in HFR, move the focuser to the fitted minimum, and confirm it with one more frame — falling back to the lowest measured sweep sample when the confirmation measures worse than confirmation_tolerance allows. A capture sweep whose fit fails is repeated with the same parameters up to max_attempts times (default 2), shifted toward the lowest sample after a monotonic curve, and its result reports attempts and the wing slope the next sweep can be sized from; the guiding train's PHD2-metric sweep makes one attempt and reports neither field. Address the devices as camera_id + focuser_id, or as train_id (the train's terminal camera + focuser, sweep parameters falling back to the train's auto_focus config block)."
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

    #[tool(
        description = "Expand one refocus trigger on an optical train into the dependency-ordered auto-focus sequence: shared focusers upstream-first (each run in the train where it is terminal), then the train's own terminal focuser. Sweep parameters come from each run train's auto_focus config block; guiding is paused around the sequence when a step moves a guiding-train focuser."
    )]
    pub(crate) async fn refocus_train(
        &self,
        Parameters(params): Parameters<RefocusTrainParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let progress_sink = ProgressSink::from_request_context(&ctx);
        let cancel = Cancel::from_context(&ctx);
        self.refocus_train_inner(params, progress_sink, cancel)
            .await
    }

    /// Body of the `auto_focus` MCP tool, split out so unit tests can
    /// pass `None` for the progress sink without constructing a real
    /// rmcp `Peer` (its constructor is `pub(crate)` in rmcp 1.7).
    pub(crate) async fn auto_focus_inner(
        &self,
        mut params: AutoFocusToolParams,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Addressing: exactly one of the explicit pair or train_id.
        // Train addressing resolves the train's terminal devices and
        // merges the train's `auto_focus` config block under the
        // per-call parameters field by field, before the
        // "missing required parameter" checks below.
        let (camera_id, focuser_id) = if let Some(train_id) = params.train_id.take() {
            if params.camera_id.is_some() || params.focuser_id.is_some() {
                return Ok(tool_error!(
                    "auto_focus: train_id is mutually exclusive with camera_id and focuser_id"
                ));
            }
            let Some(train) = self.trains.train(&train_id) else {
                return Ok(tool_error!("train not found: {}", train_id));
            };
            if train.purpose == TrainPurpose::Guiding {
                return self
                    .guide_af_tool(&train_id, params, progress_sink, cancel)
                    .await;
            }
            let Some(camera_id) = train.camera_id() else {
                return Ok(tool_error!("train '{}' has no camera", train_id));
            };
            let Some(focuser_id) = train.terminal_focuser() else {
                return Ok(tool_error!("train '{}' has no focuser", train_id));
            };
            let (camera_id, focuser_id) = (camera_id.to_string(), focuser_id.to_string());
            if let Some(block) = &train.auto_focus {
                merge_block_into_params(&mut params, block);
            }
            (camera_id, focuser_id)
        } else {
            // Field-presence validation runs in input order so the error
            // message always points at the first missing field — same
            // pattern as `measure_basic`.
            let camera_id = match params.camera_id.as_deref() {
                Some(s) => s.to_string(),
                None => return Ok(tool_error!("missing required parameter: camera_id")),
            };
            let focuser_id = match params.focuser_id.as_deref() {
                Some(s) => s.to_string(),
                None => return Ok(tool_error!("missing required parameter: focuser_id")),
            };
            (camera_id, focuser_id)
        };

        let (binning, af_params) = match capture_sweep_from_params(&params) {
            Ok(resolved) => resolved,
            Err(e) => return Ok(*e),
        };

        match self
            .run_auto_focus_step(
                &camera_id,
                &focuser_id,
                binning,
                af_params,
                progress_sink,
                cancel,
            )
            .await
        {
            Ok(result) => {
                let curve_points =
                    serde_json::to_value(&result.curve_points).unwrap_or(serde_json::Value::Null);
                let confirmation =
                    serde_json::to_value(&result.confirmation).unwrap_or(serde_json::Value::Null);
                Ok(tool_success!({
                    "best_position": result.best_position,
                    "best_hfr": result.best_hfr,
                    "fit_r_squared": result.fit_r_squared,
                    "confirmation": confirmation,
                    "confirmed": result.confirmation.accepted,
                    "final_position": result.final_position,
                    "final_hfr": result.final_hfr,
                    "samples_used": result.samples_used,
                    "curve_points": curve_points,
                    "attempts": result.attempts,
                    "wing_slope": result.wing_slope,
                    "temperature_c": result.temperature_c,
                }))
            }
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    /// Body of the `refocus_train` MCP tool — see `auto_focus_inner`
    /// for why the split exists.
    #[expect(
        clippy::too_many_lines,
        reason = "one pass over the planned steps with the pause/resume choreography inline; splitting it would hide the ordering the contract specifies"
    )]
    pub(crate) async fn refocus_train_inner(
        &self,
        params: RefocusTrainParams,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(train_id) = params.train_id else {
            return Ok(tool_error!("missing required parameter: train_id"));
        };
        let reason = params.reason.unwrap_or_else(|| "manual".to_string());

        let planned = match self.plan_refocus_steps(&train_id) {
            Ok(planned) => planned,
            Err(e) => return Ok(*e),
        };

        let metric_client = match self.refocus_metric_client(&planned).await {
            Ok(client) => client,
            Err(e) => return Ok(tool_error!("refocus_train: {}", e)),
        };

        let pause_client = self.refocus_pause_client(&train_id, &planned).await;
        let guiding_paused = pause_client.is_some();

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.emit_refocus_started(
            &operation_id,
            started_at,
            &train_id,
            &reason,
            &planned,
            guiding_paused,
        );

        if let Some(client) = &pause_client {
            if let Err(e) = client.pause_guiding(false).await {
                let msg = format!("refocus_train: failed to pause guiding: {e}");
                return Ok(self.refocus_failed(&operation_id, started_at, &msg));
            }
        }
        let mut paused = pause_client.is_some();

        let mut completed = Vec::with_capacity(planned.len());
        for (i, step) in planned.iter().enumerate() {
            debug!(
                train_id,
                focuser_id = %step.focuser_id(),
                run_train = %step.train_id(),
                "running refocus step {} of {}",
                i.saturating_add(1),
                planned.len()
            );
            // A metric step needs the GuideStep stream, so corrections
            // must be flowing again before it runs. A resume that
            // fails here is a hard error, same as the end-of-sequence
            // resume — the metric step cannot proceed paused.
            if matches!(step, PlannedStep::Metric { .. }) && paused {
                if let Some(client) = &pause_client {
                    if let Err(e) = client.resume_guiding().await {
                        let msg = format!(
                            "refocus_train: failed to resume guiding before the guide-train \
                             step: {e}"
                        );
                        return Ok(self.refocus_failed(&operation_id, started_at, &msg));
                    }
                }
                paused = false;
            }
            let step_result = self
                .run_planned_step(
                    step,
                    metric_client.as_ref(),
                    progress_sink.clone(),
                    cancel.clone(),
                )
                .await;
            match step_result {
                Ok(entry) => completed.push(entry),
                Err(e) => {
                    // Later steps depend on this one having landed, so
                    // stop here — but never leave guiding paused.
                    let resume_note = match (&pause_client, paused) {
                        (Some(client), true) => match client.resume_guiding().await {
                            Ok(()) => String::new(),
                            Err(re) => format!("; also failed to resume guiding: {re}"),
                        },
                        _ => String::new(),
                    };
                    let msg = format!(
                        "refocus_train: step {} (focuser '{}' in train '{}') failed: {e}{resume_note}",
                        i.saturating_add(1),
                        step.focuser_id(),
                        step.train_id(),
                    );
                    return Ok(self.refocus_failed(&operation_id, started_at, &msg));
                }
            }
        }

        if paused {
            if let Some(client) = &pause_client {
                if let Err(e) = client.resume_guiding().await {
                    let msg = format!(
                        "refocus_train: all steps completed but resuming guiding failed: {e}"
                    );
                    return Ok(self.refocus_failed(&operation_id, started_at, &msg));
                }
            }
        }

        Ok(self.finish_refocus(
            &operation_id,
            started_at,
            &train_id,
            &reason,
            guiding_paused,
            &completed,
        ))
    }

    /// Expansion + per-step resolution for `refocus_train`, all before
    /// any motion or event: an invalid expansion never touches
    /// hardware.
    fn plan_refocus_steps(&self, train_id: &str) -> Result<Vec<PlannedStep>, Box<CallToolResult>> {
        if self.trains.train(train_id).is_none() {
            return Err(Box::new(tool_error!("train not found: {}", train_id)));
        }
        let steps = self.trains.af_sequence(train_id).unwrap_or_default();
        if steps.is_empty() {
            return Err(Box::new(tool_error!(
                "refocus_train: train '{}' has no focusers",
                train_id
            )));
        }
        let mut planned = Vec::with_capacity(steps.len());
        for step in &steps {
            let Some(run_train) = self.trains.train(&step.train_id) else {
                return Err(Box::new(tool_error!("train not found: {}", step.train_id)));
            };
            let Some(block) = &run_train.auto_focus else {
                return Err(Box::new(tool_error!(
                    "refocus_train: train '{}' has no auto_focus config block \
                     (required for the step focusing '{}')",
                    step.train_id,
                    step.focuser_id
                )));
            };
            if run_train.purpose == TrainPurpose::Guiding {
                planned.push(PlannedStep::Metric {
                    focuser_id: step.focuser_id.clone(),
                    train_id: step.train_id.clone(),
                    sweep: guide_sweep_from_block(block),
                });
            } else {
                let Some(camera_id) = run_train.camera_id() else {
                    return Err(Box::new(tool_error!(
                        "train '{}' has no camera",
                        step.train_id
                    )));
                };
                let af_params = match af_params_from_block(&step.train_id, block) {
                    Ok(p) => p,
                    Err(e) => return Err(Box::new(tool_error!("refocus_train: {}", e))),
                };
                // Planning is the only point before `refocus_started`
                // and the guiding pause. `run_auto_focus_step` checks
                // the binning too, but a step that cannot run must not
                // first announce a refocus and interrupt guiding for
                // it. A camera that is not connected is left to the
                // step, which owns that error message.
                let binning = block.binning.map_or(DEFAULT_BINNING, SweepBinning::value);
                if let Some(cam_entry) = self.equipment.find_camera(camera_id) {
                    if let Err(e) =
                        crate::mcp::internals::validate_binning(binning, &cam_entry.invariants())
                    {
                        return Err(Box::new(tool_error!(
                            "refocus_train: train '{}': {}",
                            step.train_id,
                            e
                        )));
                    }
                }
                planned.push(PlannedStep::Capture {
                    focuser_id: step.focuser_id.clone(),
                    train_id: step.train_id.clone(),
                    camera_id: camera_id.to_string(),
                    binning,
                    af_params,
                });
            }
        }
        Ok(planned)
    }

    /// Guiding handshake decision (rp.md §`refocus_train` Contract):
    /// pause only when a *capture-based* step moves a guiding-train
    /// focuser AND the guider is configured AND it reports an active
    /// loop. A stats read that fails or reports not-guiding skips the
    /// handshake — a broken guider service must not block a refocus.
    /// Metric steps run under active corrections; the execution loop
    /// resumes before the first one.
    async fn refocus_pause_client(
        &self,
        train_id: &str,
        planned: &[PlannedStep],
    ) -> Option<std::sync::Arc<dyn rp_guider::GuiderClient>> {
        let guiding_members: HashSet<&str> = self
            .trains
            .guiding_train()
            .map(|t| t.devices.iter().map(|d| d.id.as_str()).collect())
            .unwrap_or_default();
        let touches_guiding = planned.iter().any(|s| {
            matches!(s, PlannedStep::Capture { .. }) && guiding_members.contains(s.focuser_id())
        });
        if !touches_guiding {
            return None;
        }
        let client = self.guider.clone()?;
        match client.guiding_stats().await {
            Ok(stats) if stats.guiding => Some(client),
            Ok(_) => {
                debug!(
                    train_id,
                    "guider reports not guiding; skipping pause handshake"
                );
                None
            }
            Err(e) => {
                debug!(train_id, error = %e, "guider stats unreachable; skipping pause handshake");
                None
            }
        }
    }

    /// A metric step reads PHD2's `GuideStep` stream, so the whole
    /// expansion is refused before any motion when guiding is not
    /// active — unlike the pause handshake, which degrades gracefully
    /// (Tenet 2). `Ok(None)` when no step is metric.
    async fn refocus_metric_client(
        &self,
        planned: &[PlannedStep],
    ) -> Result<Option<std::sync::Arc<dyn rp_guider::GuiderClient>>, String> {
        let has_metric = planned
            .iter()
            .any(|s| matches!(s, PlannedStep::Metric { .. }));
        if !has_metric {
            return Ok(None);
        }
        self.require_active_guiding("guide-train step")
            .await
            .map(Some)
    }

    /// Emit the `refocus` started envelope naming every planned step.
    fn emit_refocus_started(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        train_id: &str,
        reason: &str,
        planned: &[PlannedStep],
        guiding_paused: bool,
    ) {
        self.event_bus.emit_operation(EventEnvelope::started(
            "refocus",
            operation_id,
            started_at,
            serde_json::json!({
                "train_id": train_id,
                "reason": reason,
                "steps": planned
                    .iter()
                    .map(|s| serde_json::json!({
                        "focuser_id": s.focuser_id(),
                        "train_id": s.train_id(),
                    }))
                    .collect::<Vec<_>>(),
                "guiding_paused": guiding_paused,
            }),
        ));
    }

    /// Emit the `refocus` completion envelope and produce the tool
    /// success payload.
    fn finish_refocus(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        train_id: &str,
        reason: &str,
        guiding_paused: bool,
        completed: &[serde_json::Value],
    ) -> CallToolResult {
        self.event_bus.emit_operation(EventEnvelope::complete(
            "refocus",
            operation_id,
            started_at,
            serde_json::json!({
                "train_id": train_id,
                "steps": completed,
            }),
        ));
        tool_success!({
            "train_id": train_id,
            "reason": reason,
            "guiding_paused": guiding_paused,
            "steps": completed,
        })
    }

    /// Emit the `refocus` failure envelope and produce the matching
    /// tool error.
    fn refocus_failed(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        msg: &str,
    ) -> CallToolResult {
        self.event_bus.emit_operation(EventEnvelope::failed(
            "refocus",
            operation_id,
            started_at,
            msg,
        ));
        tool_error!("{}", msg)
    }

    /// Execute one planned refocus step, producing its entry in the
    /// per-step summary.
    async fn run_planned_step(
        &self,
        step: &PlannedStep,
        metric_client: Option<&std::sync::Arc<dyn rp_guider::GuiderClient>>,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<serde_json::Value, String> {
        match step {
            PlannedStep::Capture {
                camera_id,
                focuser_id,
                train_id: run_train,
                binning,
                af_params,
            } => self
                .run_auto_focus_step(
                    camera_id,
                    focuser_id,
                    *binning,
                    af_params.clone(),
                    progress_sink,
                    cancel,
                )
                .await
                .map(|result| {
                    serde_json::json!({
                        "focuser_id": focuser_id,
                        "train_id": run_train,
                        "camera_id": camera_id,
                        "best_position": result.best_position,
                        "best_hfr": result.best_hfr,
                        "final_position": result.final_position,
                        "final_hfr": result.final_hfr,
                        "confirmed": result.confirmation.accepted,
                        "samples_used": result.samples_used,
                    })
                }),
            PlannedStep::Metric {
                focuser_id,
                train_id: run_train,
                sweep,
            } => match metric_client {
                Some(client) => self
                    .run_guide_af_sweep(
                        run_train,
                        focuser_id,
                        sweep,
                        client.clone(),
                        progress_sink,
                        cancel,
                    )
                    .await
                    .map(|outcome| {
                        serde_json::json!({
                            "focuser_id": focuser_id,
                            "train_id": run_train,
                            "camera_id": serde_json::Value::Null,
                            "best_position": outcome.best_position,
                            "best_hfd": outcome.best_hfd,
                            "final_position": outcome.final_position,
                            "final_hfd": outcome.final_hfd,
                            "confirmed": outcome.confirmed,
                            "samples_used": outcome.samples_used,
                        })
                    }),
                None => Err("guide-train step planned without an active guider".to_string()),
            },
        }
    }

    /// One full V-curve run for a resolved camera + focuser pair: the
    /// shared body of `auto_focus` and each `refocus_train` step.
    /// Resolves the devices, reads the starting position and
    /// temperature, emits the `focus_started` / `focus_complete` /
    /// `focus_failed` triple, and drives the sweep through
    /// [`AutoFocusAdapter`].
    async fn run_auto_focus_step(
        &self,
        camera_id: &str,
        focuser_id: &str,
        binning: rp_vocabulary::Binning,
        af_params: imaging::tools::auto_focus::AutoFocusParams,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<imaging::tools::auto_focus::AutoFocusResult, String> {
        // Resolve devices early — the standard "<kind> not found" /
        // "<kind> not connected" errors, camera before focuser to
        // match input order in the contract. The camera is resolved
        // purely for the connection check; `do_capture` re-resolves.
        let cam_entry = self
            .equipment
            .find_camera(camera_id)
            .ok_or_else(|| format!("camera not found: {camera_id}"))?;
        if cam_entry.device().is_none() {
            return Err(format!("camera not connected: {camera_id}"));
        }
        // The contract promises a bad sweep parameter errors before any
        // motion. `do_capture` validates the binning too, but not until
        // the first sweep frame — by which point `focus_started` is out
        // and the focuser has moved. Check it here, against the same
        // cached capabilities, so an impossible binning costs nothing.
        crate::mcp::internals::validate_binning(binning, &cam_entry.invariants())?;
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;

        // Read the current focuser position + temperature exactly once
        // each (per the Contract algorithm step 1) and thread the values
        // through to both `focus_started` *and* `run_auto_focus` so the
        // event payload and the result's `temperature_c`/sweep-grid
        // origin can never disagree. Temperature is informational only:
        // any read failure (NOT_IMPLEMENTED or transient) becomes
        // `temperature_c: null`; we don't abort an auto-focus run over
        // a missing thermistor.
        let starting_position = foc
            .position()
            .await
            .map_err(|e| format!("failed to read focuser position: {e}"))?;
        let starting_temperature_c: Option<f64> = foc.temperature().await.ok();
        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus.emit_operation(EventEnvelope::started(
            "focus",
            &operation_id,
            started_at,
            serde_json::json!({
                "camera_id": camera_id,
                "focuser_id": focuser_id,
                "position": starting_position,
                "temperature": starting_temperature_c,
            }),
        ));

        let bounds = (foc_entry.config.min_position, foc_entry.config.max_position);
        // The walk order is the focuser's property, not the caller's:
        // every sample must be reached from the backlash approach side.
        let af_params = imaging::tools::auto_focus::AutoFocusParams {
            direction: sweep_direction_for(&foc_entry.config),
            ..af_params
        };

        // Store the per-request sink and cancel handle on the adapter
        // so every inner `do_capture` / `do_move_focuser_blocking` call
        // emits progress through the same `progressToken` and stops on
        // the same cancellation.
        let adapter = AutoFocusAdapter {
            handler: self,
            camera_id: camera_id.to_string(),
            focuser_id: focuser_id.to_string(),
            binning,
            progress: progress_sink,
            cancel,
        };

        match imaging::tools::auto_focus::run_auto_focus(
            &adapter,
            &adapter,
            &adapter,
            bounds,
            starting_position,
            starting_temperature_c,
            af_params,
        )
        .await
        {
            Ok(result) => {
                self.event_bus.emit_operation(EventEnvelope::complete(
                    "focus",
                    &operation_id,
                    started_at,
                    serde_json::json!({
                        "camera_id": camera_id,
                        "focuser_id": focuser_id,
                        "position": result.final_position,
                        "hfr": result.final_hfr,
                        "best_position": result.best_position,
                        "best_hfr": result.best_hfr,
                        "confirmed": result.confirmation.accepted,
                        "fit_r_squared": result.fit_r_squared,
                        "samples_used": result.samples_used,
                        "attempts": result.attempts,
                    }),
                ));
                Ok(result)
            }
            Err(e) => {
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "focus",
                    &operation_id,
                    started_at,
                    &e.to_string(),
                ));
                Err(e.to_string())
            }
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

/// Result of a guide-train metric sweep — the metric-side analogue of
/// `AutoFocusResult`, with HFD samples and no capture documents.
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
/// grid. Pure: the caller restores the focuser on failure.
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
    let fit = imaging::tools::auto_focus::fit_parabola(fit_samples).map_err(|e| e.to_string())?;
    let best_position = fit.vertex_position();
    // The min-fit-points check above guarantees a non-empty grid.
    let (Some(&grid_min), Some(&grid_max)) = (grid.iter().min(), grid.iter().max()) else {
        return Err("monotonic curve: empty sample grid".to_string());
    };
    if best_position < grid_min || best_position > grid_max {
        return Err(format!(
            "monotonic curve: fitted minimum {best_position} lies outside the sampled \
             range [{grid_min}, {grid_max}]"
        ));
    }
    let Some((lowest_position, lowest_hfd, _)) =
        imaging::tools::auto_focus::lowest_sample(fit_samples)
    else {
        return Err("monotonic curve: no valid sample to confirm against".to_string());
    };
    Ok(GuideFitStage {
        best_position,
        best_hfd: fit.vertex_value(),
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
    /// The guiding-train branch of the `auto_focus` tool: parameter
    /// rules per the [`auto_focus` Contract]'s guide-train section,
    /// then the metric sweep.
    async fn guide_af_tool(
        &self,
        train_id: &str,
        params: AutoFocusToolParams,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Capture-only parameters cannot influence a metric sweep;
        // reject rather than silently ignore.
        if params.duration.is_some()
            || params.min_area.is_some()
            || params.max_area.is_some()
            || params.binning.is_some()
            || params.threshold_sigma.is_some()
            || params.min_star_fraction.is_some()
            || params.max_attempts.is_some()
        {
            return Ok(tool_error!(
                "auto_focus: duration, min_area, max_area, binning, threshold_sigma, \
                 min_star_fraction, and max_attempts apply only to capture-based sweeps \
                 (train '{}' is the guiding train, whose metric sweep makes one attempt)",
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
        let Some(train) = self.trains.train(train_id) else {
            return Ok(tool_error!("train not found: {}", train_id));
        };
        let Some(focuser_id) = train.terminal_focuser().map(str::to_string) else {
            return Ok(tool_error!("train '{}' has no focuser", train_id));
        };
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

        let client = match self.require_active_guiding("guide-train auto_focus").await {
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

    /// The active-guiding precondition shared by the guide-train
    /// sweep and `refocus_train`'s metric-step validation. `what`
    /// names the caller in the error ("guide-train `auto_focus`" /
    /// "guide-train step").
    async fn require_active_guiding(
        &self,
        what: &str,
    ) -> Result<std::sync::Arc<dyn rp_guider::GuiderClient>, String> {
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

    /// One full PHD2-metric V-curve run on the guiding train's
    /// terminal focuser: same grid, fit, and event triple as the
    /// capture sweep, with the per-position sample being the median
    /// HFD of `frames_per_step` fresh guide frames. Corrections stay
    /// active for the whole sweep — pausing them would stop the
    /// `GuideStep` stream this sweep reads.
    async fn run_guide_af_sweep(
        &self,
        train_id: &str,
        focuser_id: &str,
        sweep: &GuideSweepParams,
        client: std::sync::Arc<dyn rp_guider::GuiderClient>,
        progress_sink: Option<ProgressSink>,
        cancel: Cancel,
    ) -> Result<GuideAfOutcome, String> {
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;
        let bounds = (foc_entry.config.min_position, foc_entry.config.max_position);

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
        // Same walk order as the capture sweep: every sample reached
        // from the focuser's backlash approach side.
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
                // median — the capture sweep's star-count
                // weighting, one metric over: a position where
                // most frames were invalid contributes a noisier
                // median and should pull the fit less.
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
        let confirmation = imaging::tools::auto_focus::HfrSample {
            hfr: sample,
            star_count: frames_used,
        };
        let confirmed = imaging::tools::auto_focus::confirmation_accepted(
            &confirmation,
            0.0,
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

/// Fill sweep parameters the call omitted from the train's
/// `auto_focus` config block — per-call values win field by field.
/// Resolve a capture sweep's per-call parameters into the binning its
/// frames are taken at and the sweep itself. The train-addressed path
/// has already merged the train's `auto_focus` block underneath, so a
/// field still missing here is missing per call and per train alike.
///
/// Presence is validated in input order, so the error always names the
/// first missing field — the same convention as `measure_basic`. The
/// block-driven analogue a `refocus_train` step takes is
/// [`af_params_from_block`].
fn capture_sweep_from_params(
    params: &AutoFocusToolParams,
) -> Result<
    (
        rp_vocabulary::Binning,
        imaging::tools::auto_focus::AutoFocusParams,
    ),
    Box<CallToolResult>,
> {
    let Some(duration) = params.duration else {
        return Err(Box::new(tool_error!(
            "missing required parameter: duration"
        )));
    };
    let Some(step_size) = params.step_size else {
        return Err(Box::new(tool_error!(
            "missing required parameter: step_size"
        )));
    };
    let Some(half_width) = params.half_width else {
        return Err(Box::new(tool_error!(
            "missing required parameter: half_width"
        )));
    };
    let Some(min_area) = params.min_area else {
        return Err(Box::new(tool_error!(
            "missing required parameter: min_area"
        )));
    };
    let Some(max_area) = params.max_area else {
        return Err(Box::new(tool_error!(
            "missing required parameter: max_area"
        )));
    };

    Ok((
        params.binning.unwrap_or(DEFAULT_BINNING),
        imaging::tools::auto_focus::AutoFocusParams {
            duration,
            step_size,
            half_width,
            min_area,
            max_area,
            threshold_sigma: params.threshold_sigma.unwrap_or(5.0),
            min_fit_points: params.min_fit_points.unwrap_or(5),
            // Overridden from the focuser's backlash block once the
            // focuser is resolved inside `run_auto_focus_step`.
            direction: imaging::tools::auto_focus::SweepDirection::Ascending,
            min_star_fraction: params
                .min_star_fraction
                .unwrap_or(imaging::tools::auto_focus::DEFAULT_MIN_STAR_FRACTION),
            confirmation_tolerance: params
                .confirmation_tolerance
                .unwrap_or(imaging::tools::auto_focus::DEFAULT_CONFIRMATION_TOLERANCE),
            max_attempts: params
                .max_attempts
                .unwrap_or(imaging::tools::auto_focus::DEFAULT_MAX_ATTEMPTS),
        },
    ))
}

fn merge_block_into_params(params: &mut AutoFocusToolParams, block: &TrainAutoFocusConfig) {
    params.duration = params.duration.or(block.duration);
    params.step_size = params.step_size.or_else(|| Some(block.step_size.value()));
    params.half_width = params.half_width.or_else(|| Some(block.half_width.value()));
    params.binning = params.binning.or_else(|| {
        block
            .binning
            .map(crate::config::optical_train::SweepBinning::value)
    });
    params.min_area = params.min_area.or(block.min_area);
    params.max_area = params.max_area.or(block.max_area);
    params.threshold_sigma = params.threshold_sigma.or(block.threshold_sigma);
    params.min_fit_points = params.min_fit_points.or(block.min_fit_points);
    params.min_star_fraction = params.min_star_fraction.or_else(|| {
        block
            .min_star_fraction
            .map(crate::config::optical_train::MinStarFraction::value)
    });
    params.confirmation_tolerance = params.confirmation_tolerance.or_else(|| {
        block
            .confirmation_tolerance
            .map(crate::config::optical_train::ConfirmationTolerance::value)
    });
    params.max_attempts = params.max_attempts.or_else(|| {
        block
            .max_attempts
            .map(crate::config::optical_train::MaxAttempts::value)
    });
}

/// The capture-sweep parameter set from an imaging train's
/// `auto_focus` block — what a `refocus_train` capture step runs
/// with. The capture fields are load-validated as present on imaging
/// trains, so a `None` here means the model and config drifted; the
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

/// error names the train rather than panicking.
fn af_params_from_block(
    train_id: &str,
    block: &TrainAutoFocusConfig,
) -> Result<imaging::tools::auto_focus::AutoFocusParams, String> {
    let missing = |field: &str| format!("train '{train_id}' auto_focus block is missing {field}");
    Ok(imaging::tools::auto_focus::AutoFocusParams {
        duration: block.duration.ok_or_else(|| missing("duration"))?,
        step_size: block.step_size.value(),
        half_width: block.half_width.value(),
        min_area: block.min_area.ok_or_else(|| missing("min_area"))?,
        max_area: block.max_area.ok_or_else(|| missing("max_area"))?,
        threshold_sigma: block.threshold_sigma.unwrap_or(5.0),
        min_fit_points: block.min_fit_points.unwrap_or(5),
        // Overridden from the focuser's backlash block once the focuser
        // is resolved inside `run_auto_focus_step`.
        direction: imaging::tools::auto_focus::SweepDirection::Ascending,
        min_star_fraction: block.min_star_fraction.map_or(
            imaging::tools::auto_focus::DEFAULT_MIN_STAR_FRACTION,
            crate::config::optical_train::MinStarFraction::value,
        ),
        confirmation_tolerance: block.confirmation_tolerance.map_or(
            imaging::tools::auto_focus::DEFAULT_CONFIRMATION_TOLERANCE,
            crate::config::optical_train::ConfirmationTolerance::value,
        ),
        max_attempts: block.max_attempts.map_or(
            imaging::tools::auto_focus::DEFAULT_MAX_ATTEMPTS,
            crate::config::optical_train::MaxAttempts::value,
        ),
    })
}

/// The metric-sweep geometry from the guiding train's `auto_focus`
/// block — what a `refocus_train` metric step runs with.
fn guide_sweep_from_block(block: &TrainAutoFocusConfig) -> GuideSweepParams {
    GuideSweepParams {
        step_size: block.step_size.value(),
        half_width: block.half_width.value(),
        frames_per_step: block
            .frames_per_step
            .map_or(3, crate::config::optical_train::FramesPerStep::value),
        min_fit_points: block.min_fit_points.unwrap_or(5),
        confirmation_tolerance: block.confirmation_tolerance.map_or(
            imaging::tools::auto_focus::DEFAULT_CONFIRMATION_TOLERANCE,
            crate::config::optical_train::ConfirmationTolerance::value,
        ),
    }
}

/// Adapter that satisfies all three [`auto_focus`] traits
/// (`FocuserOps`, `CaptureOps`, `MeasureOps`) by delegating to the
/// existing [`McpHandler`] helpers (`do_move_focuser_blocking`,
/// `do_capture`, `measure_via_document` + cache `put_section`).
///
/// Keeps the compound tool's wiring close to the corresponding
/// primitive tools: same bounds-check / poll semantics on focuser
/// motion, same FITS write / cache insert / event emission on
/// capture, same `image_analysis` section persistence on measure.
///
/// `progress` carries the per-request `ProgressSink` (or `None` when
/// the client did not supply a `progressToken`) so each inner
/// blocking helper emits `notifications/progress` against the same
/// token used by the compound tool's own call.
pub(crate) struct AutoFocusAdapter<'a> {
    pub(crate) handler: &'a McpHandler,
    pub(crate) camera_id: String,
    pub(crate) focuser_id: String,
    /// The binning every sweep frame is captured at — resolved once
    /// for the run, since a V-curve is only comparable frame to frame.
    pub(crate) binning: rp_vocabulary::Binning,
    pub(crate) progress: Option<ProgressSink>,
    /// The compound call's cancel handle, threaded into every inner
    /// helper so a cancelled sweep stops at its current step.
    pub(crate) cancel: Cancel,
}

impl AutoFocusAdapter<'_> {
    fn emitter(&self) -> Option<&dyn ProgressEmitter> {
        self.progress.as_ref().map(ProgressSink::as_emitter)
    }
}

#[async_trait::async_trait]
impl imaging::tools::auto_focus::FocuserOps for AutoFocusAdapter<'_> {
    async fn move_to(&self, position: i32) -> std::result::Result<i32, String> {
        self.handler
            .do_move_focuser_blocking(&self.focuser_id, position, self.emitter(), &self.cancel)
            .await
            .map(|outcome| outcome.position)
    }
}

#[async_trait::async_trait]
impl imaging::tools::auto_focus::CaptureOps for AutoFocusAdapter<'_> {
    async fn capture(&self, duration: Duration) -> std::result::Result<String, String> {
        let (_image_path, document_id) = self
            .handler
            .do_capture(
                CaptureRequest {
                    camera_id: &self.camera_id,
                    duration,
                    binning: self.binning,
                    target: None,
                    frame_type: None,
                },
                self.emitter(),
                &self.cancel,
            )
            .await?;
        Ok(document_id)
    }
}

#[async_trait::async_trait]
impl imaging::tools::auto_focus::MeasureOps for AutoFocusAdapter<'_> {
    async fn measure(
        &self,
        document_id: &str,
        min_area: usize,
        max_area: usize,
        threshold_sigma: f64,
    ) -> std::result::Result<imaging::tools::auto_focus::HfrSample, String> {
        let resolved = ResolvedParams {
            threshold_sigma,
            min_area,
            max_area,
        };
        let result = self
            .handler
            .measure_via_document(document_id, &resolved)
            .await
            .map_err(|e| e.to_string())?;
        // Persist the per-frame `image_analysis` section, matching the
        // standalone `measure_basic` tool's side effect — auto_focus is
        // explicitly composed of measure_basic calls per the contract.
        let value = serde_json::to_value(&result).unwrap_or(serde_json::Value::Null);
        if let Err(e) = self
            .handler
            .image_cache
            .put_section(document_id, "image_analysis", value)
            .await
        {
            debug!(error = %e, document_id, "failed to persist image_analysis section");
        }
        Ok(imaging::tools::auto_focus::HfrSample {
            hfr: result.hfr,
            star_count: result.star_count,
        })
    }
}
