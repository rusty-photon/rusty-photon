use std::time::Duration;

use ascom_alpaca::api::FilterWheel;
use ascom_alpaca::ASCOMResult;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_router, RoleServer};
use schemars::JsonSchema;
use serde::Deserialize;
use tracing::debug;

use crate::equipment::trains::TrainDeviceKind;

use super::super::handler::McpHandler;
use super::super::inflight::Cancel;
use super::super::{resolve_device, tool_error, tool_success};

/// How often `set_filter` reads the wheel while it waits for a move to
/// end: the one already under way, then its own.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The longest `set_filter` gives a move, each wait its own: the one
/// already under way before it writes all the same, and its own before
/// the call fails. A full turn of a sixteen-slot wheel at ~1.2 s a slot
/// takes ~19 s, and this is three times that. A wheel still moving past
/// it is not finishing a move.
pub(crate) const MOVE_CEILING: Duration = Duration::from_secs(60);

/// One poll interval, or the tool's answer if the call is cancelled
/// first.
async fn pause(cancel: &Cancel) -> Option<CallToolResult> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Some(tool_error!("{}", cancel.error())),
        () = tokio::time::sleep(POLL_INTERVAL) => None,
    }
}

/// The wheel's `Position`, or `None` if the call is cancelled before or
/// while it is read: a read can take a serial round trip, or an HTTP
/// request's whole timeout.
async fn read(fw: &dyn FilterWheel, cancel: &Cancel) -> Option<ASCOMResult<Option<usize>>> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        reading = fw.position() => Some(reading),
    }
}

