//! Guider tool category: `start_guiding`, `stop_guiding`, `dither`,
//! `pause_guiding`, `resume_guiding`, `get_guiding_stats`.
//!
//! All six proxy to the guider service (guider.md § HTTP API) through
//! the `rp-guider` client on `McpHandler::guider`; `None` there means
//! every tool errors with "guider not configured". Settle thresholds
//! and the RMS the events report are in **guide-camera pixels**; the
//! tools' results pass the guider's arcsecond values through beside
//! them. `dither`'s optional `unit` says how the per-call amount is
//! read: `guide_px` goes out as `amount_px`, `arcsec` as
//! `amount_arcsec`, and `main_px` is multiplied by the imaging train's
//! pixel scale (rp.md § Optical Trains) and goes out as arcseconds.
//!
//! Before a start, `rp` checks that the guider's devices are its own
//! ([`McpHandler::check_guider_devices`], rp.md § Guider Service), and
//! then takes the guide camera guard, which refuses the start at once
//! while a capture through the guide camera or a guiding train's lease
//! holds it (rp.md § The guide camera guard).
//!
//! The settle-blocking operations emit operation-event triples that
//! terminate in `*_settled` rather than `*_complete`:
//! `guide_started` / `guide_settled` / `guide_failed` and
//! `dither_started` / `dither_settled` / `dither_failed`. A stop `rp`
//! asks for emits the `guide_stopped` point event with
//! `reason: "requested"`; the safety enforcer emits the same event
//! with `reason: "safety"`, and the Guide Run Watch with
//! `reason: "guider"`.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::OwnedRwLockWriteGuard;
use tracing::{debug, warn};

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::{tool_error, tool_success};
use crate::events::EventEnvelope;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartGuidingParams {
    /// Calibrate anew before guiding starts. Defaults to false: the
    /// guider calibrates only when it holds no calibration it can use.
    #[serde(default)]
    pub recalibrate: Option<bool>,
    /// Settle threshold in guide-camera pixels. Falls back to
    /// `settle_pixels` of rp's guiding config, then to the guider's
    /// own default.
    #[serde(default)]
    pub settle_pixels: Option<f64>,
    /// How long guiding must hold within the threshold (humantime
    /// string). Same fallback chain as `settle_pixels`.
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub settle_time: Option<Duration>,
    /// Deadline for the settle to complete (humantime string). Same
    /// fallback chain as `settle_pixels`.
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub settle_timeout: Option<Duration>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopGuidingParams {}

/// Unit of a `dither` amount.
///
/// Guide-camera pixels and arcseconds go to the guider as they are;
/// main-camera pixels are converted to arcseconds by rp via the train
/// pixel-scale derivation (rp.md § Optical Trains).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DitherUnit {
    /// Guide-camera pixels.
    #[default]
    GuidePx,
    /// Main-camera pixels — requires exactly one imaging train.
    MainPx,
    /// Arcseconds on the sky.
    Arcsec,
}

