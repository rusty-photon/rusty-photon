//! Rotator tool category: `move_rotator`, `get_rotator_position`
//! (rp.md § Rotator Tool Details).
//!
//! Both tools address the device as `rotator_id` *or* `train_id` —
//! exactly one — where `train_id` resolves through the optical-train
//! model and requires the train to contain exactly one rotator.
//! `move_rotator` moves to an absolute **sky** angle (the ASCOM
//! `Position` frame) and blocks polling `IsMoving` until idle under a
//! fixed 120 s ceiling: the rotator config has no rate to size one
//! from, so the `move_rotator_started` envelope carries no predictive
//! deadline. `moved_trains` on the result lists every train
//! containing the rotator; when it includes the guiding train and a
//! guider is configured, the move runs the rotate-while-guiding ladder
//! (pause corrections → move → calibration decision → back to
//! guiding).

use std::sync::Arc;
use std::time::Duration;

use ascom_alpaca::api::Rotator;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::time::Instant;
use tracing::{debug, warn};

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::internals::{wait_until_still, wind_down_bound, DeviceHold, MoveError};
use super::super::{resolve_device, tool_error, tool_success};
use crate::equipment::trains::TrainDeviceKind;
use crate::events::{EventBus, EventEnvelope};

/// Ceiling on the post-move `IsMoving` poll. Rotators have no rate
/// config to size a predictive deadline from; 120 s mirrors the
/// focuser fallback ceiling and covers a worst-case half-turn on the
/// slowest amateur rotators.
const ROTATOR_MOVE_DEADLINE: Duration = Duration::from_mins(2);

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["rotator_id"]}, {"required": ["train_id"]}]))]
pub struct MoveRotatorParams {
    /// Roster rotator id. Exactly one of `rotator_id` / `train_id`.
    #[serde(default)]
    pub rotator_id: Option<String>,
    /// Optical-train id; resolves the train's sole rotator.
    #[serde(default)]
    pub train_id: Option<String>,
    /// Target absolute sky angle in degrees, `0.0 <= angle < 360.0`
    /// (the ASCOM Position frame).
    #[serde(default)]
    pub angle: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["rotator_id"]}, {"required": ["train_id"]}]))]
pub struct RotatorPositionParams {
    /// Roster rotator id. Exactly one of `rotator_id` / `train_id`.
    #[serde(default)]
    pub rotator_id: Option<String>,
    /// Optical-train id; resolves the train's sole rotator.
    #[serde(default)]
    pub train_id: Option<String>,
}

