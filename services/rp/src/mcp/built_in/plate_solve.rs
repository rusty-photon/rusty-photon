use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use super::super::handler::McpHandler;
use super::super::internals::DoPlateSolveInput;
use super::super::{tool_error, tool_success};

/// Nested pointing-hint object passed to `plate_solve`.
///
/// The both-or-neither contract for ra/dec is structural — supplying
/// only one of `ra_deg` / `dec_deg` is a serde-level deserialization
/// error rather than a runtime check. Field names carry units to
/// preempt the Alpaca-hours / wrapper-degrees gotcha at the call
/// site.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointingHint {
    pub ra_deg: f64,
    pub dec_deg: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlateSolveParams {
    /// Exposure-document id; resolved through the unified image+document
    /// cache. Wins over `image_path` when both are supplied. Either this
    /// or `image_path` must be present.
    #[serde(default)]
    pub document_id: Option<String>,
    /// FITS file on disk (read by the wrapper). Either this or
    /// `document_id` must be present.
    #[serde(default)]
    pub image_path: Option<String>,
    /// Explicit pointing hint (decimal degrees). Overrides the pointing
    /// the frame's sidecar recorded. Refused with `blind`.
    #[serde(default)]
    pub pointing_hint: Option<PointingHint>,
    /// When `true`, solve without a position: the frame's recorded
    /// pointing is ignored and the wrapper searches the whole sky. For a
    /// caller that knows the recorded position is wrong. Refused with
    /// `pointing_hint` or `search_radius_deg`.
    #[serde(default)]
    pub blind: Option<bool>,
    /// Image FOV hint in decimal degrees (matches ASTAP's `-fov`).
    /// Overrides the frame's recorded `optics.fov_height_deg`.
    #[serde(default)]
    pub fov_hint_deg: Option<f64>,
    /// Per-call search radius override. Wins over the rp-config default.
    /// Dropped when the solve has no position.
    #[serde(default)]
    pub search_radius_deg: Option<f64>,
    /// Per-call solve timeout (humantime string). Forwarded to the
    /// wrapper's `timeout` field.
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub timeout: Option<Duration>,
}

#[tool_router(router = tool_router_plate_solve, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Plate-solve a captured frame by proxying to the plate-solver rp-managed service. Accepts document_id or image_path; document_id wins. The position hint is pointing_hint, else the pointing the frame's sidecar recorded; the scale is fov_hint_deg, else the sidecar's optics.fov_height_deg. A frame with no position, or a call with blind=true, is solved blind (whole sky). Persists a `wcs` section to the exposure document when one resolves."
    )]
    pub(crate) async fn plate_solve(
        &self,
        Parameters(params): Parameters<PlateSolveParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Validate "neither document_id nor image_path" before
        // delegating — the standalone tool's BDD pins the no-prefix
        // shape ("missing required argument: ..."), which the helper
        // doesn't emit (it always uses the "plate_solve: ..." prefix
        // that's appropriate for in-loop callers like
        // `center_on_target`). Keeping this one check here preserves
        // the user-visible error text without forking the helper.
        if params.document_id.is_none() && params.image_path.is_none() {
            return Ok(tool_error!(
                "missing required argument: provide either document_id or image_path"
            ));
        }

        let input = DoPlateSolveInput {
            document_id: params.document_id.as_deref(),
            image_path: params.image_path.as_deref(),
            pointing_hint: params.pointing_hint.as_ref().map(|p| (p.ra_deg, p.dec_deg)),
            blind: params.blind.unwrap_or(false),
            fov_hint_deg: params.fov_hint_deg,
            search_radius_deg: params.search_radius_deg,
            timeout: params.timeout,
        };

        match self.do_plate_solve(input).await {
            Ok(out) => Ok(tool_success!({
                "ra_center": out.ra_center,
                "dec_center": out.dec_center,
                "pixel_scale_arcsec": out.pixel_scale_arcsec,
                "rotation_deg": out.rotation_deg,
                "solver": out.solver,
                "wcs_matrix": out.wcs_matrix,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }
}
