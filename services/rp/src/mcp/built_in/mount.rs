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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SlewParams {
    /// Right ascension in decimal hours, [0, 24).
    #[serde(default)]
    pub ra: Option<f64>,
    /// Declination in decimal degrees, [-90, 90].
    #[serde(default)]
    pub dec: Option<f64>,
    /// Optional per-call settle override. Wins over the mount config's
    /// `settle_after_slew`. Pass `"0s"` to skip the configured default.
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub settle_after: Option<Duration>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SyncMountParams {
    /// Right ascension in decimal hours, [0, 24).
    #[serde(default)]
    pub ra: Option<f64>,
    /// Declination in decimal degrees, [-90, 90].
    #[serde(default)]
    pub dec: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetTrackingParams {
    pub enabled: bool,
}

/// Empty parameter struct for `get_tracking` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTrackingParams {}

/// Empty parameter struct for `get_mount_position` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetMountPositionParams {}

/// Empty parameter struct for `park` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ParkParams {}

/// Empty parameter struct for `unpark` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnparkParams {}

/// Empty parameter struct for `get_park_state` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParkStateParams {}

/// Empty parameter struct for `abort_slew` — the tool takes no input.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AbortSlewParams {}

#[tool_router(router = tool_router_mount, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Slew the mount to equatorial coordinates (RA hours, Dec degrees). Blocks until the mount reports Slewing == false plus the configured / per-call settle. Tracking must be on before calling — propagates the Alpaca error otherwise."
    )]
    pub(crate) async fn slew(
        &self,
        Parameters(params): Parameters<SlewParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Build the sink once and delegate so the testable body
        // doesn't have to take a real `RequestContext` (rmcp's
        // `Peer::new` is `pub(crate)`, so unit tests can't build
        // a peer themselves).
        let sink = ProgressSink::from_request_context(&ctx);
        let emitter = sink.as_ref().map(ProgressSink::as_emitter);
        let cancel = Cancel::from_context(&ctx);
        self.slew_inner(params, emitter, &cancel).await
    }

    /// Body of the `slew` MCP tool, split out so unit tests can pass
    /// `None` for the progress emitter without constructing a real
    /// rmcp `Peer` (its constructor is `pub(crate)` in rmcp 1.7).
    pub(crate) async fn slew_inner(
        &self,
        params: SlewParams,
        progress: Option<&dyn ProgressEmitter>,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Body validation in input order (ra → dec → settle_after) so
        // the error message points at the first missing or out-of-range
        // field. Same convention as `auto_focus` / `measure_basic`.
        let Some(ra) = params.ra else {
            return Ok(tool_error!("missing required parameter: ra"));
        };
        if !(0.0..24.0).contains(&ra) {
            return Ok(tool_error!(
                "ra out of range: {} (must be in [0.0, 24.0))",
                ra
            ));
        }
        let Some(dec) = params.dec else {
            return Ok(tool_error!("missing required parameter: dec"));
        };
        if !(-90.0..=90.0).contains(&dec) {
            return Ok(tool_error!(
                "dec out of range: {} (must be in [-90.0, 90.0])",
                dec
            ));
        }

        // Resolve `settle_after`: explicit per-call value wins; otherwise
        // pull the mount's configured default (or zero if no mount is
        // configured — `do_slew_blocking` below calls `resolve_mount`
        // and surfaces the "no mount configured" error in that case).
        let settle_after = params.settle_after.unwrap_or_else(|| {
            self.equipment
                .find_mount()
                .map_or_else(Duration::default, |entry| {
                    entry.config.settle_after_slew.unwrap_or_default()
                })
        });

        match self
            .do_slew_blocking(ra, dec, settle_after, progress, cancel)
            .await
        {
            Ok((actual_ra, actual_dec)) => Ok(tool_success!({
                "actual_ra": actual_ra,
                "actual_dec": actual_dec,
            })),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(
        description = "Sync the mount's reported position to the given equatorial coordinates (RA hours, Dec degrees). Immediate; no polling."
    )]
    pub(crate) async fn sync_mount(
        &self,
        Parameters(params): Parameters<SyncMountParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(ra) = params.ra else {
            return Ok(tool_error!("missing required parameter: ra"));
        };
        if !(0.0..24.0).contains(&ra) {
            return Ok(tool_error!(
                "ra out of range: {} (must be in [0.0, 24.0))",
                ra
            ));
        }
        let Some(dec) = params.dec else {
            return Ok(tool_error!("missing required parameter: dec"));
        };
        if !(-90.0..=90.0).contains(&dec) {
            return Ok(tool_error!(
                "dec out of range: {} (must be in [-90.0, 90.0])",
                dec
            ));
        }

        match self.do_sync_mount(ra, dec).await {
            Ok(()) => Ok(tool_success!({})),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(description = "Read the mount's current pointing as RA (hours) / Dec (degrees).")]
    pub(crate) async fn get_mount_position(
        &self,
        Parameters(_params): Parameters<GetMountPositionParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let ra = match mount.right_ascension().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount right_ascension: {}", e)),
        };
        let dec = match mount.declination().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount declination: {}", e)),
        };

        Ok(tool_success!({
            "ra": ra,
            "dec": dec,
        }))
    }

    #[tool(
        description = "Read the mount's tracking state and CanSetTracking capability. Fails loud if the Tracking read errors."
    )]
    pub(crate) async fn get_tracking(
        &self,
        Parameters(_params): Parameters<GetTrackingParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let tracking = match mount.tracking().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount tracking: {}", e)),
        };
        let can_set_tracking = match mount.can_set_tracking().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount can_set_tracking: {}", e)),
        };

        Ok(tool_success!({
            "tracking": tracking,
            "can_set_tracking": can_set_tracking,
        }))
    }

    #[tool(description = "Enable or disable the mount's sidereal tracking drive.")]
    pub(crate) async fn set_tracking(
        &self,
        Parameters(params): Parameters<SetTrackingParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        debug!(enabled = params.enabled, "setting mount tracking");
        match mount.set_tracking(params.enabled).await {
            Ok(()) => Ok(tool_success!({})),
            Err(e) => Ok(tool_error!("failed to set tracking: {}", e)),
        }
    }

    #[tool(
        description = "Park the mount: invoke ASCOM Park, then poll AtPark every 100 ms until it reports parked, bounded by a predicted worst-case-traverse deadline (not a flat ceiling). Per ASCOM, a successful park clears Tracking. Unlike slew, park does NOT auto-abort on timeout — call abort_slew explicitly to interrupt a stuck park."
    )]
    pub(crate) async fn park(
        &self,
        Parameters(params): Parameters<ParkParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let sink = ProgressSink::from_request_context(&ctx);
        let emitter = sink.as_ref().map(ProgressSink::as_emitter);
        let cancel = Cancel::from_context(&ctx);
        self.park_inner(params, emitter, &cancel).await
    }

    /// Body of the `park` MCP tool, split out so unit tests can pass
    /// `None` for the progress emitter without constructing a real
    /// rmcp `Peer`. Takes the (empty) `ParkParams` for shape parity
    /// with the other `_inner` helpers.
    pub(crate) async fn park_inner(
        &self,
        _params: ParkParams,
        progress: Option<&dyn ProgressEmitter>,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.do_park_blocking(progress, cancel).await {
            Ok(()) => Ok(tool_success!({})),
            Err(e) => Ok(tool_error!("{}", e)),
        }
    }

    #[tool(
        description = "Unpark the mount. Returns immediately (no Slewing poll — most drivers just clear the AtPark flag). Does NOT auto-enable Tracking; call set_tracking explicitly before slewing."
    )]
    pub(crate) async fn unpark(
        &self,
        Parameters(_params): Parameters<UnparkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        // Mount resolved — wrap the unpark in the operation triple. There
        // is no `do_unpark_blocking` helper (the per-driver unpark is a
        // possibly multi-step Action), so the triple is emitted here. A
        // resolve failure above emits nothing, matching the other tools.
        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus
            .emit_operation(crate::events::EventEnvelope::started(
                "unpark",
                &operation_id,
                started_at,
                serde_json::json!({}),
            ));

        debug!("unparking mount");
        match mount.unpark().await {
            Ok(()) => {
                self.event_bus
                    .emit_operation(crate::events::EventEnvelope::complete(
                        "unpark",
                        &operation_id,
                        started_at,
                        serde_json::json!({}),
                    ));
                Ok(tool_success!({}))
            }
            Err(e) => {
                self.event_bus
                    .emit_operation(crate::events::EventEnvelope::failed(
                        "unpark",
                        &operation_id,
                        started_at,
                        &format!("failed to unpark: {e}"),
                    ));
                Ok(tool_error!("failed to unpark: {}", e))
            }
        }
    }

    #[tool(
        description = "Read the mount's park state and capabilities: AtPark, CanPark, CanUnpark. Fails loud on the AtPark read error (the load-bearing field)."
    )]
    pub(crate) async fn get_park_state(
        &self,
        Parameters(_params): Parameters<GetParkStateParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        let at_park = match mount.at_park().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount at_park: {}", e)),
        };
        let can_park = match mount.can_park().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount can_park: {}", e)),
        };
        let can_unpark = match mount.can_unpark().await {
            Ok(v) => v,
            Err(e) => return Ok(tool_error!("failed to read mount can_unpark: {}", e)),
        };

        Ok(tool_success!({
            "at_park": at_park,
            "can_park": can_park,
            "can_unpark": can_unpark,
        }))
    }

    #[tool(
        description = "Abort an in-progress mount slew or park. Per ASCOM, only valid while Slewing == true; the natural Alpaca error propagates otherwise."
    )]
    pub(crate) async fn abort_slew(
        &self,
        Parameters(_params): Parameters<AbortSlewParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (_entry, mount) = match self.resolve_mount() {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("{}", e)),
        };

        debug!("aborting mount slew");
        match mount.abort_slew().await {
            Ok(()) => Ok(tool_success!({})),
            Err(e) => Ok(tool_error!("failed to abort slew: {}", e)),
        }
    }
}
