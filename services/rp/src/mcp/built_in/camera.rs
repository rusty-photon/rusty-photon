use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use rp_vocabulary::FrameType;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::internals::CaptureRequest;
use super::super::progress::{ProgressEmitter, ProgressSink};
use super::super::{tool_error, tool_success};
use crate::persistence::LightSource;

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
    /// Binning as `"AxB"` (e.g. `"1x1"`, `"2x2"`) — the same spelling
    /// an acquisition goal uses. Omitted means `"1x1"`: rp writes the
    /// binning and the full-frame subframe before every exposure, so
    /// nothing another client left on the camera beforehand reaches
    /// the frame. A capture through a camera rp is already capturing on
    /// waits for that capture, so two overlapping calls cannot
    /// interleave their writes; a client outside rp is not queued — see
    /// docs/services/rp.md § Capture Tool Details, "Binning" →
    /// Concurrency.
    #[serde(default)]
    pub binning: Option<rp_vocabulary::Binning>,
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
    /// What lights the frame: `panel` (a light source that moves with
    /// the optics, such as a flat panel on the dew shield) or `sky` (a
    /// twilight flat). It tells a panel flat from a sky flat; accepted
    /// on a `Flat` or an untyped capture, an error on a `Light`, `Dark`
    /// or `Bias`, and recorded on the exposure document. While
    /// conditions are unsafe only a `Dark`, a `Bias` or a `panel`
    /// capture is taken, and only those run on through the unsafe
    /// transition — see docs/services/rp.md § Capture Tool Details,
    /// *Light source*.
    #[serde(default)]
    pub light_source: Option<LightSource>,
    /// A held train lease to capture under (docs/services/rp.md § Train
    /// Lease): the capture runs under the lease's slot for the camera
    /// and its gate permit, and is refused when the lease is not held
    /// or does not hold the camera. Omitted, a capture through a camera
    /// a lease reserves waits for the lease to end.
    #[serde(default)]
    pub lease_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CameraIdParams {
    pub camera_id: String,
}

/// What an omitted `binning` parameter means, everywhere one is
/// offered: `rp` sets the geometry on every exposure it starts, and
/// unbinned full-frame is the frame a caller who said nothing wants
/// (rp.md § Capture Tool Details, "Binning").
pub(crate) const DEFAULT_BINNING: rp_vocabulary::Binning = rp_vocabulary::Binning { x: 1, y: 1 };

/// The error for a `light_source` on a frame no light source can
/// describe: a `Light` is lit by the sky and a `Dark` or `Bias` by
/// nothing (rp.md § Capture Tool Details, *Light source*).
fn misplaced_light_source(
    frame_type: Option<FrameType>,
    light_source: Option<LightSource>,
) -> Option<String> {
    match (frame_type, light_source) {
        (Some(frame_type @ (FrameType::Light | FrameType::Dark | FrameType::Bias)), Some(_)) => {
            Some(format!(
                "capture: light_source applies to a Flat or an untyped capture, not to a {frame_type} frame"
            ))
        }
        _ => None,
    }
}