impl DitherUnit {
    const fn name(self) -> &'static str {
        match self {
            Self::GuidePx => "guide_px",
            Self::MainPx => "main_px",
            Self::Arcsec => "arcsec",
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DitherParams {
    /// Dither offset, interpreted in `unit` (guide-camera pixels by
    /// default). Falls back to `dither_pixels` of rp's guiding config
    /// when `unit` is absent or `guide_px`; an error when neither
    /// is set.
    #[serde(default)]
    pub pixels: Option<f64>,
    /// Unit of `pixels`: `guide_px` (default), `main_px`, or
    /// `arcsec`. Non-default units require an explicit `pixels`
    /// amount, and `main_px` exactly one imaging train with
    /// `focal_length_mm`.
    #[serde(default)]
    pub unit: Option<DitherUnit>,
    /// Restrict the dither to the RA axis (declination drift stays
    /// untouched). Defaults to false.
    #[serde(default)]
    pub ra_only: Option<bool>,
    /// Settle threshold in guide-camera pixels (fallback chain as in
    /// `start_guiding`).
    #[serde(default)]
    pub settle_pixels: Option<f64>,
    /// How long guiding must hold within the threshold (humantime
    /// string).
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub settle_time: Option<Duration>,
    /// Deadline for the settle to complete (humantime string).
    #[serde(default, with = "humantime_serde::option")]
    #[schemars(with = "Option<String>")]
    pub settle_timeout: Option<Duration>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PauseGuidingParams {
    /// When true, the guide frames stop too ("full" pause); otherwise
    /// only guide corrections pause and the camera keeps exposing.
    /// Defaults to false.
    #[serde(default)]
    pub full: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResumeGuidingParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetGuidingStatsParams {}

#[tool_router(router = tool_router_guider, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Start guiding via the guider service and block until settled. The guider selects its guide stars, and calibrates first when it holds no calibration it can use. Settle overrides (settle_pixels, settle_time, settle_timeout) fall back to rp's guiding config, then to the guider's own defaults. Refused when the guider's camera or mount is not the one rp holds, and at once while a capture through the guide camera or a guiding train's lease is under way. Returns the guider's state and the rolling RMS in guide-camera pixels and in arcseconds."
    )]
    pub(crate) async fn start_guiding(
        &self,
        Parameters(params): Parameters<StartGuidingParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.start_guiding_inner(params, &cancel).await
    }

    /// Body of the `start_guiding` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn start_guiding_inner(
        &self,
        params: StartGuidingParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("start_guiding: guider not configured"));
        };
        let request = rp_guider::StartGuidingRequest {
            recalibrate: params.recalibrate.unwrap_or(false),
            settle: self.merge_settle(
                params.settle_pixels,
                params.settle_time,
                params.settle_timeout,
            ),
        };
        match self
            .start_guiding_run("start_guiding", client.as_ref(), request, cancel)
            .await
        {
            Ok(outcome) => Ok(tool_success!(settled_result(&outcome))),
            Err(message) => Ok(tool_error!("{}", message)),
        }
    }

    #[tool(
        description = "Stop the guider's run or loop via the guider service, blocking until its exposure has ended and no pulse is in flight. Idempotent: stopping an idle guider succeeds. Returns the guider's state. Emits guide_stopped with reason \"requested\"."
    )]
    pub(crate) async fn stop_guiding(
        &self,
        Parameters(_params): Parameters<StopGuidingParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("stop_guiding: guider not configured"));
        };
        match self.stop_guiding_run(client.as_ref()).await {
            Ok(state) => Ok(tool_success!({ "state": state })),
            Err(e) => Ok(tool_error!("{}", guider_error_text("stop_guiding", &e))),
        }
    }

    #[tool(
        description = "Move the guider's lock by `pixels` via the guider service and block until guiding re-settles. `unit` says how `pixels` is read: guide_px (the default, guide-camera pixels), arcsec, or main_px (main-camera pixels, converted with the imaging train's pixel scale). With guide_px, `pixels` falls back to dither_pixels of rp's guiding config. ra_only restricts the offset to the RA axis. Settle overrides as in start_guiding. The guider refuses a dither outside the state guiding."
    )]
    pub(crate) async fn dither(
        &self,
        Parameters(params): Parameters<DitherParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.dither_inner(params, &cancel).await
    }

    /// Body of the `dither` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn dither_inner(
        &self,
        params: DitherParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("dither: guider not configured"));
        };
        let amount = match self.dither_amount(params.pixels, params.unit.unwrap_or_default()) {
            Ok(amount) => amount,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let settle = self.merge_settle(
            params.settle_pixels,
            params.settle_time,
            params.settle_timeout,
        );
        let ra_only = params.ra_only.unwrap_or(false);

        // Mount motion (rp.md § Mount Motion Gate): exclusive acquire
        // after parameter resolution (invalid calls fail fast above
        // without waiting) and before `dither_started`, held through
        // the settle. In-flight imaging-train exposures finish first.
        // A call cancelled while queued returns without moving anything.
        let motion_permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
            permit = self.motion_gate.exclusive("dither") => permit,
        };

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        let (amount_px, amount_arcsec) = match amount {
            rp_guider::DitherAmount::Px(px) => (Some(px), None),
            rp_guider::DitherAmount::Arcsec(arcsec) => (None, Some(arcsec)),
        };
        let started_payload = serde_json::json!({
            "amount_px": amount_px,
            "amount_arcsec": amount_arcsec,
            "ra_only": ra_only,
            "settle_pixels": settle.as_ref().and_then(|s| s.pixels),
            "settle_time": humantime_or_null(settle.as_ref().and_then(|s| s.time)),
            "settle_timeout": humantime_or_null(settle.as_ref().and_then(|s| s.timeout)),
        });
        self.event_bus.emit_operation(with_settle_deadlines(
            EventEnvelope::started("dither", &operation_id, started_at, started_payload),
            settle.as_ref(),
            Duration::ZERO,
        ));

        // Owned future so a cancelled body can hand it, permit and all,
        // to the detached tail holder below.
        let tail_cap = rp_guider::dither_deadline(
            self.guider_defaults.timeout,
            settle.as_ref().and_then(|s| s.timeout),
        );
        let dither_client = Arc::clone(&client);
        let mut dither = Box::pin(async move {
            dither_client
                .dither(rp_guider::DitherRequest {
                    amount,
                    ra_only,
                    settle,
                })
                .await
        });
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Stop-class counterpart (rp.md § In-Flight Tool
                // Calls): the guider's dither abort. The mount is
                // pulsed until the guider has answered the dither
                // request, so the permit must not fall with this body:
                // a detached holder sends the abort and keeps the gate
                // exclusive until that answer.
                let message = cancel.error();
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "dither",
                    &operation_id,
                    started_at,
                    &message,
                ));
                abort_dither_holding_permit(motion_permit, client, dither, tail_cap);
                return Ok(tool_error!("{}", message));
            }
            outcome = &mut dither => outcome,
        };
        // The settle is over once the RPC returns (settled or failed);
        // the event and the reply need no gate.
        drop(motion_permit);
        match outcome {
            Ok(outcome) => {
                self.event_bus.emit_operation(EventEnvelope::settled(
                    "dither",
                    &operation_id,
                    started_at,
                    settled_payload(&outcome),
                ));
                Ok(tool_success!(settled_result(&outcome)))
            }
            Err(e) => {
                let message = guider_error_text("dither", &e);
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "dither",
                    &operation_id,
                    started_at,
                    &message,
                ));
                Ok(tool_error!("{}", message))
            }
        }
    }

    #[tool(
        description = "Pause guide corrections via the guider service (e.g. during readout). full=true also stops the guide frames. Returns the guider's state."
    )]
    pub(crate) async fn pause_guiding(
        &self,
        Parameters(params): Parameters<PauseGuidingParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("pause_guiding: guider not configured"));
        };
        match client.pause_guiding(params.full.unwrap_or(false)).await {
            Ok(state) => Ok(tool_success!({ "state": state })),
            Err(e) => Ok(tool_error!("{}", guider_error_text("pause_guiding", &e))),
        }
    }

    #[tool(
        description = "Resume paused guiding via the guider service. Returns the guider's state: guiding, or star_lost when the run is in it."
    )]
    pub(crate) async fn resume_guiding(
        &self,
        Parameters(params): Parameters<ResumeGuidingParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.resume_guiding_inner(params, &cancel).await
    }

    /// Body of the `resume_guiding` MCP tool, split out so unit tests
    /// can pass a never-cancelled handle without constructing a real
    /// rmcp `RequestContext`.
    pub(crate) async fn resume_guiding_inner(
        &self,
        _params: ResumeGuidingParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("resume_guiding: guider not configured"));
        };
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
            outcome = client.resume_guiding() => outcome,
        };
        match outcome {
            Ok(state) => Ok(tool_success!({ "state": state })),
            Err(e) => Ok(tool_error!("{}", guider_error_text("resume_guiding", &e))),
        }
    }

    #[tool(
        description = "Read the guider's state and rolling RMS statistics (guide-camera pixels and arcseconds) from the guider service. Cheap; safe to poll."
    )]
    pub(crate) async fn get_guiding_stats(
        &self,
        Parameters(_params): Parameters<GetGuidingStatsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(client) = self.guider.clone() else {
            return Ok(tool_error!("get_guiding_stats: guider not configured"));
        };
        match client.guiding_stats().await {
            Ok(stats) => Ok(tool_success!({
                "state": stats.state,
                "guiding": stats.guiding,
                "settling": stats.settling,
                "pixel_scale_arcsec": stats.pixel_scale_arcsec,
                "rms_ra_px": stats.rms_ra_px,
                "rms_dec_px": stats.rms_dec_px,
                "total_rms_px": stats.total_rms_px,
                "rms_ra_arcsec": stats.rms_ra_arcsec,
                "rms_dec_arcsec": stats.rms_dec_arcsec,
                "total_rms_arcsec": stats.total_rms_arcsec,
                "snr": stats.snr,
                "star_mass": stats.star_mass,
                "star_count": stats.star_count,
                "sample_count": stats.sample_count,
                "last_stop": stats.last_stop,
            })),
            Err(e) => Ok(tool_error!(
                "{}",
                guider_error_text("get_guiding_stats", &e)
            )),
        }
    }
}

