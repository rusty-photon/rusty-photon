//! Power tool category (rp.md § Power Switching).
//!
//! `disconnect_device`, `connect_device`, `get_switch`, `set_switch` and
//! `restart_service`, and the night's steps over every device they
//! touch, `power_up`, `power_down` and `get_power_map`.
//!
//! The first two hold a device out of the reconnect supervisor and give
//! it back ([`crate::equipment::supervisor`] does the work); the next
//! two read and switch a power box's outputs through the power map on
//! its `switches[]` entry ([`crate::equipment::power`]); `restart_service`
//! has a held-out device's driver service restart. `power_up` and
//! `power_down` put those together for a set of outputs, and
//! `get_power_map` reads the whole map.

use std::collections::HashSet;

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
use crate::equipment::{power, EntryRef, SwitchEntry};

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

/// One output a `power_up` or `power_down` names.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutputRefParam {
    /// The power box's `switches[]` id.
    pub switch_id: String,
    /// The output's name as the box's driver reports it; it must be in
    /// rp's power map.
    pub output: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PowerUpParams {
    /// The outputs to power up. Omitted, every output the power map
    /// marks daily; an empty list is no output.
    #[serde(default)]
    pub outputs: Option<Vec<OutputRefParam>>,
    /// How long the devices get between their outputs going on and their
    /// services restarting, as a humantime duration (default `30s`, at
    /// most `5m`).
    #[serde(default)]
    pub settle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PowerDownParams {
    /// The outputs to cut. Omitted, every output the power map marks
    /// daily; an empty list is no output.
    #[serde(default)]
    pub outputs: Option<Vec<OutputRefParam>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPowerMapParams {}

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
        let word = close_word(&close);
        let error = match close {
            Close::Failed(e) => Some(e),
            Close::Closed | Close::TimedOut | Close::NoSession => None,
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
                "daily": mapped.is_some_and(|m| m.daily),
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
        let restart = match self.restart_held_out(entry, &service, &on_service).await {
            Ok(restart) => restart,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let devices: Vec<&str> = on_service.iter().map(|e| e.id()).collect();
        Ok(tool_success!({
            "device_id": entry.id(),
            "service": service,
            "devices": devices,
            "restart": restart.as_str(),
        }))
    }

    #[tool(
        description = "Read every mapped output of every power box: switch_id, output, daily, protected, on (null for a box rp cannot read) and devices ({device_id, kind, connected, held_out})"
    )]
    pub(crate) async fn get_power_map(
        &self,
        Parameters(_params): Parameters<GetPowerMapParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut outputs = Vec::new();
        for entry in &self.equipment.switches {
            let read = match entry.device() {
                Some(device) => power::switch_names(&*device)
                    .await
                    .ok()
                    .map(|names| (device, names)),
                None => None,
            };
            for mapped in &entry.config.outputs {
                let on = match &read {
                    Some((device, names)) => match names.iter().position(|n| n == &mapped.name) {
                        Some(index) => device.get_switch(index).await.ok(),
                        None => None,
                    },
                    None => None,
                };
                outputs.push(json!({
                    "switch_id": entry.id,
                    "output": mapped.name,
                    "daily": mapped.daily,
                    "protected": mapped.protected.as_ref().map(ProtectedReason::as_str),
                    "on": on,
                    "devices": self.device_views(&mapped.devices),
                }));
            }
        }
        Ok(tool_success!({ "outputs": outputs }))
    }

    #[tool(
        description = "Bring a set of outputs' devices up in fresh service processes: switch each output on, wait settle (default 30s, at most 5m) once something went on, hold out and restart once each service with a device on the outputs, then connect every device on those services. Without outputs it takes every output the power map marks daily; an empty list is no output. A failed step is reported and the call goes on. Returns outputs ({switch_id, output, on, switched, error}), services ({service, devices, restart, error}) and devices ({device_id, kind, connected, error})"
    )]
    pub(crate) async fn power_up(
        &self,
        Parameters(params): Parameters<PowerUpParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let settle = match power::parse_settle(params.settle.as_deref()) {
            Ok(settle) => settle,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let targets = match self.power_targets(params.outputs.as_deref(), false) {
            Ok(targets) => targets,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let _steps = self.equipment.power_steps.lock().await;

        let mut outputs = Vec::with_capacity(targets.len());
        let mut switched_any = false;
        for target in &targets {
            let switched = self.switch_target(target, true).await;
            switched_any |= switched.switched;
            let mut view = switched.view();
            view.insert("switched".into(), json!(switched.switched));
            outputs.push(view);
        }
        if switched_any {
            debug!(settle = ?settle, "power_up: letting the devices settle");
            tokio::time::sleep(settle).await;
        }

        let mut services_seen: Vec<String> = Vec::new();
        for id in targets.iter().flat_map(|t| t.devices.iter()) {
            let Some(entry) = self.equipment.entry(id) else {
                continue;
            };
            match power::service_of(entry.alpaca_url()) {
                Ok(service) => {
                    if !services_seen.contains(&service) {
                        services_seen.push(service);
                    }
                }
                Err(e) => debug!(device = %id, error = %e, "power_up: no service for a device"),
            }
        }

        let mut services = Vec::with_capacity(services_seen.len());
        let mut device_ids: Vec<String> = Vec::new();
        for service in &services_seen {
            let on_service: Vec<EntryRef<'_>> = self
                .equipment
                .entries()
                .filter(|e| power::service_of(e.alpaca_url()).is_ok_and(|s| &s == service))
                .collect();
            for entry in &on_service {
                if !entry.is_held_out() {
                    disconnect_entry(*entry, &self.event_bus, DISCONNECT_DEADLINE).await;
                }
            }
            let ids: Vec<&str> = on_service.iter().map(|e| e.id()).collect();
            let mut view = serde_json::Map::new();
            view.insert("service".into(), json!(service));
            view.insert("devices".into(), json!(ids));
            let restart = match on_service.first() {
                Some(first) => self.restart_held_out(*first, service, &on_service).await,
                None => Err("no device rp knows is on the service".to_string()),
            };
            match restart {
                Ok(restart) => {
                    view.insert("restart".into(), json!(restart.as_str()));
                }
                Err(e) => {
                    view.insert("restart".into(), json!("failed"));
                    view.insert("error".into(), json!(e));
                }
            }
            services.push(view);
            device_ids.extend(ids.into_iter().map(str::to_string));
        }

        let mut devices = Vec::with_capacity(device_ids.len());
        for id in &device_ids {
            let Some(entry) = self.equipment.entry(id) else {
                continue;
            };
            let mut view = serde_json::Map::new();
            view.insert("device_id".into(), json!(entry.id()));
            view.insert("kind".into(), json!(entry.kind()));
            match connect_entry(&self.equipment, entry, &self.event_bus).await {
                Supervised::Healthy | Supervised::Reestablished { live: true } => {
                    view.insert("connected".into(), json!(true));
                }
                Supervised::HeldOut | Supervised::Reestablished { live: false } => {
                    view.insert("connected".into(), json!(false));
                    view.insert(
                        "error".into(),
                        json!("held out again by a disconnect_device while it connected"),
                    );
                }
                Supervised::Failed(e) => {
                    view.insert("connected".into(), json!(false));
                    view.insert("error".into(), json!(e));
                }
            }
            devices.push(view);
        }
        info!(
            outputs = targets.len(),
            services = services.len(),
            devices = devices.len(),
            "powered a set of outputs up"
        );
        Ok(tool_success!({
            "outputs": outputs,
            "services": services,
            "devices": devices,
        }))
    }

    #[tool(
        description = "Take a set of outputs' devices out of rp's hands and cut the outputs: hold out every device the power map puts on them (each close within 10 s), then switch each output that reads on off. Without outputs it takes every output the power map marks daily; an empty list is no output. Refuses the whole call for an output the map does not list or marks protected. Reads no cooler and no park state: which outputs may go is the caller's decision. Returns outputs ({switch_id, output, on, error}) and devices ({device_id, kind, close})"
    )]
    pub(crate) async fn power_down(
        &self,
        Parameters(params): Parameters<PowerDownParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let targets = match self.power_targets(params.outputs.as_deref(), true) {
            Ok(targets) => targets,
            Err(e) => return Ok(tool_error!("{}", e)),
        };
        let _steps = self.equipment.power_steps.lock().await;

        let mut held = HashSet::new();
        let mut devices = Vec::new();
        for id in targets.iter().flat_map(|t| t.devices.iter()) {
            if !held.insert(id.as_str()) {
                continue;
            }
            let Some(entry) = self.equipment.entry(id) else {
                continue;
            };
            let close = disconnect_entry(entry, &self.event_bus, DISCONNECT_DEADLINE).await;
            let mut view = serde_json::Map::new();
            view.insert("device_id".into(), json!(entry.id()));
            view.insert("kind".into(), json!(entry.kind()));
            view.insert("close".into(), json!(close_word(&close)));
            devices.push(view);
        }
        let mut outputs = Vec::with_capacity(targets.len());
        for target in &targets {
            outputs.push(self.switch_target(target, false).await.view());
        }
        info!(
            outputs = targets.len(),
            devices = devices.len(),
            "powered a set of outputs down"
        );
        Ok(tool_success!({
            "outputs": outputs,
            "devices": devices,
        }))
    }
}

