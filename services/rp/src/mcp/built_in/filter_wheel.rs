use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::equipment::trains::TrainDeviceKind;

use super::super::handler::McpHandler;
use super::super::{resolve_device, tool_error, tool_success};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(extend("oneOf" = [{"required": ["filter_wheel_id"]}, {"required": ["train_id"]}]))]
pub struct SetFilterParams {
    /// Filter wheel device ID; mutually exclusive with `train_id`.
    #[serde(default)]
    pub filter_wheel_id: Option<String>,
    /// Optical train whose sole filter wheel changes; mutually
    /// exclusive with `filter_wheel_id`.
    #[serde(default)]
    pub train_id: Option<String>,
    pub filter_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FilterWheelIdParams {
    pub filter_wheel_id: String,
}

#[tool_router(router = tool_router_filter_wheel, vis = "pub")]
impl McpHandler {
    #[tool(description = "Set the active filter on a filter wheel")]
    pub(crate) async fn set_filter(
        &self,
        Parameters(params): Parameters<SetFilterParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let filter_wheel_id = match self.resolve_filter_wheel_addressing(
            params.filter_wheel_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        let (fw_entry, fw) =
            resolve_device!(self, find_filter_wheel, &filter_wheel_id, "filter wheel");

        let position = match fw_entry
            .config
            .filters
            .iter()
            .position(|f| f == &params.filter_name)
        {
            Some(p) => p,
            None => return Ok(tool_error!("filter not found: {}", params.filter_name)),
        };

        if let Err(e) = fw.set_position(position).await {
            return Ok(tool_error!("failed to set filter position: {}", e));
        }

        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match fw.position().await {
                Ok(Some(p)) if p == position => break,
                Ok(Some(_) | None) => {}
                Err(e) => {
                    return Ok(tool_error!("error waiting for filter wheel: {}", e));
                }
            }
        }

        self.event_bus.emit(
            "filter_switch",
            serde_json::json!({
                "filter_wheel_id": filter_wheel_id,
                "filter_name": params.filter_name,
            }),
        );

        Ok(tool_success!({
            "filter_wheel_id": filter_wheel_id,
            "filter_name": params.filter_name,
            "position": position,
        }))
    }

    #[tool(description = "Get the current filter on a filter wheel")]
    pub(crate) async fn get_filter(
        &self,
        Parameters(params): Parameters<FilterWheelIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let (fw_entry, fw) = resolve_device!(
            self,
            find_filter_wheel,
            &params.filter_wheel_id,
            "filter wheel"
        );

        let position = match fw.position().await {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(tool_error!("filter wheel is moving")),
            Err(e) => {
                return Ok(tool_error!("failed to get filter position: {}", e));
            }
        };

        let filter_name = fw_entry
            .config
            .filters
            .get(position)
            .cloned()
            .unwrap_or_else(|| format!("Filter {position}"));

        Ok(tool_success!({
            "filter_wheel_id": params.filter_wheel_id,
            "filter_name": filter_name,
            "position": position,
        }))
    }
}

impl McpHandler {
    /// Resolve `set_filter`'s `filter_wheel_id` / `train_id`
    /// addressing: exactly one must be present, and a train must
    /// contain exactly one filter wheel (the sole-rotator rule of the
    /// rotator tools, applied to wheels). Returns the resolved roster
    /// id, or the ready-to-return error `CallToolResult` (boxed —
    /// `clippy::result_large_err`).
    fn resolve_filter_wheel_addressing(
        &self,
        filter_wheel_id: Option<&str>,
        train_id: Option<&str>,
    ) -> Result<String, Box<CallToolResult>> {
        match (filter_wheel_id, train_id) {
            (Some(_), Some(_)) => Err(Box::new(tool_error!(
                "set_filter: train_id is mutually exclusive with filter_wheel_id"
            ))),
            (None, None) => Err(Box::new(tool_error!(
                "set_filter: pass exactly one of filter_wheel_id or train_id"
            ))),
            (Some(id), None) => Ok(id.to_string()),
            (None, Some(train_id)) => {
                let Some(train) = self.trains.train(train_id) else {
                    return Err(Box::new(tool_error!("train not found: {}", train_id)));
                };
                let wheels: Vec<&str> = train
                    .devices
                    .iter()
                    .filter(|d| d.kind == TrainDeviceKind::FilterWheel)
                    .map(|d| d.id.as_str())
                    .collect();
                match wheels.as_slice() {
                    [] => Err(Box::new(tool_error!(
                        "train '{}' has no filter wheel",
                        train_id
                    ))),
                    [id] => Ok((*id).to_string()),
                    many => Err(Box::new(tool_error!(
                        "train '{}' has {} filter wheels; pass filter_wheel_id",
                        train_id,
                        many.len()
                    ))),
                }
            }
        }
    }
}
