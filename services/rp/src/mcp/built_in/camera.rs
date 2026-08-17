use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{resolve_device, tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["camera_id"]}, {"required": ["train_id"]}]))]
pub struct CaptureParams {
    /// Camera device ID; mutually exclusive with `train_id`.
    #[serde(default)]
    pub camera_id: Option<String>,
    /// Optical train whose terminal camera captures; mutually
    /// exclusive with `camera_id`.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Exposure time as a humantime string (e.g. `"500ms"`, `"30s"`, `"1m30s"`).
    #[serde(with = "humantime_serde")]
    #[schemars(with = "String")]
    pub duration: Duration,
    /// Sky-target slug this capture belongs to (Decision 11). Required
    /// when `frame_type` is `Light`; optional for `Dark`/`Flat`/`Bias`
    /// (falls back to a reserved slug when omitted); ignored when
    /// `frame_type` is omitted. See docs/services/rp.md § Capture Tool
    /// Details.
    #[serde(default)]
    pub target: Option<String>,
    /// This capture's intent. Omitted (the default) keeps today's flat
    /// `<doc_uuid_8>.fits` behavior — `target` is then ignored and
    /// nothing is denormalized onto the exposure document. Supplying
    /// it requires `session.file_naming_pattern` to be configured and
    /// activates directory/file rendering per docs/services/rp.md §
    /// Capture Tool Details.
    #[serde(default)]
    pub frame_type: Option<rp_vocabulary::FrameType>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CameraIdParams {
    pub camera_id: String,
}

#[tool_router(router = tool_router_camera, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Capture an image, download image_array, save FITS file. Optional \
                        target (slug) + frame_type (Light/Dark/Flat/Bias) link the frame to \
                        the target store and render session.directory_pattern/ \
                        file_naming_pattern into the final path (Decision 11) — omit both to \
                        keep the flat <doc_uuid_8>.fits behavior"
    )]
    pub(crate) async fn capture(
        &self,
        Parameters(params): Parameters<CaptureParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // ProgressSink is `None` when the client did not supply a
        // `progressToken` in `_meta` — `do_capture` then treats the
        // emission as a no-op. See `mcp::progress` for the rmcp
        // 300 s session keep-alive race this guards against.
        let sink = ProgressSink::from_request_context(&ctx);
        let emitter = sink.as_ref().map(ProgressSink::as_emitter);
        self.capture_inner(params, emitter).await
    }

    /// Body of the `capture` MCP tool, split out so unit tests can
    /// pass `None` for the progress emitter without constructing a
    /// real rmcp `Peer`.
    pub(crate) async fn capture_inner(
        &self,
        params: CaptureParams,
        progress: Option<&dyn ProgressEmitter>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let camera_id = match self.resolve_camera_addressing(
            "capture",
            params.camera_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        match self
            .do_capture(
                &camera_id,
                params.duration,
                params.target.as_deref(),
                params.frame_type,
                progress,
            )
            .await
        {
            Ok((image_path, document_id)) => Ok(tool_success!({
                "image_path": image_path,
                "document_id": document_id,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(description = "Read camera capabilities: max_adu, exposure limits, sensor dimensions")]
    pub(crate) async fn get_camera_info(
        &self,
        Parameters(params): Parameters<CameraIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (cam_entry, cam) = resolve_device!(self, find_camera, &params.camera_id, "camera");

        // `max_adu` and the sensor dimensions are invariant physical-sensor
        // properties cached on `CameraEntry` at connect time — no live
        // Alpaca round-trip per call. `None` here means the connect-time
        // read failed; surface as a tool_error so consumers can't mistake
        // an absent value for a successful zero. `bin` and `exposure_range`
        // stay live: binning is operator-mutable and exposure range can
        // shift on driver reconfig, so neither belongs in the connect-time
        // cache.
        let Some(max_adu) = cam_entry.max_adu else {
            return Ok(tool_error!(
                "max_adu unavailable for this camera (connect-time read failed)"
            ));
        };

        let (Some(sensor_x), Some(sensor_y)) =
            (cam_entry.sensor_width_px, cam_entry.sensor_height_px)
        else {
            return Ok(tool_error!(
                "sensor size unavailable for this camera (connect-time read failed)"
            ));
        };

        let (bin_x, bin_y) = match cam.bin().await {
            Ok(bin) => (u32::from(bin[0]), u32::from(bin[1])),
            Err(e) => {
                debug!(error = %e, "failed to read binning, using defaults");
                (1u32, 1u32)
            }
        };

        let (exposure_min, exposure_max) = match cam.exposure_range().await {
            Ok(range) => (*range.start(), *range.end()),
            Err(e) => {
                debug!(error = %e, "failed to read exposure range, using defaults");
                (Duration::from_millis(1), Duration::from_hours(1))
            }
        };

        Ok(tool_success!({
            "camera_id": params.camera_id,
            "max_adu": max_adu,
            "sensor_x": sensor_x,
            "sensor_y": sensor_y,
            "bin_x": bin_x,
            "bin_y": bin_y,
            "exposure_min": humantime::format_duration(exposure_min).to_string(),
            "exposure_max": humantime::format_duration(exposure_max).to_string(),
        }))
    }
}

impl McpHandler {
    /// Resolve the `camera_id` / `train_id` addressing shared by
    /// `capture` and `center_on_target`: exactly one must be present,
    /// and `train_id` resolves the train's terminal camera. Returns
    /// the resolved roster id, or the ready-to-return error
    /// `CallToolResult` (boxed — `clippy::result_large_err`).
    pub(crate) fn resolve_camera_addressing(
        &self,
        tool: &str,
        camera_id: Option<&str>,
        train_id: Option<&str>,
    ) -> Result<String, Box<CallToolResult>> {
        match (camera_id, train_id) {
            (Some(_), Some(_)) => Err(Box::new(tool_error!(
                "{}: train_id is mutually exclusive with camera_id",
                tool
            ))),
            (None, None) => Err(Box::new(tool_error!(
                "{}: pass exactly one of camera_id or train_id",
                tool
            ))),
            (Some(id), None) => Ok(id.to_string()),
            (None, Some(train_id)) => {
                let Some(train) = self.trains.train(train_id) else {
                    return Err(Box::new(tool_error!("train not found: {}", train_id)));
                };
                train.camera_id().map_or_else(
                    || Err(Box::new(tool_error!("train '{}' has no camera", train_id))),
                    |id| Ok(id.to_string()),
                )
            }
        }
    }
}
