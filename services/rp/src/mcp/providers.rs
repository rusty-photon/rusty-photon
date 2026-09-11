//! Tool-provider aggregation (rp.md § Plugin-Provided Tools, § Tool
//! Provider Registration).
//!
//! A tool provider is a plugin running its own MCP server. At startup
//! rp dials every `type: "tool_provider"` registration through the
//! standard client (`rp-mcp-client`, ADR-017 — the same credential
//! policy every first-party client follows), discovers its tools with
//! `tools/list`, and merges them into the catalog as proxy routes: a
//! client of rp sees them beside `slew` and `capture` with no way to
//! tell the difference. The merge is checked once, at startup, and the
//! catalog then stays stable for the life of the process:
//!
//! - a tool name offered by a provider *and* a built-in, or by two
//!   providers, fails startup naming both sources — there is no
//!   precedence to guess at (tenet 2);
//! - a registration's `requires_tools` is checked against the merged
//!   catalog, so a missing dependency is a startup error rather than a
//!   3 a.m. surprise;
//! - a provider that cannot be reached within the connect budget fails
//!   startup, because without its `tools/list` there is no catalog to
//!   build.
//!
//! A proxied call ([`Provider::proxy`]) forwards the caller's arguments
//! and `_meta`, relays the provider's `notifications/progress` back to
//! the caller under the caller's own token, and returns the provider's
//! result verbatim. It is registered in the in-flight registry like any
//! built-in ([`super::inflight`]) with the class its registration gave
//! it ([`Provider::class_of`]): when its `Cancel` fires — the safety
//! transition, or the caller going away — rp sends
//! `notifications/cancelled` for the provider request and answers the
//! caller `cancelled: <reason>`.
//!
//! A provider that stops answering is marked unreachable on the first
//! failed call (`provider_changed`, `connected: false`, once per
//! transition); its tools stay in the catalog and answer a tool error
//! naming it until the reconnect supervisor's provider lane
//! ([`Providers::pass`], rp.md § Device Session Recovery) re-dials it on
//! the equipment cadence and emits `provider_changed` with
//! `connected: true`. There is no re-discovery on reconnect: a provider
//! whose tool set changed needs an rp restart, which the error message
//! and a startup-time warning both say.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRoute;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    ProgressNotificationParam, ProgressToken, RequestMetaObject, Tool,
};
use rmcp::service::Peer;
use rmcp::RoleServer;
use rp_mcp_client::{ClientAuthConfig, ProxyCallError, RpMcpClient};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::gate::ToolClass;
use super::inflight::Cancel;
use super::McpHandler;
use crate::config::ToolProviderRegistration;
use crate::equipment::FocuserEntry;
use crate::error::{Result, RpError};
use crate::events::{EventBus, EventEnvelope};

/// Startup connect budget per provider.
///
/// The same three-attempts-over-a-few-seconds shape the device connect
/// uses, for the same reason — on a cold boot systemd starts the fleet
/// in parallel, and rp racing a provider to readiness is an ordering
/// roll of the dice.
pub const CONNECT_ATTEMPTS: u32 = 3;
/// Pause between startup connect attempts.
pub const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// Bound on one dial (`server/discover`) or one health check
/// (`tools/list`), so a provider that accepts the connection and then
/// hangs cannot wedge startup or a supervisor pass.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// One registered tool provider: its discovered tools (fixed at
/// startup) and its current client session (`None` while unreachable).
pub struct Provider {
    name: String,
    url: String,
    auth: Option<ClientAuthConfig>,
    ca_cert: Option<PathBuf>,
    /// The tools discovered at startup, name-sorted. Never changes.
    tools: Vec<Tool>,
    /// The registration's `"gate": {"<tool>": "none"}` opt-outs.
    ungated: Vec<String>,
    /// The registration's `focus_tools`: tool name → the argument
    /// carrying the train it focuses. A call to one is bracketed with
    /// the focus event triple ([`FocusBracket`], rp.md § Tool Provider
    /// Registration).
    focus_tools: BTreeMap<String, String>,
    /// The live client, or `None` while the provider is unreachable. A
    /// call holding a clone keeps using it; a re-dial replaces it.
    client: RwLock<Option<Arc<RpMcpClient>>>,
    /// Where `provider_changed` is emitted.
    event_bus: Arc<EventBus>,
}

