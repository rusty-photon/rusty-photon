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
use super::super::internals::ResolvedParams;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{tool_error, tool_success};
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
    /// Per-frame `measure_basic` threshold (sigma units). Default 5.0.
    #[serde(default)]
    pub threshold_sigma: Option<f64>,
    /// Minimum number of non-null HFR samples for the parabolic fit.
    /// Default 5.
    #[serde(default)]
    pub min_fit_points: Option<usize>,
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
        description = "V-curve auto-focus: sweep ± half_width around the focuser's current position, capture and run measure_basic at each step, fit a parabola in HFR, and move the focuser to the fitted minimum. Address the devices as camera_id + focuser_id, or as train_id (the train's terminal camera + focuser, sweep parameters falling back to the train's auto_focus config block)."
    )]
    pub(crate) async fn auto_focus(
        &self,
        Parameters(params): Parameters<AutoFocusToolParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let progress_sink = ProgressSink::from_request_context(&ctx);
        self.auto_focus_inner(params, progress_sink).await
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
        self.refocus_train_inner(params, progress_sink).await
    }

    /// Body of the `auto_focus` MCP tool, split out so unit tests can
    /// pass `None` for the progress sink without constructing a real
    /// rmcp `Peer` (its constructor is `pub(crate)` in rmcp 1.7).
    pub(crate) async fn auto_focus_inner(
        &self,
        mut params: AutoFocusToolParams,
        progress_sink: Option<ProgressSink>,
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
                return self.guide_af_tool(&train_id, params, progress_sink).await;
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

        let duration = match params.duration {
            Some(d) => d,
            None => return Ok(tool_error!("missing required parameter: duration")),
        };
        let step_size = match params.step_size {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: step_size")),
        };
        let half_width = match params.half_width {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: half_width")),
        };
        let min_area = match params.min_area {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: min_area")),
        };
        let max_area = match params.max_area {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: max_area")),
        };

        let af_params = imaging::tools::auto_focus::AutoFocusParams {
            duration,
            step_size,
            half_width,
            min_area,
            max_area,
            threshold_sigma: params.threshold_sigma.unwrap_or(5.0),
            min_fit_points: params.min_fit_points.unwrap_or(5),
        };

        match self
            .run_auto_focus_step(&camera_id, &focuser_id, af_params, progress_sink)
            .await
        {
            Ok(result) => {
                let curve_points =
                    serde_json::to_value(&result.curve_points).unwrap_or(serde_json::Value::Null);
                Ok(tool_success!({
                    "best_position": result.best_position,
                    "best_hfr": result.best_hfr,
                    "final_position": result.final_position,
                    "samples_used": result.samples_used,
                    "curve_points": curve_points,
                    "temperature_c": result.temperature_c,
                }))
            }
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    /// Body of the `refocus_train` MCP tool — see `auto_focus_inner`
    /// for why the split exists.
    pub(crate) async fn refocus_train_inner(
        &self,
        params: RefocusTrainParams,
        progress_sink: Option<ProgressSink>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(train_id) = params.train_id else {
            return Ok(tool_error!("missing required parameter: train_id"));
        };
        let reason = params.reason.unwrap_or_else(|| "manual".to_string());

        // Expansion + per-step resolution, all before any motion or
        // event: an invalid expansion never touches hardware.
        if self.trains.train(&train_id).is_none() {
            return Ok(tool_error!("train not found: {}", train_id));
        }
        let steps = self.trains.af_sequence(&train_id).unwrap_or_default();
        if steps.is_empty() {
            return Ok(tool_error!(
                "refocus_train: train '{}' has no focusers",
                train_id
            ));
        }
        let mut planned = Vec::with_capacity(steps.len());
        for step in &steps {
            let Some(run_train) = self.trains.train(&step.train_id) else {
                return Ok(tool_error!("train not found: {}", step.train_id));
            };
            let Some(block) = &run_train.auto_focus else {
                return Ok(tool_error!(
                    "refocus_train: train '{}' has no auto_focus config block \
                     (required for the step focusing '{}')",
                    step.train_id,
                    step.focuser_id
                ));
            };
            if run_train.purpose == TrainPurpose::Guiding {
                planned.push(PlannedStep::Metric {
                    focuser_id: step.focuser_id.clone(),
                    train_id: step.train_id.clone(),
                    sweep: guide_sweep_from_block(block),
                });
            } else {
                let Some(camera_id) = run_train.camera_id() else {
                    return Ok(tool_error!("train '{}' has no camera", step.train_id));
                };
                let af_params = match af_params_from_block(&step.train_id, block) {
                    Ok(p) => p,
                    Err(e) => return Ok(tool_error!("refocus_train: {}", e)),
                };
                planned.push(PlannedStep::Capture {
                    focuser_id: step.focuser_id.clone(),
                    train_id: step.train_id.clone(),
                    camera_id: camera_id.to_string(),
                    af_params,
                });
            }
        }

        // A metric step reads PHD2's GuideStep stream, so the whole
        // expansion is refused before any motion when guiding is not
        // active — unlike the pause handshake below, which degrades
        // gracefully (Tenet 2).
        let has_metric = planned
            .iter()
            .any(|s| matches!(s, PlannedStep::Metric { .. }));
        let metric_client = if has_metric {
            match self.require_active_guiding("guide-train step").await {
                Ok(client) => Some(client),
                Err(e) => return Ok(tool_error!("refocus_train: {}", e)),
            }
        } else {
            None
        };

        // Guiding handshake decision (rp.md §`refocus_train` Contract):
        // pause only when a *capture-based* step moves a guiding-train
        // focuser AND the guider is configured AND it reports an
        // active loop. A stats read that fails or reports not-guiding
        // skips the handshake — a broken guider service must not block
        // a refocus. Metric steps run under active corrections; the
        // execution loop resumes before the first one.
        let guiding_members: HashSet<&str> = self
            .trains
            .guiding_train()
            .map(|t| t.devices.iter().map(|d| d.id.as_str()).collect())
            .unwrap_or_default();
        let touches_guiding = planned.iter().any(|s| {
            matches!(s, PlannedStep::Capture { .. }) && guiding_members.contains(s.focuser_id())
        });
        let mut pause_client = None;
        if touches_guiding {
            if let Some(client) = self.guider.clone() {
                match client.guiding_stats().await {
                    Ok(stats) if stats.guiding => pause_client = Some(client),
                    Ok(_) => {
                        debug!(
                            train_id,
                            "guider reports not guiding; skipping pause handshake"
                        );
                    }
                    Err(e) => {
                        debug!(train_id, error = %e, "guider stats unreachable; skipping pause handshake");
                    }
                }
            }
        }
        let guiding_paused = pause_client.is_some();

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus.emit_operation(EventEnvelope::started(
            "refocus",
            &operation_id,
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

        if let Some(client) = &pause_client {
            if let Err(e) = client.pause_guiding(false).await {
                let msg = format!("refocus_train: failed to pause guiding: {e}");
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "refocus",
                    &operation_id,
                    started_at,
                    &msg,
                ));
                return Ok(tool_error!("{}", msg));
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
                        self.event_bus.emit_operation(EventEnvelope::failed(
                            "refocus",
                            &operation_id,
                            started_at,
                            &msg,
                        ));
                        return Ok(tool_error!("{}", msg));
                    }
                }
                paused = false;
            }
            let step_result = match step {
                PlannedStep::Capture {
                    camera_id,
                    focuser_id,
                    train_id: run_train,
                    af_params,
                } => self
                    .run_auto_focus_step(
                        camera_id,
                        focuser_id,
                        af_params.clone(),
                        progress_sink.clone(),
                    )
                    .await
                    .map(|result| {
                        serde_json::json!({
                            "focuser_id": focuser_id,
                            "train_id": run_train,
                            "camera_id": camera_id,
                            "best_position": result.best_position,
                            "best_hfr": result.best_hfr,
                            "samples_used": result.samples_used,
                        })
                    }),
                PlannedStep::Metric {
                    focuser_id,
                    train_id: run_train,
                    sweep,
                } => match &metric_client {
                    Some(client) => self
                        .run_guide_af_sweep(
                            run_train,
                            focuser_id,
                            sweep,
                            client.clone(),
                            progress_sink.clone(),
                        )
                        .await
                        .map(|outcome| {
                            serde_json::json!({
                                "focuser_id": focuser_id,
                                "train_id": run_train,
                                "camera_id": serde_json::Value::Null,
                                "best_position": outcome.best_position,
                                "best_hfd": outcome.best_hfd,
                                "samples_used": outcome.samples_used,
                            })
                        }),
                    None => Err("guide-train step planned without an active guider".to_string()),
                },
            };
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
                    self.event_bus.emit_operation(EventEnvelope::failed(
                        "refocus",
                        &operation_id,
                        started_at,
                        &msg,
                    ));
                    return Ok(tool_error!("{}", msg));
                }
            }
        }

        if paused {
            if let Some(client) = &pause_client {
                if let Err(e) = client.resume_guiding().await {
                    let msg = format!(
                        "refocus_train: all steps completed but resuming guiding failed: {e}"
                    );
                    self.event_bus.emit_operation(EventEnvelope::failed(
                        "refocus",
                        &operation_id,
                        started_at,
                        &msg,
                    ));
                    return Ok(tool_error!("{}", msg));
                }
            }
        }

        self.event_bus.emit_operation(EventEnvelope::complete(
            "refocus",
            &operation_id,
            started_at,
            serde_json::json!({
                "train_id": train_id,
                "steps": completed,
            }),
        ));
        Ok(tool_success!({
            "train_id": train_id,
            "reason": reason,
            "guiding_paused": guiding_paused,
            "steps": completed,
        }))
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
        af_params: imaging::tools::auto_focus::AutoFocusParams,
        progress_sink: Option<ProgressSink>,
    ) -> Result<imaging::tools::auto_focus::AutoFocusResult, String> {
        // Resolve devices early — the standard "<kind> not found" /
        // "<kind> not connected" errors, camera before focuser to
        // match input order in the contract. The camera is resolved
        // purely for the connection check; `do_capture` re-resolves.
        let cam_entry = self
            .equipment
            .find_camera(camera_id)
            .ok_or_else(|| format!("camera not found: {camera_id}"))?;
        if cam_entry.device.is_none() {
            return Err(format!("camera not connected: {camera_id}"));
        }
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device
            .clone()
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

        // Store the per-request sink on the adapter so every
        // inner `do_capture` / `do_move_focuser_blocking` call emits
        // progress through the same `progressToken`. See
        // `mcp::progress` for the rmcp 300 s session keep-alive race
        // this guards against.
        let adapter = AutoFocusAdapter {
            handler: self,
            camera_id: camera_id.to_string(),
            focuser_id: focuser_id.to_string(),
            progress: progress_sink,
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
                        "position": result.best_position,
                        "hfr": result.best_hfr,
                        "samples_used": result.samples_used,
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
}

/// Result of a guide-train metric sweep — the metric-side analogue of
/// `AutoFocusResult`, with HFD samples and no capture documents.
struct GuideAfOutcome {
    best_position: i32,
    best_hfd: f64,
    final_position: i32,
    samples_used: usize,
    curve_points: Vec<serde_json::Value>,
    temperature_c: Option<f64>,
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
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Capture-only parameters cannot influence a metric sweep;
        // reject rather than silently ignore.
        if params.duration.is_some()
            || params.min_area.is_some()
            || params.max_area.is_some()
            || params.threshold_sigma.is_some()
        {
            return Ok(tool_error!(
                "auto_focus: duration, min_area, max_area, and threshold_sigma apply only to \
                 capture-based sweeps (train '{}' is the guiding train)",
                train_id
            ));
        }
        let train = match self.trains.train(train_id) {
            Some(t) => t,
            None => return Ok(tool_error!("train not found: {}", train_id)),
        };
        let Some(focuser_id) = train.terminal_focuser().map(str::to_string) else {
            return Ok(tool_error!("train '{}' has no focuser", train_id));
        };
        let block = train.auto_focus.as_ref();
        let step_size = match params
            .step_size
            .or_else(|| block.map(|b| b.step_size.value()))
        {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: step_size")),
        };
        let half_width = match params
            .half_width
            .or_else(|| block.map(|b| b.half_width.value()))
        {
            Some(s) => s,
            None => return Ok(tool_error!("missing required parameter: half_width")),
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
        };

        let client = match self.require_active_guiding("guide-train auto_focus").await {
            Ok(c) => c,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        match self
            .run_guide_af_sweep(train_id, &focuser_id, &sweep, client, progress_sink)
            .await
        {
            Ok(outcome) => Ok(tool_success!({
                "best_position": outcome.best_position,
                "best_hfd": outcome.best_hfd,
                "final_position": outcome.final_position,
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
    ) -> Result<GuideAfOutcome, String> {
        let foc_entry = self
            .equipment
            .find_focuser(focuser_id)
            .ok_or_else(|| format!("focuser not found: {focuser_id}"))?;
        let foc = foc_entry
            .device
            .clone()
            .ok_or_else(|| format!("focuser not connected: {focuser_id}"))?;
        let bounds = (foc_entry.config.min_position, foc_entry.config.max_position);

        let starting_position = foc
            .position()
            .await
            .map_err(|e| format!("failed to read focuser position: {e}"))?;
        let temperature_c: Option<f64> = foc.temperature().await.ok();

        let grid = imaging::tools::auto_focus::build_grid(
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
        let result: Result<GuideAfOutcome, String> = async {
            let mut watermark = 0;
            let mut curve_points = Vec::with_capacity(grid.len());
            let mut fit_samples: Vec<(i32, f64, u32)> = Vec::new();
            for &position in &grid {
                self.do_move_focuser_blocking(focuser_id, position, emitter)
                    .await?;
                // Watermark from the ring *after* the move settles, so
                // frames exposed during the focuser motion — at a
                // stale focus — never count toward this position.
                // Best-effort: on a failed read the previous
                // position's high-water mark still guards staleness,
                // and the collect loop below surfaces a persistent
                // metrics failure as its own error.
                watermark =
                    latest_frame(client.guiding_metrics().await.ok().as_ref()).max(watermark);
                let (sample, frames_used, max_frame) = self
                    .collect_guide_sample(client.as_ref(), watermark, sweep.frames_per_step)
                    .await?;
                watermark = max_frame.max(watermark);
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

            if fit_samples.len() < sweep.min_fit_points {
                return Err(format!(
                    "not enough valid guide samples: {} of {} positions produced an HFD, \
                     min_fit_points is {}",
                    fit_samples.len(),
                    grid.len(),
                    sweep.min_fit_points
                ));
            }
            let fit = imaging::tools::auto_focus::fit_parabola(&fit_samples)
                .map_err(|e| e.to_string())?;
            let best_position = fit.vertex_position();
            // The min-fit-points check above guarantees a non-empty grid.
            let (Some(&grid_min), Some(&grid_max)) = (grid.first(), grid.last()) else {
                return Err("monotonic curve: empty sample grid".to_string());
            };
            if best_position < grid_min || best_position > grid_max {
                return Err(format!(
                    "monotonic curve: fitted minimum {best_position} lies outside the sampled \
                     range [{grid_min}, {grid_max}]"
                ));
            }
            let final_position = self
                .do_move_focuser_blocking(focuser_id, best_position, emitter)
                .await?;
            Ok(GuideAfOutcome {
                best_position,
                best_hfd: fit.vertex_value(),
                final_position,
                samples_used: fit_samples.len(),
                curve_points,
                temperature_c,
            })
        }
        .await;

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
                        "position": outcome.best_position,
                        "hfd": outcome.best_hfd,
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
            tokio::time::sleep(GUIDE_METRICS_POLL).await;
        }
    }
}

/// The highest frame number in a metrics window, 0 when absent — the
/// initial freshness watermark.
fn latest_frame(metrics: Option<&rp_guider::GuidingMetrics>) -> u64 {
    metrics.map_or(0, |m| m.frames.iter().map(|f| f.frame).max().unwrap_or(0))
}

/// Fill sweep parameters the call omitted from the train's
/// `auto_focus` config block — per-call values win field by field.
fn merge_block_into_params(params: &mut AutoFocusToolParams, block: &TrainAutoFocusConfig) {
    params.duration = params.duration.or(block.duration);
    params.step_size = params.step_size.or(Some(block.step_size.value()));
    params.half_width = params.half_width.or(Some(block.half_width.value()));
    params.min_area = params.min_area.or(block.min_area);
    params.max_area = params.max_area.or(block.max_area);
    params.threshold_sigma = params.threshold_sigma.or(block.threshold_sigma);
    params.min_fit_points = params.min_fit_points.or(block.min_fit_points);
}

/// The capture-sweep parameter set from an imaging train's
/// `auto_focus` block — what a `refocus_train` capture step runs
/// with. The capture fields are load-validated as present on imaging
/// trains, so a `None` here means the model and config drifted; the
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
    pub(crate) progress: Option<ProgressSink>,
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
            .do_move_focuser_blocking(&self.focuser_id, position, self.emitter())
            .await
    }
}

#[async_trait::async_trait]
impl imaging::tools::auto_focus::CaptureOps for AutoFocusAdapter<'_> {
    async fn capture(&self, duration: Duration) -> std::result::Result<String, String> {
        let (_image_path, document_id) = self
            .handler
            .do_capture(&self.camera_id, duration, None, None, self.emitter())
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
