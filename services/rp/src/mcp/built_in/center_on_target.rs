use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;

use super::super::handler::McpHandler;
use super::super::internals::DoPlateSolveInput;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{resolve_device, tool_error, tool_success};
use crate::imaging;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["camera_id"]}, {"required": ["train_id"]}]))]
pub struct CenterOnTargetToolParams {
    /// Camera that captures each iteration's frame; mutually
    /// exclusive with `train_id`.
    #[serde(default)]
    pub camera_id: Option<String>,
    /// Optical train whose terminal camera captures each iteration's
    /// frame; mutually exclusive with `camera_id`.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Target right ascension, decimal hours, [0, 24).
    #[serde(default)]
    pub ra: Option<f64>,
    /// Target declination, decimal degrees, [-90, 90].
    #[serde(default)]
    pub dec: Option<f64>,
    /// Per-iteration exposure (humantime string).
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub duration: Option<Duration>,
    /// Convergence threshold on the great-circle residual between the
    /// solved center and (ra, dec), in arcseconds.
    #[serde(default)]
    pub tolerance_arcsec: Option<f64>,
    /// Hard cap on the number of iterations. Capped at `MAX_ATTEMPTS`
    /// (50) before any motion.
    #[serde(default)]
    pub max_attempts: Option<usize>,
}

