//! Rotator tool category: `move_rotator`, `get_rotator_position`
//! (rp.md § Rotator Tool Details).
//!
//! Both tools address the device as `rotator_id` *or* `train_id` —
//! exactly one — where `train_id` resolves through the optical-train
//! model and requires the train to contain exactly one rotator.
//! `move_rotator` moves to an absolute **sky** angle (the ASCOM
//! `Position` frame) and blocks polling `IsMoving` until idle under a
//! fixed 120 s ceiling: there is no per-rotator rate config yet, so
//! the `move_rotator_started` envelope carries no predictive
//! deadline. `moved_trains` on the result lists every train
//! containing the rotator; when it includes the guiding train and
//! guiding is active, the move runs the rotate-while-guiding ladder
//! (pause corrections → move → calibration decision → re-select the
//! star → resume).

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::{resolve_device, tool_error, tool_success};
use crate::equipment::trains::TrainDeviceKind;
use crate::events::EventEnvelope;

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
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rotator_id = match self.resolve_rotator_addressing(
            "move_rotator",
            params.rotator_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(result) => return Ok(*result),
        };
        let angle = match params.angle {
            Some(a) => a,
            None => return Ok(tool_error!("missing required parameter: angle")),
        };
        if !angle.is_finite() || !(0.0..360.0).contains(&angle) {
            return Ok(tool_error!(
                "move_rotator: angle out of range: {} (expected 0.0 <= angle < 360.0)",
                angle
            ));
        }
        let (_entry, rot) = resolve_device!(self, find_rotator, &rotator_id, "rotator");
        let moved_trains: Vec<String> = self
            .trains
            .trains_with_device(&rotator_id)
            .iter()
            .map(|t| t.id.clone())
            .collect();

        // Rotate-while-guiding ladder decision (rp.md § Rotator Tool
        // Details): engage when the move rotates the guiding train,
        // the guider is configured, and it reports an active guide
        // loop. A stats read that fails or reports not-guiding runs
        // the move bare — a mid-day rotation must not fail because
        // PHD2 is closed (Tenet 2).
        let rotates_guiding = self
            .trains
            .guiding_train()
            .is_some_and(|g| moved_trains.contains(&g.id));
        let mut ladder_client = None;
        if rotates_guiding {
            if let Some(client) = self.guider.clone() {
                match client.guiding_stats().await {
                    Ok(stats) if stats.guiding => ladder_client = Some(client),
                    Ok(_) => debug!(rotator_id, "guider not guiding; rotating bare"),
                    Err(e) => {
                        debug!(rotator_id, error = %e, "guider stats unreachable; rotating bare");
                    }
                }
            }
        }

        // The operation triple starts here: the ladder's pause and
        // pre-move read are part of the move (they issue RPCs on its
        // behalf), so their failures must surface as
        // `move_rotator_failed` to event subscribers, not as bare
        // tool errors. Rotator moves carry no predictive deadline, so
        // emitting `*_started` ahead of the pause costs nothing.
        let guiding_paused = ladder_client.is_some();
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
            match rot.position().await {
                Ok(a) => pre_move_angle = Some(a),
                Err(e) => {
                    let resume_note = match client.resume_guiding().await {
                        Ok(()) => String::new(),
                        Err(re) => format!("; also failed to resume guiding: {re}"),
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

        let move_result: Result<(f64, f64), String> = async {
            debug!(rotator_id, angle, "moving rotator");
            rot.move_absolute(angle)
                .await
                .map_err(|e| format!("failed to move rotator: {e}"))?;

            let now = std::time::Instant::now();
            // Unreachable overflow degrades to an expired deadline, not a panic.
            let deadline = now.checked_add(ROTATOR_MOVE_DEADLINE).unwrap_or(now);
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                match rot.is_moving().await {
                    Ok(false) => break,
                    Ok(true) if std::time::Instant::now() < deadline => continue,
                    Ok(true) => return Err("timeout waiting for rotator to settle".to_string()),
                    Err(e) => return Err(format!("error polling rotator is_moving: {e}")),
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
                    let pre = pre_move_angle.unwrap_or(position);
                    match self.run_ladder_tail(client.as_ref(), pre, position).await {
                        Ok(outcome) => guiding_ladder = outcome,
                        Err(e) => {
                            let msg = format!("move_rotator: {e}");
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
                Ok(tool_success!({
                    "rotator_id": rotator_id,
                    "angle": position,
                    "mechanical_angle": mechanical,
                    "moved_trains": moved_trains,
                    "guiding_ladder": guiding_ladder,
                }))
            }
            Err(e) => {
                // A failed move may still have rotated the field:
                // re-select and resume best-effort, reporting the move
                // error as primary (rp.md § Rotator Tool Details).
                let mut msg = e;
                if let Some(client) = &ladder_client {
                    if let Err(re) = client.reselect_star().await {
                        debug!(rotator_id, error = %re, "post-failure star re-select failed");
                    }
                    if let Err(re) = client.resume_guiding().await {
                        msg = format!("{msg}; also failed to resume guiding: {re}");
                    }
                }
                self.event_bus.emit_operation(EventEnvelope::failed(
                    "move_rotator",
                    &operation_id,
                    started_at,
                    &msg,
                ));
                Ok(tool_error!("{}", msg))
            }
        }
    }

    /// Steps 3–4 of the rotate-while-guiding ladder, after a
    /// successful move: the calibration decision, the star
    /// re-selection, and the resume. Any failure attempts a resume
    /// and surfaces as a hard tool error — a night with guiding
    /// silently left paused must not look like success.
    async fn run_ladder_tail(
        &self,
        client: &dyn rp_guider::GuiderClient,
        pre_move_angle: f64,
        post_move_angle: f64,
    ) -> Result<serde_json::Value, String> {
        let tail: Result<serde_json::Value, String> = async {
            let equipment = client
                .current_equipment()
                .await
                .map_err(|e| format!("failed to read PHD2 equipment: {e}"))?;
            let phd2_has_rotator = equipment.rotator.is_some_and(|r| r.connected);
            let delta_deg = angular_delta_deg(pre_move_angle, post_move_angle);
            let threshold = self.guider_defaults.recalibrate_above_deg;
            let calibration_cleared = if !phd2_has_rotator && delta_deg > threshold {
                client
                    .clear_calibration()
                    .await
                    .map_err(|e| format!("failed to clear the PHD2 calibration: {e}"))?;
                debug!(
                    delta_deg,
                    threshold, "cleared PHD2 calibration after rotation"
                );
                true
            } else {
                false
            };
            client
                .reselect_star()
                .await
                .map_err(|e| format!("failed to re-select the guide star: {e}"))?;
            Ok(serde_json::json!({
                "phd2_has_rotator": phd2_has_rotator,
                "delta_deg": delta_deg,
                "calibration_cleared": calibration_cleared,
            }))
        }
        .await;

        match tail {
            Ok(outcome) => {
                client
                    .resume_guiding()
                    .await
                    .map_err(|e| format!("failed to resume guiding after rotating: {e}"))?;
                Ok(outcome)
            }
            Err(e) => {
                let resume_note = match client.resume_guiding().await {
                    Ok(()) => String::new(),
                    Err(re) => format!("; also failed to resume guiding: {re}"),
                };
                Err(format!("{e}{resume_note}"))
            }
        }
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

/// Shortest angular distance between two sky angles, folded to
/// `[0°, 180°]` — a rotator delta never exceeds a half turn by
/// shortest path.
fn angular_delta_deg(a: f64, b: f64) -> f64 {
    let d = (a - b).abs() % 360.0;
    if d > 180.0 {
        360.0 - d
    } else {
        d
    }
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
            assert!(
                (delta - expected).abs() < 1e-9,
                "delta({a}, {b}) = {delta}, expected {expected}"
            );
        }
    }
}
