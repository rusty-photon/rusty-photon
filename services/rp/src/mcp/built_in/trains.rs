//! Optical-train tool category: `get_train_info` (rp.md § Optical
//! Trains).
//!
//! A read over the validated train model — no device is touched. It
//! exists so a tool provider addressed by `train_id` (calibrator-flats
//! plan, D4) can learn what a train contains without `rp` handing out
//! the whole config: the terminal camera, the sole filter wheel with
//! its configured filter names in position order, the cover
//! calibrator, the focusers and the sole rotator, plus `purpose` and
//! `focal_length_mm`. `rp` stays the only owner of the train model.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};
use crate::equipment::trains::TrainDeviceKind;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTrainInfoParams {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
}

#[tool_router(router = tool_router_trains, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Describe an optical train without touching any device: its terminal camera_id, the sole filter wheel (filter_wheel_id plus filters, the configured names in position order; both null when the train has none or several), calibrator_id (null when none), focusers in optical order, the sole rotator_id (null when none or several), purpose, focal_length_mm and the ordered devices list"
    )]
    pub(crate) async fn get_train_info(
        &self,
        Parameters(params): Parameters<GetTrainInfoParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(train) = self.trains.train(&params.train_id) else {
            return Ok(tool_error!("train not found: {}", params.train_id));
        };

        let filter_wheel_id = train.sole_of_kind(TrainDeviceKind::FilterWheel);
        // The names are config facts (`filter_wheels[].filters`), the
        // same list `set_filter` resolves a name against — the roster
        // and the train model come from one config, so the wheel is
        // always registered; an empty list is a wheel configured
        // without names.
        let filters: Option<Vec<String>> = filter_wheel_id.map(|id| {
            self.equipment
                .find_filter_wheel(id)
                .map(|entry| entry.config.filters.clone())
                .unwrap_or_default()
        });

        let devices: Vec<serde_json::Value> = train
            .devices
            .iter()
            .map(|d| serde_json::json!({ "id": d.id, "kind": d.kind.name() }))
            .collect();

        debug!(train_id = %params.train_id, "described optical train");
        Ok(tool_success!({
            "train_id": train.id,
            "purpose": train.purpose,
            "focal_length_mm": train.focal_length_mm,
            "camera_id": train.camera_id(),
            "filter_wheel_id": filter_wheel_id,
            "filters": filters,
            "calibrator_id": train.calibrator_id(),
            "focusers": train.ids_of_kind(TrainDeviceKind::Focuser),
            "rotator_id": train.sole_of_kind(TrainDeviceKind::Rotator),
            "devices": devices,
        }))
    }
}