#[tool_router(router = tool_router_center_on_target, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Iteratively capture, plate-solve, sync (iter 1 only), and slew until the great-circle residual between the solved field-center and (ra, dec) is at or below tolerance_arcsec. Singular mount required. See `center_on_target` Contract in rp.md."
    )]
    pub(crate) async fn center_on_target(
        &self,
        Parameters(params): Parameters<CenterOnTargetToolParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let progress_sink = ProgressSink::from_request_context(&ctx);
        self.center_on_target_inner(params, progress_sink).await
    }

    /// Body of the `center_on_target` MCP tool, split out so unit
    /// tests can pass `None` for the progress sink without
    /// constructing a real rmcp `Peer`.
    pub(crate) async fn center_on_target_inner(
        &self,
        params: CenterOnTargetToolParams,
        progress_sink: Option<ProgressSink>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Body validation in input order so the missing-parameter
        // outline always points at the first missing field — same
        // convention as `auto_focus` / `measure_basic`.
        let camera_id = match self.resolve_camera_addressing(
            "center_on_target",
            params.camera_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let cot_params = match cot_params_from(&params) {
            Ok(cot_params) => cot_params,
            Err(e) => return Ok(*e),
        };

        // Resolve devices early so the device-resolution error
        // scenarios trip before any numeric-range or motion errors.
        let (_cam_entry, _cam) = resolve_device!(self, find_camera, &camera_id, "camera");
        // Mount resolution: same shape as `do_sync_mount` /
        // `do_slew_blocking` would surface, just hoisted here so the
        // BDD "no mount configured" / "mount not connected" scenarios
        // see the error before the loop body runs.
        if let Err(e) = self.resolve_mount() {
            return Ok(tool_error!("{}", e));
        }

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.emit_centering_started(&operation_id, started_at, &camera_id, &cot_params);

        // Store the per-request sink on the adapter so every
        // inner `do_capture` and `do_slew_blocking` call emits
        // progress through the same `progressToken`. See
        // `mcp::progress` for the rmcp 300 s session keep-alive race
        // this guards against.
        let adapter = CenterOnTargetAdapter {
            handler: self,
            camera_id: camera_id.clone(),
            progress: progress_sink,
        };

        let emit_iteration = self.centering_iteration_emitter(camera_id.clone());

        match imaging::tools::center_on_target::run_center_on_target(
            &adapter,
            &adapter,
            &adapter,
            cot_params,
            emit_iteration,
        )
        .await
        {
            Ok(result) => {
                self.event_bus
                    .emit_operation(crate::events::EventEnvelope::complete(
                        "centering",
                        &operation_id,
                        started_at,
                        serde_json::json!({
                            "camera_id": camera_id,
                            "final_error_arcsec": result.final_error_arcsec,
                            "attempts": result.attempts,
                            "final_ra": result.final_ra,
                            "final_dec": result.final_dec,
                        }),
                    ));
                let iterations =
                    serde_json::to_value(&result.iterations).unwrap_or(serde_json::Value::Null);
                Ok(tool_success!({
                    "final_error_arcsec": result.final_error_arcsec,
                    "attempts": result.attempts,
                    "final_ra": result.final_ra,
                    "final_dec": result.final_dec,
                    "iterations": iterations,
                }))
            }
            Err(e) => {
                self.event_bus
                    .emit_operation(crate::events::EventEnvelope::failed(
                        "centering",
                        &operation_id,
                        started_at,
                        &e.to_string(),
                    ));
                Ok(tool_error!("{}", e))
            }
        }
    }

    /// Emit the `centering` started envelope with its advisory
    /// outer-loop deadlines (§2.5): per-iteration slews/captures carry
    /// their own deadlines; this sizes the whole loop for the Sentinel
    /// watchdog. rp does not enforce it.
    fn emit_centering_started(
        &self,
        operation_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        camera_id: &str,
        cot: &imaging::tools::center_on_target::CenterOnTargetParams,
    ) {
        let (predicted_ms, max_ms) = super::super::internals::centering_deadlines(
            cot.max_attempts,
            cot.duration,
            self.centering.solve_time_estimate,
            self.centering.slew_overhead_estimate,
        );
        self.event_bus.emit_operation(
            crate::events::EventEnvelope::started(
                "centering",
                operation_id,
                started_at,
                serde_json::json!({
                    "camera_id": camera_id,
                    "ra": cot.ra,
                    "dec": cot.dec,
                    "tolerance_arcsec": cot.tolerance_arcsec,
                    "max_attempts": cot.max_attempts,
                }),
            )
            .with_deadlines(predicted_ms, max_ms),
        );
    }

    /// The `centering_iteration` event emitter threaded into the
    /// iteration loop.
    fn centering_iteration_emitter(
        &self,
        camera_id: String,
    ) -> impl Fn(&imaging::tools::center_on_target::IterationRecord) {
        let event_bus = self.event_bus.clone();
        move |rec: &imaging::tools::center_on_target::IterationRecord| {
            let action = serde_json::to_value(rec.action).unwrap_or(serde_json::Value::Null);
            event_bus.emit(
                "centering_iteration",
                serde_json::json!({
                    "camera_id": camera_id,
                    "document_id": rec.document_id,
                    "residual_arcsec": rec.residual_arcsec,
                    "solved_ra": rec.solved_ra,
                    "solved_dec": rec.solved_dec,
                    "action": action,
                }),
            );
        }
    }
}

/// Body validation for the coordinate/exposure parameters, in input
/// order so the missing-parameter outline always points at the first
/// missing field.
fn cot_params_from(
    params: &CenterOnTargetToolParams,
) -> Result<imaging::tools::center_on_target::CenterOnTargetParams, Box<CallToolResult>> {
    let Some(ra) = params.ra else {
        return Err(Box::new(tool_error!("missing required parameter: ra")));
    };
    let Some(dec) = params.dec else {
        return Err(Box::new(tool_error!("missing required parameter: dec")));
    };
    let Some(duration) = params.duration else {
        return Err(Box::new(tool_error!(
            "missing required parameter: duration"
        )));
    };
    let Some(tolerance_arcsec) = params.tolerance_arcsec else {
        return Err(Box::new(tool_error!(
            "missing required parameter: tolerance_arcsec"
        )));
    };
    let Some(max_attempts) = params.max_attempts else {
        return Err(Box::new(tool_error!(
            "missing required parameter: max_attempts"
        )));
    };
    Ok(imaging::tools::center_on_target::CenterOnTargetParams {
        ra,
        dec,
        duration,
        tolerance_arcsec,
        max_attempts,
    })
}