#[tool_router(router = tool_router_rotator, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Move the rotator to an absolute sky angle in degrees (0.0 <= angle < 360.0, the ASCOM Position frame), blocking until it reports idle. Address as rotator_id or train_id (the train's sole rotator). Returns the read-back sky and mechanical angles plus moved_trains, the optical trains containing the rotator."
    )]
    pub(crate) async fn move_rotator(
        &self,
        Parameters(params): Parameters<MoveRotatorParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.move_rotator_inner(params, &cancel).await
    }

    /// Body of the `move_rotator` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    #[expect(
        clippy::too_many_lines,
        reason = "the rotate-while-guiding ladder (pause, pre-move read, move, calibration decision, back to guiding) is one contract, kept in one body so its order reads top to bottom"
    )]
    pub(crate) async fn move_rotator_inner(
        &self,
        params: MoveRotatorParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rotator_id = match self.resolve_rotator_addressing(
            "move_rotator",
            params.rotator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(result) => return Ok(*result),
        };
        let Some(angle) = params.angle else {
            return Ok(tool_error!("missing required parameter: angle"));
        };
        if !angle.is_finite() || !(0.0..360.0).contains(&angle) {
            return Ok(tool_error!(
                "move_rotator: angle out of range: {} (expected 0.0 <= angle < 360.0)",
                angle
            ));
        }
        // The range admits `-0.0`; the device and the events take it as 0.
        let angle = angle + 0.0;
        let (entry, rot) = resolve_device!(self, find_rotator, &rotator_id, "rotator");
        // One rotation through a rotator at a time, held for the whole
        // call, ladder included; a rotator a train lease holds still
        // waits for the lease to end (rp.md § Rotator Tool Details).
        let hold = match self
            .device_hold(("rotator", &rotator_id), Some(&entry.slot), None, cancel)
            .await
        {
            Ok(hold) => hold,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let moved_trains: Vec<String> = self
            .trains
            .trains_with_device(&rotator_id)
            .iter()
            .map(|t| t.id.clone())
            .collect();

        // Rotate-while-guiding ladder (rp.md § Rotator Tool Details):
        // it runs when the move turns the guiding train and a guider
        // is configured, and the guider's state decides what it does.
        // A state read that fails goes on as for a guider in no run:
        // nothing is paused, and a turn above the threshold still
        // owes the clear.
        let rotates_guiding = self
            .trains
            .guiding_train()
            .is_some_and(|g| moved_trains.contains(&g.id));
        let ladder_client = self.guider.clone().filter(|_| rotates_guiding);
        let mut found_state = None;
        if let Some(client) = &ladder_client {
            match client.guiding_stats().await {
                Ok(stats) => found_state = Some(stats.state),
                Err(e) => {
                    debug!(rotator_id, error = %e, "the guider's state could not be read; rotating as for a guider in no run");
                }
            }
        }
        // Only a run found guiding is paused, and only a run the
        // ladder paused is brought back to guiding.
        let guiding_paused = found_state == Some(rp_guider::GuiderState::Guiding);

        // The operation triple starts here: the ladder's pause and
        // pre-move read are part of the move (they issue RPCs on its
        // behalf), so their failures must surface as
        // `move_rotator_failed` to event subscribers, not as bare
        // tool errors. Rotator moves carry no predictive deadline, so
        // emitting `*_started` ahead of the pause costs nothing.
        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        self.event_bus.emit_operation(EventEnvelope::started(
            "move_rotator",
            &operation_id,
            started_at,
            serde_json::json!({
                "rotator_id": rotator_id,
                "angle": angle,
                "guiding_paused": guiding_paused,
            }),
        ));

        // Pause corrections (output-only) before any motion; a pause
        // failure aborts the tool with the rotator untouched. The
        // pre-move sky angle feeds the calibration decision below.
        let mut pre_move_angle = None;
        if let Some(client) = &ladder_client {
            if guiding_paused {
                if let Err(e) = client.pause_guiding(false).await {
                    let msg = format!("move_rotator: failed to pause guiding before rotating: {e}");
                    self.event_bus.emit_operation(EventEnvelope::failed(
                        "move_rotator",
                        &operation_id,
                        started_at,
                        &msg,
                    ));
                    return Ok(tool_error!("{}", msg));
                }
            }
            match rot.position().await {
                Ok(a) => pre_move_angle = Some(a),
                Err(e) => {
                    let resume_note = if guiding_paused {
                        resume_note(client.as_ref()).await
                    } else {
                        String::new()
                    };
                    let msg = format!(
                        "move_rotator: failed to read the pre-move sky angle: {e}{resume_note}"
                    );
                    self.event_bus.emit_operation(EventEnvelope::failed(
                        "move_rotator",
                        &operation_id,
                        started_at,
                        &msg,
                    ));
                    return Ok(tool_error!("{}", msg));
                }
            }
        }

        // The move's ceiling, which also bounds the wind-down of a move
        // that may have left the rotator moving. Unreachable overflow
        // degrades to an expired ceiling, not a panic.
        let now = Instant::now();
        let ceiling = now.checked_add(ROTATOR_MOVE_DEADLINE).unwrap_or(now);
        let move_result: Result<(f64, f64), MoveError> = async {
            debug!(rotator_id, angle, "moving rotator");
            rot.move_absolute(angle)
                .await
                .map_err(|e| format!("failed to move rotator: {e}"))?;

            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        // Stop-class counterpart (rp.md § In-Flight
                        // Tool Calls): halt the move where it is.
                        halt_rotator(rot.as_ref(), &rotator_id, "cancellation").await;
                        return Err(MoveError::Unsettled(cancel.error()));
                    }
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
                match rot.is_moving().await {
                    Ok(false) => break,
                    Ok(true) if Instant::now() < ceiling => {}
                    // Still moving at the ceiling: halted, as a slew's
                    // timeout is aborted (rp.md § Rotator Tool Details).
                    Ok(true) => {
                        halt_rotator(rot.as_ref(), &rotator_id, "its ceiling").await;
                        return Err(MoveError::Unsettled(
                            "timeout waiting for rotator to settle".to_string(),
                        ));
                    }
                    // Its state is unknown, so it may still be moving.
                    Err(e) => {
                        return Err(MoveError::Unsettled(format!(
                            "error polling rotator is_moving: {e}"
                        )))
                    }
                }
            }

            let position = rot
                .position()
                .await
                .map_err(|e| format!("failed to read rotator position: {e}"))?;
            let mechanical = rot
                .mechanical_position()
                .await
                .map_err(|e| format!("failed to read rotator mechanical position: {e}"))?;
            Ok((position, mechanical))
        }
        .await;

        match move_result {
            Ok((position, mechanical)) => {
                let mut guiding_ladder = serde_json::Value::Null;
                if let Some(client) = &ladder_client {
                    let delta_deg = angular_delta_deg(pre_move_angle.unwrap_or(position), position);
                    match self
                        .run_ladder_tail(client.as_ref(), found_state, delta_deg, cancel)
                        .await
                    {
                        Ok(outcome) => guiding_ladder = outcome,
                        Err(msg) => {
                            self.event_bus.emit_operation(EventEnvelope::failed(
                                "move_rotator",
                                &operation_id,
                                started_at,
                                &msg,
                            ));
                            drop(hold);
                            return Ok(tool_error!("{}", msg));
                        }
                    }
                }
                self.event_bus.emit_operation(EventEnvelope::complete(
                    "move_rotator",
                    &operation_id,
                    started_at,
                    serde_json::json!({
                        "rotator_id": rotator_id,
                        "angle": position,
                        "mechanical_angle": mechanical,
                        "moved_trains": moved_trains,
                        "guiding_ladder": guiding_ladder,
                    }),
                ));
                drop(hold);
                Ok(tool_success!({
                    "rotator_id": rotator_id,
                    "angle": position,
                    "mechanical_angle": mechanical,
                    "moved_trains": moved_trains,
                    "guiding_ladder": guiding_ladder,
                }))
            }
            Err(e) => {
                let (unsettled, mut msg) = match e {
                    MoveError::Unsettled(msg) => (true, msg),
                    MoveError::Failed(msg) => (false, msg),
                };
                // A failed move may still have rotated the field:
                // re-select and resume best-effort, reporting the move
                // error as primary (rp.md § Rotator Tool Details).
                if let Some(client) = ladder_client.as_ref().filter(|_| guiding_paused) {
                    if let Err(re) = client.reselect_star().await {
                        debug!(rotator_id, error = %re, "post-failure star re-select failed");
                    }
                    msg.push_str(&resume_note(client.as_ref()).await);
                }
                if unsettled {
                    wind_down_rotator(
                        rot,
                        hold,
                        self.event_bus.clone(),
                        UnsettledRotation {
                            rotator_id: rotator_id.clone(),
                            operation_id,
                            started_at,
                            error: msg.clone(),
                            ceiling,
                        },
                    );
                } else {
                    self.event_bus.emit_operation(EventEnvelope::failed(
                        "move_rotator",
                        &operation_id,
                        started_at,
                        &msg,
                    ));
                    drop(hold);
                }
                Ok(tool_error!("{}", msg))
            }
        }
    }

    /// Steps 3 and 4 of the rotate-while-guiding ladder, after a
    /// successful move: the calibration decision and, for a run the
    /// ladder paused, the way back to guiding. `found_state` is the
    /// guider's state before the move, `None` when it could not be
    /// read. Every failure is a hard tool error, its text the whole
    /// message: a night with guiding left paused, or with calibrations
    /// of the old angle still stored, must not look like success.
    async fn run_ladder_tail(
        &self,
        client: &dyn rp_guider::GuiderClient,
        found_state: Option<rp_guider::GuiderState>,
        delta_deg: f64,
        cancel: &Cancel,
    ) -> Result<serde_json::Value, String> {
        let paused_by_ladder = found_state == Some(rp_guider::GuiderState::Guiding);
        let threshold = self.guider_defaults.recalibrate_above_deg;
        if delta_deg <= threshold {
            // The calibrations still hold. The turn moved the stars,
            // so a run the ladder paused takes new ones before its
            // corrections resume; a failed re-selection still resumes.
            if paused_by_ladder {
                let reselected = client.reselect_star().await;
                let resumed = client.resume_guiding().await;
                reselected.map_err(|e| {
                    format!("move_rotator: failed to re-select the guide stars: {e}")
                })?;
                resumed.map_err(|e| {
                    format!("move_rotator: failed to resume guiding after rotating: {e}")
                })?;
            }
            return Ok(serde_json::json!({
                "delta_deg": delta_deg,
                "calibration_cleared": false,
                "guiding_restarted": false,
            }));
        }

        // The calibrations are of another angle now. The guider keeps
        // them from run to run, so they go whether it guides or not,
        // and it refuses the clear in a run state, so a run stops
        // first.
        if found_state.is_some_and(rp_guider::GuiderState::is_run) {
            self.stop_guiding_run(client).await.map_err(|e| {
                format!(
                    "move_rotator: failed to stop guiding before clearing the calibrations: {e}"
                )
            })?;
        }
        client
            .clear_calibration()
            .await
            .map_err(|e| format!("move_rotator: failed to clear the guider's calibrations: {e}"))?;
        debug!(
            delta_deg,
            threshold, "cleared the guider's calibrations after rotation"
        );
        if paused_by_ladder {
            // The start calibrates at the new angle.
            let request = rp_guider::StartGuidingRequest {
                recalibrate: false,
                settle: self.merge_settle(None, None, None),
            };
            self.start_guiding_run("move_rotator", client, request, cancel)
                .await?;
        }
        Ok(serde_json::json!({
            "delta_deg": delta_deg,
            "calibration_cleared": true,
            "guiding_restarted": paused_by_ladder,
        }))
    }

    #[tool(
        description = "Read the rotator's sky angle, mechanical angle, and motion state. Address as rotator_id or train_id (the train's sole rotator). Cheap; safe to poll."
    )]
    pub(crate) async fn get_rotator_position(
        &self,
        Parameters(params): Parameters<RotatorPositionParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rotator_id = match self.resolve_rotator_addressing(
            "get_rotator_position",
            params.rotator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(result) => return Ok(*result),
        };
        let (_entry, rot) = resolve_device!(self, find_rotator, &rotator_id, "rotator");

        let position = match rot.position().await {
            Ok(p) => p,
            Err(e) => return Ok(tool_error!("failed to read rotator position: {}", e)),
        };
        let mechanical = match rot.mechanical_position().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(tool_error!(
                    "failed to read rotator mechanical position: {}",
                    e
                ))
            }
        };
        let is_moving = match rot.is_moving().await {
            Ok(m) => m,
            Err(e) => return Ok(tool_error!("failed to read rotator is_moving: {}", e)),
        };
        Ok(tool_success!({
            "rotator_id": rotator_id,
            "angle": position,
            "mechanical_angle": mechanical,
            "is_moving": is_moving,
        }))
    }
}