/// Wait, before a write, for a move under way to end: until the wheel
/// reads a slot, a read errors, or the move has had [`MOVE_CEILING`]
/// since `started`. `Some` is the tool's answer to a cancellation.
async fn wait_for_rest(
    fw: &dyn FilterWheel,
    cancel: &Cancel,
    started: tokio::time::Instant,
    filter_wheel_id: &str,
) -> Option<CallToolResult> {
    let mut waiting = false;
    loop {
        let Some(reading) = read(fw, cancel).await else {
            return Some(tool_error!("{}", cancel.error()));
        };
        match reading {
            Ok(None) if started.elapsed() < MOVE_CEILING => {
                if !waiting {
                    debug!(
                        filter_wheel_id,
                        "the filter wheel is moving; waiting for its move to end before the write"
                    );
                    waiting = true;
                }
            }
            Ok(None) => {
                debug!(filter_wheel_id, ceiling = ?MOVE_CEILING, "the filter wheel still reads moving; writing all the same");
                return None;
            }
            Ok(Some(slot)) => {
                if waiting {
                    debug!(filter_wheel_id, slot, "the move under way has ended");
                }
                return None;
            }
            // The write says what is wrong with the wheel, and a driver
            // may answer a failed move with an error until a write goes
            // out.
            Err(e) => {
                debug!(filter_wheel_id, error = %e, "the filter wheel could not be read before the write; writing all the same");
                return None;
            }
        }
        if let Some(cancelled) = pause(cancel).await {
            return Some(cancelled);
        }
    }
}

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
    /// A held train lease to change the filter under (docs/services/rp.md
    /// § Train Lease): admitted only for a wheel in the lease's train.
    /// Omitted, a filter change of a wheel a lease reserves waits for
    /// the lease to end.
    #[serde(default)]
    pub lease_id: Option<String>,
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
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cancel = Cancel::from_context(&ctx);
        self.set_filter_inner(params, &cancel).await
    }

    /// Body of the `set_filter` MCP tool, split out so unit tests can pass
    /// a never-cancelled handle without constructing a real rmcp
    /// `RequestContext`.
    pub(crate) async fn set_filter_inner(
        &self,
        params: SetFilterParams,
        cancel: &Cancel,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let filter_wheel_id = match self.resolve_filter_wheel_addressing(
            params.filter_wheel_id.as_deref(),
            params.train_id.as_deref(),
        ) {
            Ok(id) => id,
            Err(e) => return Ok(*e),
        };
        // One filter change through a wheel at a time, held from the
        // first read to the end of the wait for the move (rp.md § Filter
        // Wheel Tool Details), or the lease's hold on the wheel.
        let slot = self
            .equipment
            .find_filter_wheel(&filter_wheel_id)
            .map(|entry| &entry.slot);
        let _hold = match self
            .device_hold(
                ("filter wheel", &filter_wheel_id),
                slot,
                params.lease_id.as_deref(),
                cancel,
            )
            .await
        {
            Ok(hold) => hold,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let (fw_entry, fw) =
            resolve_device!(self, find_filter_wheel, &filter_wheel_id, "filter wheel");

        let Some(position) = fw_entry.config.filter_position(&params.filter_name) else {
            return Ok(tool_error!("filter not found: {}", params.filter_name));
        };

        // A wheel written while it travels does not take the new slot
        // (rp.md § Filter Wheel Tool Details), so a move under way —
        // another client's, or one a cancelled call left going — is
        // waited out before the write. One that begins between the read
        // and the write gets the write refused, and is waited out too.
        let started = tokio::time::Instant::now();
        loop {
            if let Some(cancelled) =
                wait_for_rest(fw.as_ref(), cancel, started, &filter_wheel_id).await
            {
                return Ok(cancelled);
            }

            // Written even when the wheel already names the slot: a
            // driver's `Position` can be stale, and the write is the
            // command. Raced against the cancellation like the reads; a
            // write cut short may still reach the wheel, which then
            // finishes its move on its own.
            let written = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(tool_error!("{}", cancel.error())),
                written = fw.set_position(position) => written,
            };
            let Err(e) = written else {
                break;
            };
            // Refused by a wheel that is moving, as by one whose move
            // began after the read or that the read could not see.
            if started.elapsed() < MOVE_CEILING {
                let Some(reading) = read(fw.as_ref(), cancel).await else {
                    return Ok(tool_error!("{}", cancel.error()));
                };
                if matches!(reading, Ok(None)) {
                    debug!(%filter_wheel_id, error = %e, "the write was refused by a filter wheel that is moving; waiting for it to rest");
                    if let Some(cancelled) = pause(cancel).await {
                        return Ok(cancelled);
                    }
                    continue;
                }
            }
            return Ok(tool_error!("failed to set filter position: {}", e));
        }

        let sent = tokio::time::Instant::now();
        loop {
            // No stop-class counterpart for a wheel (rp.md § In-Flight
            // Tool Calls): the wait ends, the wheel finishes its move.
            if let Some(cancelled) = pause(cancel).await {
                return Ok(cancelled);
            }
            let Some(reading) = read(fw.as_ref(), cancel).await else {
                return Ok(tool_error!("{}", cancel.error()));
            };
            match reading {
                Ok(Some(p)) if p == position => break,
                Ok(Some(_) | None) if sent.elapsed() < MOVE_CEILING => {}
                // Not there by the ceiling: another client's write may
                // have sent it elsewhere, or the move was lost.
                Ok(reading) => {
                    debug!(%filter_wheel_id, ?reading, ceiling = ?MOVE_CEILING, "the filter wheel did not reach the slot written");
                    return Ok(tool_error!(
                        "the filter wheel did not reach {} (slot {position}) within {MOVE_CEILING:?}",
                        params.filter_name
                    ));
                }
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

        let filter_name = filter_name_at(fw_entry, position);

        Ok(tool_success!({
            "filter_wheel_id": params.filter_wheel_id,
            "filter_name": filter_name,
            "position": position,
        }))
    }
}

/// The configured name of a wheel slot, or `Filter <n>` for a slot the
/// config does not name.
fn filter_name_at(entry: &crate::equipment::FilterWheelEntry, position: usize) -> String {
    entry
        .config
        .filter_name_at(position)
        .map_or_else(|| format!("Filter {position}"), str::to_string)
}

impl McpHandler {
    /// The filter currently in the wheel the planner's filter-batching
    /// tie-break reads (rp.md § Dynamic Planner → Decision Logic bullet
    /// 4): the named imaging train's sole filter wheel, or — with no
    /// train named — the rig's only configured wheel, read with the
    /// same `Position` call `get_filter` makes. `None` — one `debug!`
    /// line, never an error — when there is no such wheel (a filterless
    /// rig, a train with none or several), the wheel is not connected,
    /// is mid-move, or the read fails: the tie-break is then neutral.
    pub(crate) async fn current_filter_for_planner(
        &self,
        train_id: Option<&str>,
    ) -> Option<String> {
        let wheel_id = match train_id {
            Some(train_id) => {
                let Ok(id) = self.resolve_filter_wheel_addressing(None, Some(train_id)) else {
                    tracing::debug!(
                        train_id,
                        "train has no sole filter wheel; the filter tie-break is neutral"
                    );
                    return None;
                };
                id
            }
            None => match self.equipment.filter_wheels.as_slice() {
                [wheel] => wheel.id.clone(),
                wheels => {
                    tracing::debug!(
                        wheels = wheels.len(),
                        "no single filter wheel to read; the filter tie-break is neutral"
                    );
                    return None;
                }
            },
        };
        let Some(entry) = self.equipment.find_filter_wheel(&wheel_id) else {
            tracing::debug!(filter_wheel_id = %wheel_id, "filter wheel not in the roster; the filter tie-break is neutral");
            return None;
        };
        let Some(fw) = entry.device() else {
            tracing::debug!(filter_wheel_id = %wheel_id, "filter wheel not connected; the filter tie-break is neutral");
            return None;
        };
        match fw.position().await {
            Ok(Some(position)) => Some(filter_name_at(entry, position)),
            Ok(None) => {
                tracing::debug!(filter_wheel_id = %wheel_id, "filter wheel is moving; the filter tie-break is neutral");
                None
            }
            Err(e) => {
                tracing::debug!(filter_wheel_id = %wheel_id, error = %e, "filter wheel read failed; the filter tie-break is neutral");
                None
            }
        }
    }

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