/// Adapter satisfying the three [`center_on_target`] traits
/// (`CaptureOps`, `PlateSolveOps`, `MountOps`) by delegating to the
/// existing [`McpHandler`] helpers.
///
/// `PlateSolveOps` calls back into the in-process `plate_solve`
/// handler with `use_mount_hints: true`, so the hours→degrees
/// conversion lives in exactly one place (the `plate_solve` Contract).
/// `MountOps::sync_to` calls `do_sync_mount` after dividing the
/// solved degrees by 15 to match Alpaca's RA-in-hours convention.
/// `MountOps::slew_to` calls `do_slew_blocking` with the operator-
/// configured `settle_after_slew` so iteration cadence honours
/// rig-specific mechanical settle.
pub(crate) struct CenterOnTargetAdapter<'a> {
    pub(crate) handler: &'a McpHandler,
    pub(crate) camera_id: String,
    /// Per-request progress sink (or `None` when the client did not
    /// supply a `progressToken`). Threaded into every `do_capture` /
    /// `do_slew_blocking` call below so the inner poll loops emit
    /// `notifications/progress` against the compound tool's own
    /// session.
    pub(crate) progress: Option<ProgressSink>,
}

impl CenterOnTargetAdapter<'_> {
    fn emitter(&self) -> Option<&dyn ProgressEmitter> {
        self.progress.as_ref().map(ProgressSink::as_emitter)
    }
}

#[async_trait::async_trait]
impl imaging::tools::center_on_target::CaptureOps for CenterOnTargetAdapter<'_> {
    async fn capture(&self, duration: Duration) -> std::result::Result<String, String> {
        let (_image_path, document_id) = self
            .handler
            .do_capture(&self.camera_id, duration, None, None, self.emitter())
            .await?;
        Ok(document_id)
    }
}

#[async_trait::async_trait]
impl imaging::tools::center_on_target::PlateSolveOps for CenterOnTargetAdapter<'_> {
    async fn solve(
        &self,
        document_id: &str,
    ) -> std::result::Result<imaging::tools::center_on_target::SolveOutcome, String> {
        // Same in-process body the standalone `plate_solve` MCP
        // tool uses (configured-check, document resolution, hint
        // sourcing, request build, error mapping, `wcs`
        // persistence) — both go through `do_plate_solve` so
        // future changes to defaults / validation / persistence
        // land in one place. center_on_target hardcodes
        // `pointing_hint: None, use_mount_hints: true` so the
        // hours→degrees conversion stays in `do_plate_solve`'s
        // single mount-read path, matching the `plate_solve`
        // Contract verbatim.
        let input = DoPlateSolveInput {
            document_id: Some(document_id),
            image_path: None,
            pointing_hint: None,
            use_mount_hints: true,
            fov_hint_deg: None,
            search_radius_deg: None,
            timeout: None,
        };
        let out = self.handler.do_plate_solve(input).await?;
        Ok(imaging::tools::center_on_target::SolveOutcome {
            ra_center_deg: out.ra_center,
            dec_center_deg: out.dec_center,
        })
    }
}

#[async_trait::async_trait]
impl imaging::tools::center_on_target::MountOps for CenterOnTargetAdapter<'_> {
    async fn sync_to(&self, ra_deg: f64, dec_deg: f64) -> std::result::Result<(), String> {
        // The driver works in degrees; Alpaca's RA is hours.
        let ra_hours = ra_deg / 15.0;
        self.handler.do_sync_mount(ra_hours, dec_deg).await
    }
    async fn slew_to(&self, ra_hours: f64, dec_deg: f64) -> std::result::Result<(), String> {
        let settle = self
            .handler
            .equipment
            .find_mount()
            .and_then(|m| m.config.settle_after_slew)
            .unwrap_or_default();
        self.handler
            .do_slew_blocking(ra_hours, dec_deg, settle, self.emitter())
            .await
            .map(|_| ())
    }
}