/// Resume the corrections the ladder paused on a path that already
/// has an error to report: empty when the resume succeeds, else the
/// clause that goes on the end of that error.
async fn resume_note(client: &dyn rp_guider::GuiderClient) -> String {
    match client.resume_guiding().await {
        Ok(_) => String::new(),
        Err(e) => format!("; also failed to resume guiding: {e}"),
    }
}

/// Shortest angular distance between two sky angles, folded to
/// `[0°, 180°]` — a rotator delta never exceeds a half turn by
/// shortest path — and rounded to 0.01°. The difference of two decimal
/// angles carries binary residue: 8.05 − 3.05 computes as
/// 5.000000000000001, which the calibration decision would read as
/// above a 5° threshold (rp.md § Rotator Tool Details).
fn angular_delta_deg(a: f64, b: f64) -> f64 {
    let d = (a - b).abs() % 360.0;
    let folded = if d > 180.0 { 360.0 - d } else { d };
    (folded * 100.0).round() / 100.0
}

impl McpHandler {
    /// Resolve the `rotator_id` / `train_id` addressing shared by both
    /// rotator tools: exactly one must be present, and a train must
    /// contain exactly one rotator. Returns the resolved roster id, or
    /// the ready-to-return error `CallToolResult` (boxed —
    /// `clippy::result_large_err`).
    fn resolve_rotator_addressing(
        &self,
        tool: &str,
        rotator_id: Option<&str>,
        train_id: Option<&str>,
    ) -> Result<String, Box<CallToolResult>> {
        match (rotator_id, train_id) {
            (Some(_), Some(_)) | (None, None) => Err(Box::new(tool_error!(
                "{}: pass exactly one of rotator_id or train_id",
                tool
            ))),
            (Some(id), None) => Ok(id.to_string()),
            (None, Some(train_id)) => {
                let Some(train) = self.trains.train(train_id) else {
                    return Err(Box::new(tool_error!("train not found: {}", train_id)));
                };
                let rotators: Vec<&str> = train
                    .devices
                    .iter()
                    .filter(|d| d.kind == TrainDeviceKind::Rotator)
                    .map(|d| d.id.as_str())
                    .collect();
                match rotators.as_slice() {
                    [] => Err(Box::new(tool_error!("train '{}' has no rotator", train_id))),
                    [id] => Ok((*id).to_string()),
                    many => Err(Box::new(tool_error!(
                        "train '{}' has {} rotators; pass rotator_id",
                        train_id,
                        many.len()
                    ))),
                }
            }
        }
    }
}

