//! Optical-train tool category: `get_train_info` and
//! `get_refocus_plan` (rp.md § Optical Trains, § Train optics,
//! § `get_refocus_plan` Contract).
//!
//! Reads over the validated train model — no device is touched. They
//! exist so a tool provider addressed by `train_id` (the calibrator
//! and focus providers) can learn what a train contains, what its
//! optics are, and which focusers a refocus of it touches in what
//! order, without `rp` handing out the whole config. `rp` stays the
//! only owner of the train model.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};
use crate::config::focuser::MicronsPerStep;
use crate::equipment::trains::{Train, TrainDeviceKind};

/// The plate-scale constant: arcseconds per radian over a thousand,
/// so that microns over millimetres come out in arcseconds.
const ARCSEC_PER_MM_PER_UM: f64 = 206.265;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTrainInfoParams {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetRefocusPlanParams {
    /// An `equipment.optical_trains[]` id.
    pub train_id: String,
}

#[tool_router(router = tool_router_trains, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Describe an optical train without touching any device: its terminal camera_id, the sole filter wheel (filter_wheel_id plus filters, the configured names in position order, plus filter_wavelengths_nm mapping each name to its configured wavelength or null; all three null when the train has none or several), calibrator_id (null when none), focusers in optical order plus terminal_focuser_id (the last of them, the one the train's own auto_focus sweeps and whose probe a temperature_changed names for this train; null without a focuser), the sole rotator_id (null when none or several), purpose, focal_length_mm, the ordered devices list, and optics — focal_length_mm, aperture_mm, focal_ratio, pixel_size_um (the terminal camera's x pixel), pixel_scale_arcsec_per_pixel and microns_per_step (configured, else the terminal focuser's StepSize), each null when unknown"
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
        let wheel = filter_wheel_id.and_then(|id| self.equipment.find_filter_wheel(id));
        let filters: Option<Vec<String>> = filter_wheel_id.map(|_| {
            wheel
                .map(|entry| entry.config.filter_names())
                .unwrap_or_default()
        });
        let filter_wavelengths_nm: Option<serde_json::Value> = filter_wheel_id.map(|_| {
            let map: serde_json::Map<String, serde_json::Value> = wheel
                .map(|entry| {
                    entry
                        .config
                        .filters
                        .iter()
                        .map(|f| (f.name().to_string(), serde_json::json!(f.wavelength_nm())))
                        .collect()
                })
                .unwrap_or_default();
            serde_json::Value::Object(map)
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
            "filter_wavelengths_nm": filter_wavelengths_nm,
            "calibrator_id": train.calibrator_id(),
            "focusers": train.ids_of_kind(TrainDeviceKind::Focuser),
            "terminal_focuser_id": train.terminal_focuser(),
            "rotator_id": train.sole_of_kind(TrainDeviceKind::Rotator),
            "devices": devices,
            "optics": self.train_optics(train),
        }))
    }

    #[tool(
        description = "The dependency-ordered refocus sequence of an optical train, as a read — nothing moves: steps of {focuser_id, run_train_id, camera_id, metric} in run order (the train's shared focusers upstream-first, each run in the train where it is terminal, then the train's own focuser; a guiding-train step is last with metric \"guide\" and a null camera_id, every other step metric \"capture\" through the run train's camera), and guide_coupled, whether a capture step moves a focuser the guiding train shares (a walker pauses guide corrections around it). An unknown train, or one without focusers, is an error naming it"
    )]
    pub(crate) async fn get_refocus_plan(
        &self,
        Parameters(params): Parameters<GetRefocusPlanParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(plan) = self.trains.refocus_plan(&params.train_id) else {
            return Ok(tool_error!("train not found: {}", params.train_id));
        };
        if plan.steps.is_empty() {
            return Ok(tool_error!("train '{}' has no focusers", params.train_id));
        }
        debug!(
            train_id = %params.train_id,
            steps = plan.steps.len(),
            guide_coupled = plan.guide_coupled,
            "derived the refocus plan"
        );
        Ok(tool_success!(plan))
    }

    /// The train's optics block (rp.md § Train optics): the configured
    /// facts, the terminal camera's and focuser's connect-time reads,
    /// and the two derivations — each `null` when an input is unknown.
    fn train_optics(&self, train: &Train) -> serde_json::Value {
        let pixel_size_um = train
            .camera_id()
            .and_then(|id| self.equipment.find_camera(id))
            .and_then(|entry| entry.invariants().pixel_size_x_um)
            .filter(|px| px.is_finite() && *px > 0.0);
        let microns_per_step = train
            .terminal_focuser()
            .and_then(|id| self.equipment.find_focuser(id))
            .and_then(|entry| {
                entry
                    .config
                    .microns_per_step
                    .map(MicronsPerStep::value)
                    .or_else(|| entry.invariants().step_size_um)
            });
        let focal_ratio = match (train.focal_length_mm, train.aperture_mm) {
            (Some(focal_length), Some(aperture)) => Some(focal_length / aperture),
            _ => None,
        };
        let pixel_scale_arcsec_per_pixel = match (pixel_size_um, train.focal_length_mm) {
            (Some(px), Some(focal_length)) => Some(ARCSEC_PER_MM_PER_UM * px / focal_length),
            _ => None,
        };
        serde_json::json!({
            "focal_length_mm": train.focal_length_mm,
            "aperture_mm": train.aperture_mm,
            "focal_ratio": focal_ratio,
            "pixel_size_um": pixel_size_um,
            "pixel_scale_arcsec_per_pixel": pixel_scale_arcsec_per_pixel,
            "microns_per_step": microns_per_step,
        })
    }
}