impl McpHandler {
    /// Start a guiding run and wait for it to settle: the devices
    /// check, the guide camera guard, then the `guide` event triple
    /// around the guider's start. The `start_guiding` tool and the
    /// rotate-while-guiding ladder both start a run here, so both emit
    /// the same events.
    ///
    /// `what` leads the error text and names the start in a refusal the
    /// guard gives a capture or a lease. A refusal of the devices check
    /// or of the guard is an error with no event: nothing has been asked
    /// of the guider. The guard is held until the call ends, the stop
    /// that undoes a cancelled start included (rp.md § The guide camera
    /// guard). A cancellation after `guide_started` stops the guider
    /// (rp.md § In-Flight Tool Calls) and reports `guide_failed`.
    pub(crate) async fn start_guiding_run(
        &self,
        what: &'static str,
        client: &dyn rp_guider::GuiderClient,
        request: rp_guider::StartGuidingRequest,
        cancel: &Cancel,
    ) -> Result<rp_guider::SettledOutcome, String> {
        let checked = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(cancel.error()),
            checked = self.check_guider_devices(client) => checked,
        };
        checked.map_err(|e| format!("{what}: {e}"))?;
        let _guard = self
            .guarded_guide_camera()
            .map(|camera_id| self.guide_camera.take_for_start(camera_id, what))
            .transpose()
            .map_err(|e| format!("{what}: {e}"))?;

        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        let settle = request.settle.as_ref();
        let started_payload = serde_json::json!({
            "recalibrate": request.recalibrate,
            "settle_pixels": settle.and_then(|s| s.pixels),
            "settle_time": humantime_or_null(settle.and_then(|s| s.time)),
            "settle_timeout": humantime_or_null(settle.and_then(|s| s.timeout)),
        });
        // A start calibrates when the guider holds no calibration it
        // can use, so its deadline has the calibration in it.
        self.event_bus.emit_operation(with_settle_deadlines(
            EventEnvelope::started("guide", &operation_id, started_at, started_payload),
            settle,
            self.guider_defaults.calibration_timeout,
        ));

        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Stop-class counterpart (rp.md § In-Flight Tool Calls):
                // undo the half-started run. Best-effort — on the
                // safety path the enforcer's own stop follows anyway.
                if let Err(e) = client.stop_guiding().await {
                    debug!(error = %e, "stop_guiding after a cancelled start failed");
                }
                let message = cancel.error();
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "guide",
                    &operation_id,
                    started_at,
                    &message,
                ));
                return Err(message);
            }
            outcome = client.start_guiding(request) => outcome,
        };
        match outcome {
            Ok(outcome) => {
                self.event_bus.emit_operation(EventEnvelope::settled(
                    "guide",
                    &operation_id,
                    started_at,
                    settled_payload(&outcome),
                ));
                Ok(outcome)
            }
            Err(e) => {
                let message = guider_error_text(what, &e);
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "guide",
                    &operation_id,
                    started_at,
                    &message,
                ));
                Err(message)
            }
        }
    }

    /// Stop the guider's run or loop and, once the guider has answered,
    /// emit `guide_stopped` with `reason: "requested"`. The
    /// `stop_guiding` tool and the rotate-while-guiding ladder both
    /// stop here.
    pub(crate) async fn stop_guiding_run(
        &self,
        client: &dyn rp_guider::GuiderClient,
    ) -> Result<rp_guider::GuiderState, rp_guider::GuiderError> {
        let state = client.stop_guiding().await?;
        self.event_bus.emit(
            "guide_stopped",
            serde_json::json!({ "reason": "requested", "stop_reason": null }),
        );
        Ok(state)
    }

    /// The same camera and mount check before a start (rp.md § Guider
    /// Service): read the guider's `devices` and compare the
    /// `unique_id` it reports for each device with the one rp's own
    /// session bound. A difference refuses, naming both ids, and so
    /// does an id that is missing or blank on either side: an identity
    /// that cannot be checked never passes. The check reads and
    /// compares, and sends no device anything.
    pub(crate) async fn check_guider_devices(
        &self,
        client: &dyn rp_guider::GuiderClient,
    ) -> Result<(), String> {
        let devices = client
            .devices()
            .await
            .map_err(|e| format!("the guider's devices could not be read: {e}"))?;
        self.check_guider_mount(devices.mount.as_ref())?;
        self.check_guider_camera(devices.camera.as_ref())
    }

    /// The guider's mount is rp's mount.
    fn check_guider_mount(&self, mount: Option<&rp_guider::GuiderDevice>) -> Result<(), String> {
        let Some(theirs) = reported_id(mount) else {
            return Err(
                "the guider reports no unique_id for its mount, so it cannot be checked against rp's mount"
                    .to_string(),
            );
        };
        let ours = self
            .equipment
            .mount
            .as_ref()
            .and_then(|m| m.session.bound_identity())
            .map(|identity| identity.unique_id)
            .filter(|id| !id.is_empty());
        let Some(ours) = ours else {
            return Err(format!(
                "rp holds no session with its mount, so the guider's mount '{theirs}' cannot be checked against it"
            ));
        };
        if ours != theirs {
            return Err(format!(
                "the guider's mount '{theirs}' is not rp's mount '{ours}'"
            ));
        }
        Ok(())
    }

    /// The guider's camera is the guiding train's camera. On a rig
    /// with no guiding train it is none of the cameras rp holds a
    /// session with, since rp guards no camera there.
    fn check_guider_camera(&self, camera: Option<&rp_guider::GuiderDevice>) -> Result<(), String> {
        let theirs = reported_id(camera);
        let Some(train) = self.trains.guiding_train() else {
            let Some(theirs) = theirs else {
                return Err(
                    "the guider reports no unique_id for its camera, so it cannot be checked against rp's cameras"
                        .to_string(),
                );
            };
            let held = self.equipment.cameras.iter().find(|entry| {
                entry
                    .session
                    .bound_identity()
                    .is_some_and(|identity| identity.unique_id == theirs)
            });
            return held.map_or(Ok(()), |entry| {
                Err(format!(
                    "the guider's camera '{theirs}' is rp's camera '{}', which rp captures through unguarded: no guiding train holds it",
                    entry.id
                ))
            });
        };
        let Some(theirs) = theirs else {
            return Err(
                "the guider reports no unique_id for its camera, so it cannot be checked against the guiding train's camera"
                    .to_string(),
            );
        };
        let camera_id = train.camera_id().unwrap_or_default();
        let ours = self
            .equipment
            .find_camera(camera_id)
            .and_then(|entry| entry.session.bound_identity())
            .map(|identity| identity.unique_id)
            .filter(|id| !id.is_empty());
        let Some(ours) = ours else {
            return Err(format!(
                "rp holds no session with the guiding train's camera '{camera_id}', so the guider's camera '{theirs}' cannot be checked against it"
            ));
        };
        if ours != theirs {
            return Err(format!(
                "the guider's camera '{theirs}' is not the guiding train's camera '{ours}'"
            ));
        }
        Ok(())
    }

    /// The amount a `dither` sends. Guide-camera pixels and arcseconds
    /// go out as they are; main-camera pixels go out as arcseconds, by
    /// the imaging train's pixel scale. The `dither_pixels` config
    /// default is guide-camera pixels by definition, so it only backs
    /// the default unit.
    fn dither_amount(
        &self,
        pixels: Option<f64>,
        unit: DitherUnit,
    ) -> Result<rp_guider::DitherAmount, String> {
        match (pixels, unit) {
            (None, DitherUnit::GuidePx) => self
                .guider_defaults
                .dither_pixels
                .map(rp_guider::DitherAmount::Px)
                .ok_or_else(|| {
                    "dither: missing required argument: provide pixels or configure guider.dither_pixels"
                        .to_string()
                }),
            (None, unit) => Err(format!(
                "dither: unit '{}' requires an explicit pixels amount (the dither_pixels config default is guide-camera pixels)",
                unit.name()
            )),
            (Some(px), DitherUnit::GuidePx) => Ok(rp_guider::DitherAmount::Px(px)),
            (Some(arcsec), DitherUnit::Arcsec) => Ok(rp_guider::DitherAmount::Arcsec(arcsec)),
            (Some(px), DitherUnit::MainPx) => Ok(rp_guider::DitherAmount::Arcsec(
                px * self.main_pixel_scale_arcsec_per_px()?,
            )),
        }
    }

    /// The single imaging train's pixel scale — `main_px` is only
    /// well-defined when exactly one imaging train exists.
    fn main_pixel_scale_arcsec_per_px(&self) -> Result<f64, String> {
        let imaging: Vec<_> = self
            .trains
            .trains()
            .iter()
            .filter(|t| t.purpose == crate::config::TrainPurpose::Imaging)
            .collect();
        match imaging.as_slice() {
            [train] => self.train_pixel_scale_arcsec_per_px(train),
            [] => Err("dither: unit 'main_px' requires exactly one imaging train (found none)"
                .to_string()),
            many => Err(format!(
                "dither: unit 'main_px' requires exactly one imaging train (found {}); use unit 'arcsec' or 'guide_px'",
                many.len()
            )),
        }
    }

    /// A train's pixel scale in arcsec per pixel:
    /// `206.265 × pixel_size_x_um / focal_length_mm` (square pixels
    /// assumed — the x-axis size is used, read from the camera at
    /// connect time).
    fn train_pixel_scale_arcsec_per_px(
        &self,
        train: &crate::equipment::trains::Train,
    ) -> Result<f64, String> {
        let Some(focal_length_mm) = train.focal_length_mm else {
            return Err(format!(
                "dither: train '{}' has no focal_length_mm",
                train.id
            ));
        };
        let Some(camera_id) = train.camera_id() else {
            return Err(format!("dither: train '{}' has no camera", train.id));
        };
        let Some(entry) = self.equipment.find_camera(camera_id) else {
            return Err(format!("dither: camera not found: {camera_id}"));
        };
        // A driver can report 0 (or garbage) for an unpopulated
        // property — OmniSim reads 0 before connect, for instance. A
        // non-positive size would turn the conversion into a division
        // by zero, so treat it the same as an absent read.
        match entry.invariants().pixel_size_x_um {
            Some(px) if px.is_finite() && px > 0.0 => Ok(206.265 * px / focal_length_mm),
            _ => Err(format!(
                "dither: pixel size of camera '{camera_id}' unavailable (connect-time read failed or camera not connected)"
            )),
        }
    }

    /// Merge per-call settle parameters over the rp-config defaults,
    /// field by field. `None` when every field ends up unset — the
    /// wire then omits `settle` entirely and the guider's own settle
    /// values apply.
    pub(crate) fn merge_settle(
        &self,
        pixels: Option<f64>,
        time: Option<Duration>,
        timeout: Option<Duration>,
    ) -> Option<rp_guider::SettleOverride> {
        let settle = rp_guider::SettleOverride {
            pixels: pixels.or(self.guider_defaults.settle_pixels),
            time: time.or(self.guider_defaults.settle_time),
            timeout: timeout.or(self.guider_defaults.settle_timeout),
        };
        (!settle.is_empty()).then_some(settle)
    }
}