/// Halt a rotator whose move rp ends on `why`. A failed halt may have
/// left it moving, which the wind-down then waits out.
async fn halt_rotator(rot: &dyn Rotator, rotator_id: &str, why: &str) {
    if let Err(e) = rot.halt().await {
        warn!(rotator_id, why, error = %e, "rotator halt failed; the rotator may still be moving");
    }
}

/// What a rotator move that may have left the rotator moving reports
/// when its wind-down ends: the `move_rotator_failed` that closes the
/// move's triple, and the move's ceiling.
struct UnsettledRotation {
    rotator_id: String,
    operation_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    error: String,
    ceiling: Instant,
}

/// Wind down a rotator move that may have left the rotator moving
/// (rp.md § Rotator Tool Details, Kept while a move that may still be
/// moving winds down): keep the rotator's slot until the rotator reads
/// not moving, and at most until [`wind_down_bound`]. Then emit
/// `move_rotator_failed` carrying the angles it came to rest at, and
/// give the slot back, so a rotation queued behind this one starts
/// from a rotator at rest.
fn wind_down_rotator(
    rot: Arc<dyn Rotator>,
    hold: DeviceHold,
    event_bus: Arc<EventBus>,
    unsettled: UnsettledRotation,
) {
    let bound = wind_down_bound(unsettled.ceiling);
    tokio::spawn(async move {
        let rotator_id = unsettled.rotator_id.as_str();
        let still = wait_until_still("rotator", rotator_id, bound, || {
            let rot = rot.clone();
            async move { rot.is_moving().await }
        })
        .await;
        let (angle, mechanical_angle) = if still {
            let angle = rot.position().await;
            let mechanical = rot.mechanical_position().await;
            debug!(
                rotator_id,
                ?angle,
                ?mechanical,
                "the rotator has stopped after its move; its slot is free"
            );
            (angle.ok(), mechanical.ok())
        } else {
            (None, None)
        };
        let mut failed = EventEnvelope::failed(
            "move_rotator",
            &unsettled.operation_id,
            unsettled.started_at,
            &unsettled.error,
        );
        if let Some(payload) = failed.payload.as_object_mut() {
            payload.insert("angle".to_string(), angle.into());
            payload.insert("mechanical_angle".to_string(), mechanical_angle.into());
        }
        // Emitted before the slot goes back, so it precedes the
        // `move_rotator_started` of a rotation queued behind this one.
        event_bus.emit_operation(failed);
        drop(hold);
    });
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::angular_delta_deg;

    #[test]
    fn angular_delta_folds_to_the_shortest_path() {
        for (a, b, expected) in [
            (0.0, 10.0, 10.0),
            (10.0, 0.0, 10.0),
            (350.0, 10.0, 20.0),
            (0.0, 180.0, 180.0),
            (0.0, 181.0, 179.0),
            (90.0, 90.0, 0.0),
        ] {
            let delta = angular_delta_deg(a, b);
            assert_eq!(delta, expected, "delta({a}, {b})");
        }
    }

    #[test]
    fn angular_delta_is_exact_to_a_hundredth() {
        // Unrounded, these read 5.000000000000001, 4.189999999999998 and
        // 19.99000000000001.
        for (a, b, expected) in [
            (3.05, 8.05, 5.0),
            (152.0, 147.81, 4.19),
            (10.0, 350.01, 19.99),
        ] {
            assert_eq!(angular_delta_deg(a, b), expected, "delta({a}, {b})");
        }
    }
}