/// What switching one output came to.
struct SwitchedOutput<'a> {
    target: &'a power::PowerTarget,
    /// The state read back, `None` when the output could not be read.
    on: Option<bool>,
    /// The call wrote the output.
    switched: bool,
    error: Option<String>,
}

impl SwitchedOutput<'_> {
    fn view(&self) -> serde_json::Map<String, Value> {
        let mut view = serde_json::Map::new();
        view.insert("switch_id".into(), json!(self.target.switch_id));
        view.insert("output".into(), json!(self.target.output));
        view.insert("on".into(), json!(self.on));
        if let Some(error) = &self.error {
            view.insert("error".into(), json!(error));
        }
        view
    }
}

impl McpHandler {
    /// The outputs a `power_up` or `power_down` names, resolved against
    /// the power map (see [`power::resolve_targets`]).
    fn power_targets(
        &self,
        requested: Option<&[OutputRefParam]>,
        cutting: bool,
    ) -> Result<Vec<power::PowerTarget>, String> {
        let switches: Vec<&crate::config::SwitchConfig> =
            self.equipment.switches.iter().map(|s| &s.config).collect();
        let pairs: Option<Vec<(String, String)>> = requested.map(|list| {
            list.iter()
                .map(|r| (r.switch_id.clone(), r.output.clone()))
                .collect()
        });
        power::resolve_targets(&switches, pairs.as_deref(), cutting)
    }