impl Provider {
    /// The registration's `name`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The registration's `mcp_server_url`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The tools discovered at startup, in name order.
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Whether the provider currently has a live session.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.client().is_some()
    }

    /// The class of one of this provider's tools per its registration:
    /// gated unless the registration's `gate` key opts it out. The
    /// operator's `safety.gate` override is applied on top by
    /// [`super::gate::ClassTable::with_catalog`].
    #[must_use]
    pub fn class_of(&self, tool: &str) -> ToolClass {
        if self.ungated.iter().any(|name| name == tool) {
            ToolClass::Ungated
        } else {
            ToolClass::Gated
        }
    }

    fn client(&self) -> Option<Arc<RpMcpClient>> {
        self.client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn install(&self, client: RpMcpClient) {
        *self
            .client
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(client));
    }

    /// Drop the session. Returns whether there was one — the caller
    /// emits `provider_changed` once per transition, not once per
    /// failed call.
    fn mark_unreachable(&self) -> bool {
        self.client
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some()
    }

    /// One dial: `server/discover` against the registration's URL with
    /// its credential, bounded by [`DIAL_TIMEOUT`].
    async fn dial(&self) -> std::result::Result<RpMcpClient, String> {
        match tokio::time::timeout(
            DIAL_TIMEOUT,
            RpMcpClient::connect(&self.url, self.auth.as_ref(), self.ca_cert.as_deref()),
        )
        .await
        {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err(format!("no answer within {DIAL_TIMEOUT:?}")),
        }
    }

    /// The startup dial: [`CONNECT_ATTEMPTS`] tries,
    /// [`CONNECT_RETRY_DELAY`] apart.
    async fn dial_with_retries(&self) -> std::result::Result<RpMcpClient, String> {
        let mut last = String::new();
        for attempt in 1..=CONNECT_ATTEMPTS {
            match self.dial().await {
                Ok(client) => return Ok(client),
                Err(e) => {
                    debug!(provider = %self.name, attempt, error = %e, "tool provider dial failed");
                    last = e;
                    if attempt < CONNECT_ATTEMPTS {
                        tokio::time::sleep(CONNECT_RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(last)
    }

    /// The text of the tool error a call answers while the provider is
    /// unreachable.
    fn unreachable_message(&self, detail: Option<&str>) -> String {
        let detail = detail.map_or_else(String::new, |d| format!(": {d}"));
        format!(
            "tool provider `{}` is unreachable{detail} (its tools stay in the catalog and \
             answer this error until it is back; a provider whose tool set changed needs an \
             rp restart)",
            self.name
        )
    }

    /// The tool error a call answers while the provider is unreachable.
    fn unreachable_error(&self, detail: Option<&str>) -> CallToolResponse {
        CallToolResult::error(vec![ContentBlock::text(self.unreachable_message(detail))]).into()
    }

    /// The proxy body of one of this provider's tools: forward the
    /// arguments and `_meta`, relay progress, return the result
    /// verbatim, and cancel the provider's request when the caller's
    /// `Cancel` fires (rp.md § Plugin-Provided Tools). A tool the
    /// registration's `focus_tools` names is bracketed with the focus
    /// event triple around the forwarded call.
    async fn proxy(
        self: Arc<Self>,
        context: ToolCallContext<'_, McpHandler>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        let tool = context.name.clone();
        let handler = context.service;
        let request = context.request_context;
        let Some(client) = self.client() else {
            debug!(provider = %self.name, %tool, "proxied call while the provider is unreachable");
            return Ok(self.unreachable_error(None));
        };

        let bracket = match self.focus_tools.get(tool.as_ref()) {
            Some(argument) => {
                FocusBracket::open(
                    handler,
                    &self.name,
                    &tool,
                    argument,
                    context.arguments.as_ref(),
                )
                .await
            }
            None => None,
        };

        let mut params = CallToolRequestParams::new(tool.clone());
        params.arguments = context.arguments;
        params.meta = forwarded_meta(&request.meta);

        // Progress goes back under the caller's token, in order, from a
        // task that ends when the forwarded call drops its sender.
        let relay = request.meta.get_progress_token().map(|token| {
            let (tx, rx) = mpsc::unbounded_channel();
            (
                tx,
                tokio::spawn(relay_progress(rx, request.peer.clone(), token)),
            )
        });
        let (progress_tx, relay_task) = match relay {
            Some((tx, task)) => (Some(tx), Some(task)),
            None => (None, None),
        };

        let cancel = Cancel::from_context(&request);
        let cancel_reason = {
            let cancel = cancel.clone();
            async move {
                cancel.cancelled().await;
                cancel.reason().to_string()
            }
        };

        debug!(provider = %self.name, %tool, "forwarding tool call");
        let outcome = client
            .call_tool_forwarding(params, progress_tx, cancel_reason)
            .await;
        if let Some(task) = relay_task {
            // Flush the relayed progress before the result goes out.
            let _ = task.await;
        }

        let (response, end) = match outcome {
            Ok(result) => {
                let end = BracketEnd::from_result(&result);
                (Ok(result.into()), end)
            }
            Err(ProxyCallError::Cancelled) => {
                debug!(provider = %self.name, %tool, reason = %cancel.reason(), "forwarded call cancelled");
                let error = cancel.error();
                (
                    Ok(CallToolResult::error(vec![ContentBlock::text(error.clone())]).into()),
                    BracketEnd::Failed(error),
                )
            }
            Err(ProxyCallError::Protocol(data)) => {
                let end = BracketEnd::Failed(data.message.to_string());
                (Err(data), end)
            }
            Err(ProxyCallError::Request(message)) => {
                if self.mark_unreachable() {
                    warn!(provider = %self.name, %tool, error = %message, "tool provider unreachable; its tools answer an error until it is back");
                    self.emit_changed(false);
                }
                let end = BracketEnd::Failed(self.unreachable_message(Some(&message)));
                (Ok(self.unreachable_error(Some(&message))), end)
            }
        };
        if let Some(bracket) = bracket {
            bracket.close(end);
        }
        response
    }

    fn emit_changed(&self, connected: bool) {
        self.event_bus.emit(
            "provider_changed",
            serde_json::json!({ "provider": self.name, "connected": connected }),
        );
    }
}

/// The `_meta` forwarded to the provider: every key the caller sent
/// except the protocol-reserved `io.modelcontextprotocol/*` entries
/// (the client transport sets its own) and `progressToken` (rmcp mints
/// one per request; the caller's is what the relay answers under).
/// `None` when nothing is left.
/// The result fields `focus_complete` always carries (rp.md § Events):
/// a provider result lacking one reports it `null`.
const FOCUS_RESULT_FIELDS: [&str; 7] = [
    "position",
    "hfr",
    "best_position",
    "best_hfr",
    "confirmed",
    "fit_r_squared",
    "samples_used",
];

/// The result fields `focus_complete` carries only when the provider
/// result does.
const FOCUS_OPTIONAL_FIELDS: [&str; 2] = ["attempts", "steps"];

/// The focus event triple around a forwarded `focus_tools` call
/// (rp.md § Tool Provider Registration): opened before the call with
/// the train's resolved devices and the focuser's readings, closed
/// with the result or the error.
struct FocusBracket {
    camera_id: String,
    focuser_id: String,
    operation_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    event_bus: Arc<EventBus>,
}

/// How a bracketed call ended.
enum BracketEnd {
    /// The provider's success result, as the JSON the bracket reads
    /// the focus fields from.
    Complete(serde_json::Value),
    /// The tool error, cancellation reason or unreachable-provider
    /// error the caller received.
    Failed(String),
}

impl BracketEnd {
    fn from_result(result: &CallToolResult) -> Self {
        if result.is_error == Some(true) {
            Self::Failed(result_text(result))
        } else {
            Self::Complete(result_value(result))
        }
    }
}

impl FocusBracket {
    /// Resolve the train the call names and emit `focus_started`.
    /// `None` — the call is forwarded without a bracket — when the
    /// argument is missing or names no train with a focuser and a
    /// camera; the provider answers with its own error. A focuser
    /// reading that fails is `null`, not a refusal.
    async fn open(
        handler: &McpHandler,
        provider: &str,
        tool: &str,
        argument: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Option<Self> {
        let Some(train_id) = arguments
            .and_then(|args| args.get(argument))
            .and_then(serde_json::Value::as_str)
        else {
            debug!(provider, tool, argument, "focus tool called without its train argument; forwarding without the focus bracket");
            return None;
        };
        let Some(train) = handler.trains.train(train_id) else {
            debug!(
                provider,
                tool,
                train_id,
                "focus tool names an unknown train; forwarding without the focus bracket"
            );
            return None;
        };
        let (Some(camera_id), Some(focuser_id)) = (train.camera_id(), train.terminal_focuser())
        else {
            debug!(
                provider,
                tool,
                train_id,
                "focus tool names a train without a focuser; forwarding without the focus bracket"
            );
            return None;
        };
        let device = handler
            .equipment
            .find_focuser(focuser_id)
            .and_then(FocuserEntry::device);
        let (position, temperature) = match device {
            Some(foc) => (foc.position().await.ok(), foc.temperature().await.ok()),
            None => (None, None),
        };
        let operation_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now();
        handler.event_bus.emit_operation(EventEnvelope::started(
            "focus",
            &operation_id,
            started_at,
            serde_json::json!({
                "camera_id": camera_id,
                "focuser_id": focuser_id,
                "position": position,
                "temperature": temperature,
            }),
        ));
        debug!(
            provider,
            tool, train_id, camera_id, focuser_id, "focus bracket opened around the forwarded call"
        );
        Some(Self {
            camera_id: camera_id.to_string(),
            focuser_id: focuser_id.to_string(),
            operation_id,
            started_at,
            event_bus: Arc::clone(&handler.event_bus),
        })
    }

    /// Emit `focus_complete` from the result, or `focus_failed` with
    /// the error.
    fn close(self, end: BracketEnd) {
        match end {
            BracketEnd::Complete(value) => self.event_bus.emit_operation(EventEnvelope::complete(
                "focus",
                &self.operation_id,
                self.started_at,
                focus_complete_payload(&self.camera_id, &self.focuser_id, &value),
            )),
            BracketEnd::Failed(error) => self.event_bus.emit_operation(EventEnvelope::failed(
                "focus",
                &self.operation_id,
                self.started_at,
                &error,
            )),
        }
    }
}

/// The `focus_complete` payload from a provider result: the seven
/// focus fields by name (`null` when absent), `attempts` and `steps`
/// only when present.
fn focus_complete_payload(
    camera_id: &str,
    focuser_id: &str,
    result: &serde_json::Value,
) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    payload.insert("camera_id".to_owned(), serde_json::json!(camera_id));
    payload.insert("focuser_id".to_owned(), serde_json::json!(focuser_id));
    for field in FOCUS_RESULT_FIELDS {
        payload.insert(
            field.to_owned(),
            result
                .get(field)
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
    }
    for field in FOCUS_OPTIONAL_FIELDS {
        if let Some(value) = result.get(field) {
            payload.insert(field.to_owned(), value.clone());
        }
    }
    serde_json::Value::Object(payload)
}

/// A success result as JSON: its `structuredContent`, else its first
/// text block parsed as JSON, else `null`.
fn result_value(result: &CallToolResult) -> serde_json::Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    result
        .content
        .iter()
        .find_map(|block| block.as_text())
        .and_then(|text| serde_json::from_str(&text.text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// An error result's text blocks joined, the message the caller sees.
fn result_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| text.text.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The `focus_tools` rule: every key must be a tool the provider
/// offers.
fn unknown_focus_tools<'a>(
    focus_tools: &'a BTreeMap<String, String>,
    tools: &[Tool],
) -> Vec<&'a String> {
    focus_tools
        .keys()
        .filter(|name| !tools.iter().any(|tool| tool.name == name.as_str()))
        .collect()
}

fn forwarded_meta(meta: &RequestMetaObject) -> Option<RequestMetaObject> {
    let forwarded: serde_json::Map<String, serde_json::Value> = meta
        .iter()
        .filter(|(key, _)| {
            !key.starts_with("io.modelcontextprotocol/") && key.as_str() != "progressToken"
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (!forwarded.is_empty()).then_some(RequestMetaObject(rmcp::model::MetaObject(forwarded)))
}

/// Re-emit the provider's progress to the caller under the caller's
/// token, in order, until the sender drops.
async fn relay_progress(
    mut rx: mpsc::UnboundedReceiver<ProgressNotificationParam>,
    peer: Peer<RoleServer>,
    token: ProgressToken,
) {
    while let Some(mut notification) = rx.recv().await {
        notification.progress_token = token.clone();
        if let Err(e) = peer.notify_progress(notification).await {
            debug!(error = %e, "relayed notify_progress failed; caller likely disconnected");
        }
    }
}

/// Every registered provider, dialed and discovered at startup.
pub struct Providers {
    providers: Vec<Arc<Provider>>,
}

impl Providers {
    /// Dial every registration, discover its tools, and check the merge
    /// against `built_in` (the catalog's own tool names) — collisions,
    /// then `requires_tools` (rp.md § Tool Provider Registration).
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Config`] naming the provider when it cannot be
    /// reached within the connect budget or its `tools/list` fails, when
    /// a tool name is offered twice (both sources named), or when a
    /// registration's `requires_tools` names a tool the merged catalog
    /// does not have.
    pub async fn connect(
        registrations: &[ToolProviderRegistration],
        ca_cert: Option<&Path>,
        event_bus: Arc<EventBus>,
        built_in: &[String],
    ) -> Result<Self> {
        let mut providers = Vec::with_capacity(registrations.len());
        for registration in registrations {
            let provider = Provider {
                name: registration.name.clone(),
                url: registration.mcp_server_url.clone(),
                auth: registration.auth.clone(),
                ca_cert: ca_cert.map(Path::to_path_buf),
                tools: Vec::new(),
                ungated: registration.ungated_tools.clone(),
                focus_tools: registration.focus_tools.clone(),
                client: RwLock::new(None),
                event_bus: Arc::clone(&event_bus),
            };
            let client = provider.dial_with_retries().await.map_err(|e| {
                RpError::Config(format!(
                    "tool provider `{}` at {} is unreachable: {e} (rp needs its tools/list to \
                     build the catalog)",
                    provider.name, provider.url
                ))
            })?;
            let mut tools = tokio::time::timeout(DIAL_TIMEOUT, client.list_tool_records())
                .await
                .map_err(|_| format!("no answer within {DIAL_TIMEOUT:?}"))
                .and_then(|listed| listed.map_err(|e| e.to_string()))
                .map_err(|e| {
                    RpError::Config(format!(
                        "tool provider `{}` at {}: tools/list failed: {e}",
                        provider.name, provider.url
                    ))
                })?;
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            let unknown_opt_outs: Vec<&String> = provider
                .ungated
                .iter()
                .filter(|name| !tools.iter().any(|tool| tool.name == name.as_str()))
                .collect();
            if !unknown_opt_outs.is_empty() {
                return Err(RpError::Config(format!(
                    "tool provider `{}`: its `gate` key names tools it does not offer: {unknown_opt_outs:?}",
                    provider.name
                )));
            }
            let unknown_focus = unknown_focus_tools(&provider.focus_tools, &tools);
            if !unknown_focus.is_empty() {
                return Err(RpError::Config(format!(
                    "tool provider `{}`: its `focus_tools` key names tools it does not offer: {unknown_focus:?}",
                    provider.name
                )));
            }
            info!(
                provider = %provider.name,
                url = %provider.url,
                tools = ?tools.iter().map(|tool| tool.name.as_ref()).collect::<Vec<_>>(),
                "tool provider connected; its tools join the catalog"
            );
            provider.install(client);
            providers.push(Arc::new(Provider { tools, ..provider }));
        }

        let offered: Vec<(String, Vec<String>)> = providers
            .iter()
            .map(|provider| {
                (
                    provider.name.clone(),
                    provider
                        .tools
                        .iter()
                        .map(|tool| tool.name.to_string())
                        .collect(),
                )
            })
            .collect();
        let collisions = merge_errors(built_in, &offered);
        if !collisions.is_empty() {
            return Err(RpError::Config(collisions.join("; ")));
        }
        let catalog: BTreeSet<String> = built_in
            .iter()
            .cloned()
            .chain(offered.iter().flat_map(|(_, tools)| tools.iter().cloned()))
            .collect();
        let missing = missing_requirements(&catalog, registrations);
        if !missing.is_empty() {
            return Err(RpError::Config(missing.join("; ")));
        }
        Ok(Self { providers })
    }

    /// No registrations at all — the ordinary deployment.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// The registered providers, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Provider>> {
        self.providers.iter()
    }

    /// Every provider tool with its registration class, for
    /// [`super::gate::ClassTable::with_catalog`].
    #[must_use]
    pub fn tool_classes(&self) -> Vec<(String, ToolClass)> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider.tools.iter().map(move |tool| {
                    let name = tool.name.to_string();
                    let class = provider.class_of(&name);
                    (name, class)
                })
            })
            .collect()
    }

    /// One proxy route per provider tool, advertising the provider's
    /// own `Tool` record (name, description, schemas) unchanged.
    #[must_use]
    pub fn routes(&self) -> Vec<ToolRoute<McpHandler>> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider.tools.iter().map(move |tool| {
                    let provider = Arc::clone(provider);
                    ToolRoute::new_dyn(tool.clone(), move |context| {
                        let provider = Arc::clone(&provider);
                        Box::pin(provider.proxy(context))
                    })
                })
            })
            .collect()
    }

    /// The reconnect supervisor's provider lane (rp.md § Device Session
    /// Recovery): health-check every live session with `tools/list`,
    /// re-dial every dead one, and emit `provider_changed` on each
    /// transition. A live provider whose tool set no longer matches the
    /// startup catalog is logged — the catalog is not rebuilt.
    pub async fn pass(&self) {
        for provider in &self.providers {
            if let Some(client) = provider.client() {
                match tokio::time::timeout(DIAL_TIMEOUT, client.list_tool_records()).await {
                    Ok(Ok(tools)) => {
                        warn_on_drift(provider, &tools);
                        continue;
                    }
                    Ok(Err(e)) => {
                        debug!(provider = %provider.name, error = %e, "tool provider health check failed; re-dialing");
                    }
                    Err(_) => {
                        debug!(provider = %provider.name, "tool provider health check timed out; re-dialing");
                    }
                }
                if provider.mark_unreachable() {
                    warn!(provider = %provider.name, "tool provider unreachable; its tools answer an error until it is back");
                    provider.emit_changed(false);
                }
            }
            match provider.dial().await {
                Ok(client) => {
                    provider.install(client);
                    info!(provider = %provider.name, "tool provider session re-established");
                    provider.emit_changed(true);
                }
                Err(e) => {
                    debug!(provider = %provider.name, error = %e, "tool provider still unreachable");
                }
            }
        }
    }
}

/// Log when a live provider's `tools/list` no longer matches what it
/// offered at startup: the catalog is built once, so the operator has
/// to restart rp to pick the change up.
fn warn_on_drift(provider: &Provider, tools: &[Tool]) {
    let now: BTreeSet<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let then: BTreeSet<&str> = provider
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect();
    if now != then {
        let added: Vec<&&str> = now.difference(&then).collect();
        let removed: Vec<&&str> = then.difference(&now).collect();
        warn!(
            provider = %provider.name,
            ?added,
            ?removed,
            "tool provider offers a different tool set than at startup; the catalog is built \
             once — restart rp to pick it up"
        );
    }
}

/// The merge rule (tenet 2).
///
/// A tool name offered by a provider and a built-in, or by two
/// providers, is an error naming both sources; a provider listing one
/// name twice in its own `tools/list` is its own error, so the
/// collision message never names one provider twice. `offered` is each
/// provider's name with its tool names. Empty means the merge is clean.
#[must_use]
pub fn merge_errors(built_in: &[String], offered: &[(String, Vec<String>)]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut claimed: BTreeMap<&str, &str> = BTreeMap::new();
    for (provider, tools) in offered {
        let mut own: BTreeSet<&str> = BTreeSet::new();
        for tool in tools {
            if !own.insert(tool) {
                errors.push(format!(
                    "tool provider `{provider}` lists `{tool}` more than once in its tools/list"
                ));
            } else if built_in.iter().any(|name| name == tool) {
                errors.push(format!(
                    "tool `{tool}` is offered by tool provider `{provider}` and built into rp; \
                     a provider cannot shadow a built-in — rename the provider's tool"
                ));
            } else if let Some(other) = claimed.get(tool.as_str()) {
                errors.push(format!(
                    "tool `{tool}` is offered by tool providers `{other}` and `{provider}`; \
                     there is no precedence between providers — rename one"
                ));
            } else {
                claimed.insert(tool, provider);
            }
        }
    }
    errors
}

/// The `requires_tools` rule: every tool a registration requires must
/// be in the merged `catalog`. Empty means satisfied.
#[must_use]
pub fn missing_requirements(
    catalog: &BTreeSet<String>,
    registrations: &[ToolProviderRegistration],
) -> Vec<String> {
    registrations
        .iter()
        .filter_map(|registration| {
            let missing: Vec<&str> = registration
                .requires_tools
                .iter()
                .filter(|tool| !catalog.contains(*tool))
                .map(String::as_str)
                .collect();
            (!missing.is_empty()).then(|| {
                format!(
                    "tool provider `{}` requires tools the catalog does not have: {missing:?}",
                    registration.name
                )
            })
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn registration(name: &str, requires: &[&str], ungated: &[&str]) -> ToolProviderRegistration {
        ToolProviderRegistration {
            name: name.to_owned(),
            mcp_server_url: "http://127.0.0.1:1/mcp".to_owned(),
            auth: None,
            ungated_tools: names(ungated),
            requires_tools: names(requires),
            focus_tools: BTreeMap::new(),
        }
    }

    fn provider(name: &str, tools: &[&str], ungated: &[&str]) -> Arc<Provider> {
        Arc::new(Provider {
            name: name.to_owned(),
            url: "http://127.0.0.1:1/mcp".to_owned(),
            auth: None,
            ca_cert: None,
            tools: tools
                .iter()
                .map(|tool| Tool::new((*tool).to_owned(), "", Arc::new(serde_json::Map::new())))
                .collect(),
            ungated: names(ungated),
            focus_tools: BTreeMap::new(),
            client: RwLock::new(None),
            event_bus: Arc::new(EventBus::from_config(&[], None).unwrap()),
        })
    }

    fn tool_records(tools: &[&str]) -> Vec<Tool> {
        tools
            .iter()
            .map(|tool| Tool::new((*tool).to_owned(), "", Arc::new(serde_json::Map::new())))
            .collect()
    }

    #[test]
    fn a_focus_tools_key_must_name_an_offered_tool() {
        let focus_tools: BTreeMap<String, String> = [
            ("focus_train".to_owned(), "train_id".to_owned()),
            ("refocus".to_owned(), "train".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            unknown_focus_tools(&focus_tools, &tool_records(&["focus_train", "echo"])),
            vec!["refocus"]
        );
        let none_unknown =
            unknown_focus_tools(&focus_tools, &tool_records(&["focus_train", "refocus"]));
        assert!(none_unknown.is_empty(), "{none_unknown:?}");
    }

    /// The seven focus fields are always present — `null` when the
    /// result lacks them — and `attempts` / `steps` only when carried.
    #[test]
    fn focus_complete_payload_reads_the_named_fields_and_nulls_the_rest() {
        let payload = focus_complete_payload(
            "cam",
            "foc",
            &serde_json::json!({
                "position": 5120, "hfr": 2.4, "confirmed": true,
                "steps": [{"focuser_id": "foc"}], "extra": "ignored"
            }),
        );
        assert_eq!(
            payload,
            serde_json::json!({
                "camera_id": "cam", "focuser_id": "foc",
                "position": 5120, "hfr": 2.4, "best_position": null, "best_hfr": null,
                "confirmed": true, "fit_r_squared": null, "samples_used": null,
                "steps": [{"focuser_id": "foc"}]
            })
        );
        let bare = focus_complete_payload("cam", "foc", &serde_json::Value::Null);
        assert!(bare["position"].is_null());
        assert!(bare.get("attempts").is_none());
        assert!(bare.get("steps").is_none());
    }

    /// A success result is read from `structuredContent` first, then
    /// its text block; an error result ends the bracket with its text.
    #[test]
    fn a_bracket_end_follows_the_result_shape() {
        let text = CallToolResult::success(vec![ContentBlock::text(r#"{"position": 7}"#)]);
        assert_eq!(result_value(&text)["position"], 7);
        let mut structured = CallToolResult::success(vec![ContentBlock::text("ignored")]);
        structured.structured_content = Some(serde_json::json!({"position": 9}));
        assert_eq!(result_value(&structured)["position"], 9);
        let opaque = CallToolResult::success(vec![ContentBlock::text("not json")]);
        assert!(result_value(&opaque).is_null());

        match BracketEnd::from_result(&CallToolResult::error(vec![
            ContentBlock::text("no stars"),
            ContentBlock::text("curve_points: []"),
        ])) {
            BracketEnd::Failed(error) => assert_eq!(error, "no stars; curve_points: []"),
            BracketEnd::Complete(_) => panic!("an error result must fail the bracket"),
        }
        match BracketEnd::from_result(&text) {
            BracketEnd::Complete(value) => assert_eq!(value["position"], 7),
            BracketEnd::Failed(error) => {
                panic!("a success result must complete the bracket: {error}")
            }
        }
    }

    #[test]
    fn a_clean_merge_has_no_errors() {
        let errors = merge_errors(
            &names(&["capture", "slew"]),
            &[
                ("a".to_owned(), names(&["echo"])),
                ("b".to_owned(), names(&["classify"])),
            ],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// Tenet 2: a collision names both sources, whichever they are.
    #[test]
    fn a_collision_with_a_built_in_or_another_provider_names_both() {
        let errors = merge_errors(
            &names(&["capture"]),
            &[
                ("a".to_owned(), names(&["capture", "echo"])),
                ("b".to_owned(), names(&["echo"])),
            ],
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].contains("`capture`")
                && errors[0].contains("`a`")
                && errors[0].contains("built into rp"),
            "{}",
            errors[0]
        );
        assert!(
            errors[1].contains("`echo`") && errors[1].contains("`a`") && errors[1].contains("`b`"),
            "{}",
            errors[1]
        );
    }

    /// A provider repeating a name is not a collision with itself: it
    /// gets its own error, and the name is claimed once.
    #[test]
    fn a_provider_listing_a_tool_twice_is_its_own_error() {
        let errors = merge_errors(
            &names(&["capture"]),
            &[
                ("a".to_owned(), names(&["echo", "echo"])),
                ("b".to_owned(), names(&["echo"])),
            ],
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(
            errors[0].contains("`a`") && errors[0].contains("more than once"),
            "{}",
            errors[0]
        );
        assert!(
            errors[1].contains("`a`")
                && errors[1].contains("`b`")
                && !errors[1].contains("`a` and `a`"),
            "{}",
            errors[1]
        );
    }

    #[test]
    fn requires_tools_is_checked_against_the_merged_catalog() {
        let catalog: BTreeSet<String> = names(&["capture", "echo"]).into_iter().collect();
        let none_missing =
            missing_requirements(&catalog, &[registration("a", &["capture", "echo"], &[])]);
        assert!(none_missing.is_empty(), "{none_missing:?}");
        let missing = missing_requirements(
            &catalog,
            &[
                registration("a", &["capture"], &[]),
                registration("b", &["plate_solve", "echo", "measure_wavefront"], &[]),
            ],
        );
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(
            missing[0].contains("`b`")
                && missing[0].contains("plate_solve")
                && missing[0].contains("measure_wavefront")
                && !missing[0].contains("\"echo\""),
            "{}",
            missing[0]
        );
    }

    /// Gated by default; the registration's `gate` key opts out.
    #[test]
    fn provider_tools_are_gated_unless_opted_out() {
        let provider = provider("a", &["echo", "slow_echo"], &["echo"]);
        assert_eq!(provider.class_of("echo"), ToolClass::Ungated);
        assert_eq!(provider.class_of("slow_echo"), ToolClass::Gated);
        let providers = Providers {
            providers: vec![provider],
        };
        assert_eq!(
            providers.tool_classes(),
            vec![
                ("echo".to_owned(), ToolClass::Ungated),
                ("slow_echo".to_owned(), ToolClass::Gated)
            ]
        );
    }

    /// The routes advertise the provider's own tool records.
    #[test]
    fn routes_carry_the_providers_tool_records() {
        let providers = Providers {
            providers: vec![
                provider("a", &["echo"], &[]),
                provider("b", &["classify"], &[]),
            ],
        };
        let routes = providers.routes();
        let route_names: Vec<&str> = routes.iter().map(ToolRoute::name).collect();
        assert_eq!(route_names, ["echo", "classify"]);
        assert!(!providers.is_empty());
        assert!(Providers::none().is_empty());
    }

    /// The caller's protocol fields and progress token are not
    /// forwarded; everything else is.
    #[test]
    fn forwarded_meta_drops_protocol_keys_and_the_progress_token() {
        let mut meta = RequestMetaObject::new();
        meta.set_progress_token(ProgressToken(rmcp::model::NumberOrString::Number(7)));
        meta.set_protocol_version(rmcp::model::ProtocolVersion::V_2026_07_28);
        meta.insert("traceparent".to_owned(), serde_json::json!("00-abc-def-01"));
        let forwarded = forwarded_meta(&meta).expect("the trace key survives");
        assert_eq!(
            forwarded.get("traceparent"),
            Some(&serde_json::json!("00-abc-def-01"))
        );
        assert!(forwarded.get_progress_token().is_none());
        assert!(forwarded.protocol_version().is_none());

        let mut only_reserved = RequestMetaObject::new();
        only_reserved.set_protocol_version(rmcp::model::ProtocolVersion::V_2026_07_28);
        assert!(forwarded_meta(&only_reserved).is_none());
    }

    /// The unreachable answer is a tool error naming the provider and
    /// the restart caveat, with the detail when there is one.
    #[test]
    fn the_unreachable_error_names_the_provider() {
        let provider = provider("ml-quality-classifier", &["classify"], &[]);
        let CallToolResponse::Complete(result) =
            provider.unreachable_error(Some("connection refused"))
        else {
            panic!("expected a complete result");
        };
        assert_eq!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(
            text.contains("`ml-quality-classifier`")
                && text.contains("connection refused")
                && text.contains("rp restart"),
            "{text}"
        );
        assert!(!provider.is_connected());
        assert!(
            !provider.mark_unreachable(),
            "nothing to drop while disconnected"
        );
    }

    /// An unreachable provider at startup fails the build with the
    /// provider named — loopback port 1 refuses immediately, so the
    /// three attempts cost only the retry delays.
    #[tokio::test(start_paused = true)]
    async fn an_unreachable_provider_fails_startup_naming_it() {
        let bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let err = Providers::connect(
            &[registration("a", &[], &[])],
            None,
            bus,
            &names(&["capture"]),
        )
        .await
        .err()
        .expect("startup must fail");
        let message = err.to_string();
        assert!(
            message.contains("`a`")
                && message.contains("unreachable")
                && message.contains("tools/list"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn no_registrations_connect_to_nothing() {
        let bus = Arc::new(EventBus::from_config(&[], None).unwrap());
        let providers = Providers::connect(&[], None, bus, &names(&["capture"]))
            .await
            .unwrap();
        assert!(providers.is_empty());
        let classes = providers.tool_classes();
        assert!(classes.is_empty(), "{classes:?}");
        providers.pass().await;
    }
}
