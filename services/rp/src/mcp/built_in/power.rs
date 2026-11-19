//! Power tool category: `disconnect_device`, `connect_device`,
//! `get_switch`, `set_switch`, `restart_service` (rp.md § Power
//! Switching).
//!
//! The first two hold a device out of the reconnect supervisor and give
//! it back ([`crate::equipment::supervisor`] does the work); the next
//! two read and switch a power box's outputs through the power map on
//! its `switches[]` entry ([`crate::equipment::power`]); the last has a
//! held-out device's driver service restart.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info};

use super::super::handler::McpHandler;
use super::super::{tool_error, tool_success};
use crate::config::ProtectedReason;
use crate::equipment::supervisor::{
    connect_entry, disconnect_entry, Close, Supervised, DISCONNECT_DEADLINE,
};
use crate::equipment::{power, EntryRef};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceIdParams {
    /// The device's roster id, or `mount` for the mount.
    pub device_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSwitchParams {
    /// The power box's `switches[]` id.
    pub switch_id: String,
    /// The output's name as the box's driver reports it; omitted, every
    /// switch the box reports is read.
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetSwitchParams {
    /// The power box's `switches[]` id.
    pub switch_id: String,
    /// The output's name as the box's driver reports it; it must be in
    /// rp's power map.
    pub output: String,
    /// `true` to switch the output on, `false` to switch it off.
    pub on: bool,
}

#[tool_router(router = tool_router_power, vis = "pub")]
impl McpHandler {
    #[tool(
        description = "Take a device out of rp's hands before its power is cut: hold it out of the reconnect supervisor, then send its driver Connected=false within a 10 s deadline. The hold-out stands however the close ends. Returns device_id, kind and close (closed | timed_out | failed, with error | no_session)"
    )]
    pub(crate) async fn disconnect_device(
        &self,
        Parameters(params): Parameters<DeviceIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.entry(&params.device_id) else {
            return Ok(tool_error!("device not found: {}", params.device_id));
        };
        let close = disconnect_entry(entry, &self.event_bus, DISCONNECT_DEADLINE).await;
        let (word, error) = match close {
            Close::Closed => ("closed", None),
            Close::TimedOut => ("timed_out", None),
            Close::Failed(e) => ("failed", Some(e)),
            Close::NoSession => ("no_session", None),
        };
        let mut result = serde_json::Map::new();
        result.insert("device_id".into(), json!(entry.id()));
        result.insert("kind".into(), json!(entry.kind()));
        result.insert("close".into(), json!(word));
        if let Some(error) = error {
            result.insert("error".into(), json!(error));
        }
        Ok(tool_success!(result))
    }

    #[tool(
        description = "Give a device disconnect_device held out back to rp and connect it now with the full connect budget. A live session that reads healthy is left as it is, never cycled. Returns device_id, kind and connected"
    )]
    pub(crate) async fn connect_device(
        &self,
        Parameters(params): Parameters<DeviceIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.entry(&params.device_id) else {
            return Ok(tool_error!("device not found: {}", params.device_id));
        };
        match connect_entry(&self.equipment, entry, &self.event_bus).await {
            Supervised::Healthy | Supervised::Reestablished { live: true } => {
                debug!(
                    kind = entry.kind(),
                    id = entry.id(),
                    "connect_device: connected"
                );
                Ok(tool_success!({
                    "device_id": entry.id(),
                    "kind": entry.kind(),
                    "connected": true,
                }))
            }
            Supervised::HeldOut | Supervised::Reestablished { live: false } => Ok(tool_error!(
                "device {} was held out again by a disconnect_device while it connected",
                entry.id()
            )),
            Supervised::Failed(e) => Ok(tool_error!(
                "could not connect {}: {e}; the reconnect supervisor keeps trying every pass",
                entry.id()
            )),
        }
    }

    #[tool(
        description = "Read a power box's outputs: the one named output, or every switch the box reports. Each comes back with name, index, can_write, on and value, beside the power map's mapped, protected and devices ({device_id, kind, connected, held_out}); unmatched lists map outputs the box reports no switch for"
    )]
    pub(crate) async fn get_switch(
        &self,
        Parameters(params): Parameters<GetSwitchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.find_switch(&params.switch_id) else {
            return Ok(tool_error!("switch not found: {}", params.switch_id));
        };
        let Some(device) = entry.device() else {
            return Ok(tool_error!("switch not connected: {}", params.switch_id));
        };
        let names = match power::switch_names(&*device).await {
            Ok(names) => names,
            Err(e) => return Ok(tool_error!("switch {}: {e}", params.switch_id)),
        };
        let selected: Vec<(usize, &String)> = match params.output.as_deref() {
            Some(output) => match power::index_of(&params.switch_id, &names, output) {
                Ok(index) => names.iter().enumerate().skip(index).take(1).collect(),
                Err(e) => return Ok(tool_error!("{}", e)),
            },
            None => names.iter().enumerate().collect(),
        };
        let mut outputs = Vec::with_capacity(selected.len());
        for (index, name) in selected {
            let reading = match power::read_output(&*device, index, name.clone()).await {
                Ok(reading) => reading,
                Err(e) => return Ok(tool_error!("switch {}: {e}", params.switch_id)),
            };
            let mapped = entry.config.output(&reading.name);
            outputs.push(json!({
                "name": reading.name,
                "index": reading.index,
                "can_write": reading.can_write,
                "on": reading.on,
                "value": reading.value,
                "mapped": mapped.is_some(),
                "protected": mapped
                    .and_then(|m| m.protected.as_ref())
                    .map(ProtectedReason::as_str),
                "devices": mapped.map_or_else(Vec::new, |m| self.device_views(&m.devices)),
            }));
        }
        let unmatched: Vec<&str> = entry
            .config
            .outputs
            .iter()
            .map(|o| o.name.as_str())
            .filter(|name| !names.iter().any(|n| n == name))
            .collect();
        Ok(tool_success!({
            "switch_id": params.switch_id,
            "outputs": outputs,
            "unmatched": unmatched,
        }))
    }

    #[tool(
        description = "Switch a power box's output on or off. The output must be in rp's power map; switching off is refused on a protected output and while a device the map puts on it is not held out by disconnect_device. Returns switch_id, output and on, read back after the write"
    )]
    pub(crate) async fn set_switch(
        &self,
        Parameters(params): Parameters<SetSwitchParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.find_switch(&params.switch_id) else {
            return Ok(tool_error!("switch not found: {}", params.switch_id));
        };
        let Some(device) = entry.device() else {
            return Ok(tool_error!("switch not connected: {}", params.switch_id));
        };
        // Held from the hold-out check through the write, so no
        // connect_device can give a device on the output back to rp in
        // between (rp.md § Power Switching).
        let _power = self.equipment.power.lock().await;
        if let Err(refusal) = power::switchable(&entry.config, &params.output, params.on, |id| {
            self.equipment.entry(id).is_some_and(EntryRef::is_held_out)
        }) {
            return Ok(tool_error!("{}", refusal));
        }
        let names = match power::switch_names(&*device).await {
            Ok(names) => names,
            Err(e) => return Ok(tool_error!("switch {}: {e}", params.switch_id)),
        };
        let index = match power::index_of(&params.switch_id, &names, &params.output) {
            Ok(index) => index,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        match device.can_write(index).await {
            Ok(true) => {}
            Ok(false) => {
                return Ok(tool_error!(
                    "output '{}' of switch '{}' is read-only",
                    params.output,
                    params.switch_id
                ))
            }
            Err(e) => {
                return Ok(tool_error!(
                    "could not read CanWrite of '{}': {e}",
                    params.output
                ))
            }
        }
        if let Err(e) = device.set_switch(index, params.on).await {
            return Ok(tool_error!(
                "could not switch '{}' of switch '{}': {e}",
                params.output,
                params.switch_id
            ));
        }
        let on = match device.get_switch(index).await {
            Ok(on) => on,
            Err(e) => {
                return Ok(tool_error!(
                    "switched '{}' but could not read it back: {e}",
                    params.output
                ))
            }
        };
        info!(switch_id = %params.switch_id, output = %params.output, requested = params.on, on, "switched a power-box output");
        Ok(tool_success!({
            "switch_id": params.switch_id,
            "output": params.output,
            "on": on,
        }))
    }

    #[tool(
        description = "Have the driver service that serves a device restart, so it meets the device in a fresh process. Every device rp has on the service (the device's alpaca_url scheme, host and port) must be held out by disconnect_device. Waits up to 60 s for the service's new process to answer. Returns device_id, service, devices and restart (restarted | timed_out)"
    )]
    pub(crate) async fn restart_service(
        &self,
        Parameters(params): Parameters<DeviceIdParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(entry) = self.equipment.entry(&params.device_id) else {
            return Ok(tool_error!("device not found: {}", params.device_id));
        };
        let service = match power::service_of(entry.alpaca_url()) {
            Ok(service) => service,
            Err(e) => return Ok(tool_error!("device {}: {e}", entry.id())),
        };
        let on_service: Vec<EntryRef<'_>> = self
            .equipment
            .entries()
            .filter(|e| power::service_of(e.alpaca_url()).is_ok_and(|s| s == service))
            .collect();
        let client = match power::restart_client(entry, self.equipment.ca_cert_path.as_deref()) {
            Ok(client) => client,
            Err(e) => return Ok(tool_error!("device {}: {e}", entry.id())),
        };
        let old = {
            // Held from the hold-out check through the request, so no
            // connect_device can give a device on the service back to rp
            // in between (rp.md § Power Switching).
            let _power = self.equipment.power.lock().await;
            let in_hands: Vec<&str> = on_service
                .iter()
                .filter(|e| !e.is_held_out())
                .map(|e| e.id())
                .collect();
            if !in_hands.is_empty() {
                return Ok(tool_error!(
                    "the service at {service} serves devices rp has not disconnected: {}; \
                     disconnect each with disconnect_device before restarting it",
                    in_hands.join(", ")
                ));
            }
            match power::request_restart(&client, &service).await {
                Ok(old) => old,
                Err(e) => return Ok(tool_error!("{}", e)),
            }
        };
        let restart = power::await_restart(
            &client,
            &service,
            &old,
            power::RESTART_POLL,
            power::RESTART_DEADLINE,
        )
        .await;
        info!(%service, restart = restart.as_str(), "restarted a driver service");
        let devices: Vec<&str> = on_service.iter().map(|e| e.id()).collect();
        Ok(tool_success!({
            "device_id": entry.id(),
            "service": service,
            "devices": devices,
            "restart": restart.as_str(),
        }))
    }
}

impl McpHandler {
    /// The devices an output feeds, each as `get_switch` reports it.
    fn device_views(&self, ids: &[String]) -> Vec<Value> {
        ids.iter()
            .map(|id| {
                self.equipment.entry(id).map_or_else(
                    || json!({ "device_id": id, "kind": null, "connected": false, "held_out": false }),
                    |entry| {
                        json!({
                            "device_id": id,
                            "kind": entry.kind(),
                            "connected": entry.is_connected(),
                            "held_out": entry.is_held_out(),
                        })
                    },
                )
            })
            .collect()
    }
}