    /// Switch one output to `on`, leaving one that already reads so
    /// alone. A box rp cannot read, an output it reports no switch for
    /// or reports read-only, and a failed write are reported, not
    /// raised.
    async fn switch_target<'a>(
        &self,
        target: &'a power::PowerTarget,
        on: bool,
    ) -> SwitchedOutput<'a> {
        let mut outcome = SwitchedOutput {
            target,
            on: None,
            switched: false,
            error: None,
        };
        let Some(entry) = self.equipment.find_switch(&target.switch_id) else {
            outcome.error = Some(format!("switch not found: {}", target.switch_id));
            return outcome;
        };
        match switch_one(&self.equipment, entry, &target.output, on).await {
            Ok((read_back, switched)) => {
                outcome.on = Some(read_back);
                outcome.switched = switched;
            }
            Err((read, e)) => {
                outcome.on = read;
                outcome.error = Some(e);
            }
        }
        outcome
    }

    /// Have the service at `service` restart once every device rp has
    /// on it is held out, and wait for its next process. `entry` lends
    /// the credential the request goes with.
    async fn restart_held_out(
        &self,
        entry: EntryRef<'_>,
        service: &str,
        on_service: &[EntryRef<'_>],
    ) -> Result<power::Restarted, String> {
        let client = power::restart_client(entry, self.equipment.ca_cert_path.as_deref())
            .map_err(|e| format!("device {}: {e}", entry.id()))?;
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
                return Err(format!(
                    "the service at {service} serves devices rp has not disconnected: {}; \
                     disconnect each with disconnect_device before restarting it",
                    in_hands.join(", ")
                ));
            }
            power::request_restart(&client, service).await?
        };
        let restart = power::await_restart(
            &client,
            service,
            &old,
            power::RESTART_POLL,
            power::RESTART_DEADLINE,
        )
        .await;
        info!(%service, restart = restart.as_str(), "restarted a driver service");
        Ok(restart)
    }

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

/// How a `disconnect_device` close is reported.
const fn close_word(close: &Close) -> &'static str {
    match close {
        Close::Closed => "closed",
        Close::TimedOut => "timed_out",
        Close::Failed(_) => "failed",
        Close::NoSession => "no_session",
    }
}

/// Switch the output named `output` of `entry`'s box to `on`: the state
/// read back and whether this call wrote it. One that already reads
/// `on` is left alone. Switching off holds the registry's power lock
/// across the hold-out check and the write, as `set_switch` does, so no
/// device on the output is connected between the two.
///
/// # Errors
///
/// The state read before the failure, if any, and the reason.
async fn switch_one(
    equipment: &crate::equipment::EquipmentRegistry,
    entry: &SwitchEntry,
    output: &str,
    on: bool,
) -> Result<(bool, bool), (Option<bool>, String)> {
    let Some(device) = entry.device() else {
        return Err((None, format!("switch not connected: {}", entry.id)));
    };
    let names = power::switch_names(&*device)
        .await
        .map_err(|e| (None, format!("switch {}: {e}", entry.id)))?;
    let index = power::index_of(&entry.id, &names, output).map_err(|e| (None, e))?;
    let current = device
        .get_switch(index)
        .await
        .map_err(|e| (None, format!("could not read the state of '{output}': {e}")))?;
    if current == on {
        return Ok((current, false));
    }
    match device.can_write(index).await {
        Ok(true) => {}
        Ok(false) => {
            return Err((
                Some(current),
                format!("output '{output}' of switch '{}' is read-only", entry.id),
            ))
        }
        Err(e) => {
            return Err((
                Some(current),
                format!("could not read CanWrite of '{output}': {e}"),
            ))
        }
    }
    {
        let _power = equipment.power.lock().await;
        if !on {
            power::switchable(&entry.config, output, false, |id| {
                equipment.entry(id).is_some_and(EntryRef::is_held_out)
            })
            .map_err(|e| (Some(current), e))?;
        }
        device.set_switch(index, on).await.map_err(|e| {
            (
                Some(current),
                format!("could not switch '{output}' of switch '{}': {e}", entry.id),
            )
        })?;
    }
    let read_back = device.get_switch(index).await.map_err(|e| {
        (
            None,
            format!("switched '{output}' but could not read it back: {e}"),
        )
    })?;
    debug!(switch_id = %entry.id, output, requested = on, on = read_back, "switched an output");
    Ok((read_back, true))
}