#[tool_router(router = tool_router_camera, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Capture an image, download image_array, save FITS file. Optional \
                        binning (\"AxB\", default \"1x1\") is written to the camera along with \
                        a full-frame subframe before every exposure, so a binning or crop \
                        another client left on it beforehand never reaches the frame. A \
                        capture through a camera rp is already capturing on waits for that \
                        capture to finish instead of failing; a camera another client \
                        (outside rp) is exposing on is not queued. Optional \
                        target (slug) + frame_type (Light/Dark/Flat/Bias) link the frame to \
                        the target store and render session.directory_pattern/ \
                        file_naming_pattern into the final path (Decision 11) — omit both to \
                        keep the flat <doc_uuid_8>.fits behavior. Optional light_source \
                        (panel/sky, with a Flat or an untyped capture) says what lights the \
                        frame. While conditions are unsafe only a Dark, a Bias or a \
                        light_source panel capture is taken; any other is refused with the \
                        SafetyUnsafe error, and is cancelled (\"cancelled: safety\") when \
                        conditions turn unsafe while it exposes"
    )]
    pub(crate) async fn capture(
        &self,
        Parameters(params): Parameters<CaptureParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // ProgressSink is `None` when the client did not supply a
        // `progressToken` in `_meta` — `do_capture` then treats the
        // emission as a no-op (see `mcp::progress`).
        let sink = ProgressSink::from_request_context(&ctx);
        let emitter = sink.as_ref().map(ProgressSink::as_emitter);
        let cancel = Cancel::from_context(&ctx);
        self.capture_inner(params, emitter, &cancel).await
    }

    /// Body of the `capture` MCP tool, split out so unit tests can
    /// pass `None` for the progress emitter without constructing a
    /// real rmcp `Peer`.
    pub(crate) async fn capture_inner(
        &self,
        params: CaptureParams,
        progress: Option<&dyn ProgressEmitter>,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // A frame that needs the sky never gets here while unsafe: the
        // dispatch classes the call from these same arguments and
        // refuses it (`gate::call_class`).
        if let Some(error) = misplaced_light_source(params.frame_type, params.light_source) {
            return Ok(tool_error!("{}", error));
        }
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
                CaptureRequest {
                    camera_id: &camera_id,
                    duration: params.duration,
                    binning: params.binning.unwrap_or(DEFAULT_BINNING),
                    target: params.target.as_deref(),
                    frame_type: params.frame_type,
                    light_source: params.light_source,
                    lease_id: params.lease_id.as_deref(),
                },
                progress,
                cancel,
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

    #[tool(
        description = "Read camera capabilities and current settings: max_adu, exposure limits, sensor dimensions, current binning, the binning envelope (max_bin_x/max_bin_y/can_asymmetric_bin — what a capture may ask for; null when the connect-time read failed), gain and offset (null only when the driver does not implement the property; any other read failure is an error)"
    )]
    pub(crate) async fn get_camera_info(
        &self,
        Parameters(params): Parameters<CameraIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(cam_entry) = self.equipment.find_camera(&params.camera_id) else {
            return Ok(tool_error!("camera not found: {}", params.camera_id));
        };
        // The sensor dimensions are invariant physical-sensor properties
        // cached on `CameraEntry` at connect time — no live Alpaca
        // round-trip per call. `None` here means the connect-time read
        // failed; surface as a tool_error so consumers can't mistake an
        // absent value for a successful zero. `max_adu`, `bin` and
        // `exposure_range` stay live: a driver may tie `MaxADU` to the
        // readout mode, binning is operator-mutable and exposure range can
        // shift on driver reconfig, so none belongs in the connect-time
        // cache. The handle comes out of the session slot paired with the
        // cache, so the live reads below cannot describe a session the
        // cached values never belonged to.
        let Some((cam, invariants)) = cam_entry.snapshot() else {
            return Ok(tool_error!("camera not connected: {}", params.camera_id));
        };
        let max_adu = match cam.max_adu().await {
            Ok(max_adu) => max_adu,
            Err(e) => return Ok(tool_error!("failed to read max_adu: {}", e)),
        };

        let (Some(sensor_x), Some(sensor_y)) =
            (invariants.sensor_width_px, invariants.sensor_height_px)
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

        // Gain and offset are read from the driver on every call, never
        // cached here — they are operator-mutable, and a record of flat
        // timing is only valid at the gain it was trained at
        // (calibrator-flats plan, D4/D5).
        // `null` means exactly one thing: the driver has no such
        // property (ASCOM `NotImplemented`, the common case for CCDs).
        // Any other read failure is a tool error, so a transport blip
        // can never be persisted as "this camera has no gain".
        let gain = match cam.gain().await {
            Ok(gain) => Some(gain),
            Err(e) if e.code == ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED => {
                debug!(camera_id = %params.camera_id, "gain not implemented by the driver; reporting null");
                None
            }
            Err(e) => return Ok(tool_error!("failed to read gain: {}", e)),
        };
        let offset = match cam.offset().await {
            Ok(offset) => Some(offset),
            Err(e) if e.code == ascom_alpaca::ASCOMErrorCode::NOT_IMPLEMENTED => {
                debug!(camera_id = %params.camera_id, "offset not implemented by the driver; reporting null");
                None
            }
            Err(e) => return Ok(tool_error!("failed to read offset: {}", e)),
        };

        Ok(tool_success!({
            "camera_id": params.camera_id,
            "max_adu": max_adu,
            "sensor_x": sensor_x,
            "sensor_y": sensor_y,
            "bin_x": bin_x,
            "bin_y": bin_y,
            "max_bin_x": invariants.max_bin_x,
            "max_bin_y": invariants.max_bin_y,
            "can_asymmetric_bin": invariants.can_asymmetric_bin,
            "gain": gain,
            "offset": offset,
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