/// The `unique_id` the guider reports for a device, stripped of
/// padding as rp strips its own; `None` for a device the guider does
/// not have, an id it has not read, and a blank one.
fn reported_id(device: Option<&rp_guider::GuiderDevice>) -> Option<&str> {
    device
        .and_then(|d| d.unique_id.as_deref())
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// Attach the settle deadline to a `*_started` envelope when the
/// resolved settle pins a timeout: predicted = the hold time (or the
/// timeout when no hold time is known), max = `before_settle` plus the
/// timeout plus the margin the guider's answer gets past it. Without a
/// resolved timeout the guider's own applies, which rp cannot see, so
/// the deadline fields stay omitted (same posture as operations
/// without predictions).
///
/// A misconfigured hold time longer than the timeout is clamped to the
/// timeout here — `predicted_duration_ms` must never exceed
/// `max_duration_ms` (Sentinel treats that as a contract violation).
fn with_settle_deadlines(
    envelope: EventEnvelope,
    settle: Option<&rp_guider::SettleOverride>,
    before_settle: Duration,
) -> EventEnvelope {
    let Some(timeout) = settle.and_then(|s| s.timeout) else {
        return envelope;
    };
    let predicted = settle.and_then(|s| s.time).unwrap_or(timeout).min(timeout);
    let max = before_settle
        .saturating_add(timeout)
        .saturating_add(rp_guider::SETTLE_MARGIN);
    envelope.with_deadlines(
        u64::try_from(predicted.as_millis()).unwrap_or(u64::MAX),
        u64::try_from(max.as_millis()).unwrap_or(u64::MAX),
    )
}

/// Payload shared by `guide_settled` / `dither_settled`: the settled
/// RMS snapshot in guide-camera pixels.
fn settled_payload(outcome: &rp_guider::SettledOutcome) -> serde_json::Value {
    serde_json::json!({
        "rms_ra_px": outcome.rms_ra_px,
        "rms_dec_px": outcome.rms_dec_px,
        "total_rms_px": outcome.total_rms_px,
        "sample_count": outcome.sample_count,
    })
}

/// Result shared by `start_guiding` / `dither`: the guider's state and
/// the settled RMS, in guide-camera pixels and in the guider's
/// arcseconds.
fn settled_result(outcome: &rp_guider::SettledOutcome) -> serde_json::Value {
    serde_json::json!({
        "state": outcome.state,
        "rms_ra_px": outcome.rms_ra_px,
        "rms_dec_px": outcome.rms_dec_px,
        "total_rms_px": outcome.total_rms_px,
        "rms_ra_arcsec": outcome.rms_ra_arcsec,
        "rms_dec_arcsec": outcome.rms_dec_arcsec,
        "total_rms_arcsec": outcome.total_rms_arcsec,
        "sample_count": outcome.sample_count,
    })
}

/// Map a client error onto the tool-error text, mirroring
/// `do_plate_solve`'s formatting: unreachable / structured envelope
/// (code + message, details when present) / internal.
pub(crate) fn guider_error_text(tool: &str, e: &rp_guider::GuiderError) -> String {
    match e {
        rp_guider::GuiderError::ServiceUnreachable(reason) => {
            format!("{tool}: service unreachable: {reason}")
        }
        rp_guider::GuiderError::Service {
            code,
            message,
            details,
        } => {
            if details.is_null() {
                format!("{tool}: {code}: {message}")
            } else {
                format!("{tool}: {code}: {message} (details: {details})")
            }
        }
        rp_guider::GuiderError::Internal(reason) => format!("{tool}: internal: {reason}"),
    }
}

/// Send the guider's dither abort for a cancelled `dither` and keep
/// `permit` alive on a detached task until the guider has answered the
/// dither request (settled, aborted, failed, or `cap` elapsed), so no
/// imaging-train capture starts into guide pulses the cancelled body
/// can no longer wait for. Both outcomes are only logged: the caller
/// already has its `cancelled: <reason>` answer.
fn abort_dither_holding_permit(
    permit: OwnedRwLockWriteGuard<()>,
    client: Arc<dyn rp_guider::GuiderClient>,
    dither: impl std::future::Future<Output = Result<rp_guider::SettledOutcome, rp_guider::GuiderError>>
        + Send
        + 'static,
    cap: Duration,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let abort = async {
            match client.abort_dither().await {
                Ok(aborted) => debug!(aborted, "dither abort sent for a cancelled dither"),
                Err(e) => debug!(error = %e, "dither abort for a cancelled dither failed"),
            }
        };
        let ((), tail) = tokio::join!(abort, tokio::time::timeout(cap, dither));
        match tail {
            Ok(Ok(_)) => debug!("cancelled dither settled; motion permit released"),
            Ok(Err(e)) => debug!(error = %e, "cancelled dither ended; motion permit released"),
            Err(_) => warn!(
                cap = ?cap,
                "the guider did not answer a cancelled dither within its cap; releasing the motion permit"
            ),
        }
    });
}

/// Humantime string for an optional duration; JSON `null` when unset
/// so `*_started` payloads keep a stable key set.
fn humantime_or_null(duration: Option<Duration>) -> serde_json::Value {
    duration.map_or(serde_json::Value::Null, |d| {
        serde_json::Value::String(humantime::format_duration(d).to_string())
    })
}
