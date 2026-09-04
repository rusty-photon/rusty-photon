//! MCP server for `rp`.
//!
//! `rp` exposes its action surface as MCP tools over rmcp's
//! streamable-HTTP transport. The handler [`McpHandler`] owns shared
//! state (equipment registry, event bus, session config, image cache,
//! observer site, planner targets, plate-solver client, guider
//! client, target store) and exposes 64 tools across 15 categories:
//! camera, imaging, filter wheel, cover/calibrator, focuser, mount,
//! rotator, `auto_focus` (incl. `refocus_train`), `plate_solve`, guider,
//! `center_on_target`, planner, targets, `plan_schema`, safety — plus
//! every tool a registered tool provider offers, proxied through
//! [`providers`] (rp.md § Plugin-Provided Tools).
//!
//! ## Layout
//!
//! Each tool category lives in its own file under [`built_in`], holding
//! its parameter structs, its tool method bodies, and any
//! category-specific helpers. The category file declares its own
//! `#[tool_router(router = tool_router_<name>, vis = "pub")]` impl block
//! on `McpHandler`. [`McpHandler::new`] merges the per-category routers
//! via `+` (see [`handler::McpHandler::new`]). A single explicit
//! `#[tool_handler(router = self.tool_router)] impl ServerHandler` block
//! at the bottom of this file glues the merged router into rmcp's
//! transport.
//!
//! Cross-category helper methods on `McpHandler`
//! (`do_capture`, `do_move_focuser_blocking`, `*_via_document` /
//! `*_via_path` dispatch helpers, `persist_capture_artifact`,
//! `resolve_mount`, `read_mount_hints_for_plate_solve`,
//! `do_slew_blocking`, `do_park_blocking`) live in [`internals`]
//! together with their supporting private types (`ResolvedParams`,
//! `BackgroundOutcome`, `DetectStarsOutcome`,
//! `ResolvedMeasureStarsParams`, `PollIdleError`) and free helper
//! functions (`clip_outcome`, `detect_outcome`, `star_to_json`,
//! `poll_slewing_until_idle`).
//!
//! ## Adding a tool category
//!
//! 1. Add `<name>.rs` under [`built_in`] with a
//!    `#[tool_router(router = tool_router_<name>, vis = "pub")] impl
//!    McpHandler { ... #[tool] async fn ... }` block. Param structs and
//!    private helpers go in the same file.
//! 2. Add `pub mod <name>;` to `built_in/mod.rs` and a re-export of the
//!    category's param structs.
//! 3. Add `+ Self::tool_router_<name>()` to the merge chain in
//!    [`handler::McpHandler::new`]. No edits needed in any existing
//!    category file.
//!
//! ## Adding a tool to an existing category
//!
//! Edit only `built_in/<category>.rs`. Add the param struct(s), then
//! add a new `#[tool(description = "...")] async fn ...` inside the
//! existing `#[tool_router]` impl block.
//!
//! ## Macros
//!
//! Three private declarative macros simplify tool bodies:
//!
//! - `tool_success!({...})` — wraps a `serde_json::json!()` payload
//!   into a `CallToolResult::success` text content.
//! - `tool_error!("...", arg)` — returns a `CallToolResult::error`
//!   carrying the formatted message.
//! - `resolve_device!(self, find_X, &id, "kind")` — looks up a device
//!   by id in the equipment registry, early-returning the standard
//!   "kind not found" / "kind not connected" error when missing or
//!   disconnected.
//!
//! They live in this file and are re-exported via `pub(crate) use` so
//! sibling submodules can `use super::{tool_success, tool_error,
//! resolve_device};`.

pub mod built_in;
pub mod gate;
pub mod handler;
pub mod inflight;
pub mod internals;
pub mod progress;
pub mod providers;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
// Slicing a literal UUID down to its 8-char disk key. There is no
// `allow-string-slice-in-tests` knob, so the exemption is scoped here.
#[allow(clippy::string_slice)]
mod tests;

pub use handler::McpHandler;

// ---------------------------------------------------------------------------
// Shared private macros, exposed to sibling submodules via `pub(crate) use`.
// ---------------------------------------------------------------------------

/// Build a successful `CallToolResult` from a `serde_json::json!(...)` value.
macro_rules! tool_success {
    ($($json:tt)+) => {
        ::rmcp::model::CallToolResult::success(vec![::rmcp::model::ContentBlock::text(
            ::serde_json::json!($($json)+).to_string(),
        )])
    };
}

/// Build an error `CallToolResult` from a format string or literal.
macro_rules! tool_error {
    ($lit:literal) => {
        ::rmcp::model::CallToolResult::error(vec![::rmcp::model::ContentBlock::text($lit)])
    };
    ($($arg:tt)+) => {
        ::rmcp::model::CallToolResult::error(vec![::rmcp::model::ContentBlock::text(format!($($arg)+))])
    };
}

/// Look up a device by ID and return the entry + connected device, or
/// early-return a `tool_error` `CallToolResult` from the enclosing function.
///
/// Usage: `let (entry, device) = resolve_device!(self, find_camera, &id, "camera");`
/// (the `id` argument is forwarded into `EquipmentRegistry::find_*`,
/// which take `&str` — every real call site passes `&params.camera_id`,
/// `&camera_id`, etc.)
macro_rules! resolve_device {
    ($self:expr, $finder:ident, $id:expr, $kind:literal) => {{
        let Some(entry) = $self.equipment.$finder($id) else {
            return Ok(tool_error!(concat!($kind, " not found: {}"), $id));
        };
        let Some(device) = entry.device() else {
            return Ok(tool_error!(concat!($kind, " not connected: {}"), $id));
        };
        (entry, device)
    }};
}

pub(crate) use resolve_device;
pub(crate) use tool_error;
pub(crate) use tool_success;

// ---------------------------------------------------------------------------
// ServerHandler glue.
//
// The standalone `#[tool_handler(router = self.tool_router)]` reads the
// merged router off `McpHandler::tool_router` (the field populated in
// `McpHandler::new` by summing per-category routers via `+`). We use
// the standalone form rather than the `#[tool_router(server_handler)]`
// shortcut because pattern (c) — multiple per-category `#[tool_router]`
// blocks merged manually — would otherwise emit conflicting
// `ServerHandler` impls.
//
// `call_tool` is written out by hand (the macro only generates it when
// absent) so every call enters the in-flight registry (rp.md § Safety →
// In-Flight Tool Calls) and passes the safety gate before dispatch: one
// place, every tool, including ones added later. The registry hands the
// body its `Cancel` through the request extensions; bodies that block
// read it back with `Cancel::from_context`.
//
// `list_tools` is written out too, for its cache hints: the catalog is
// built once at startup (built-ins plus every provider's tools) and
// stays stable for the life of the process, so a 2026-07-28 client is
// told it may cache the listing (`ttlMs` = `CATALOG_TTL`, `cacheScope`
// `private`) rather than the macro's "never cache" default.
//
// Register first, gate second: the enforcer closes the gate *before*
// it sweeps the registry, so a gated call that registers after the
// sweep sees the closed gate here, and one that registered before it
// is swept — either way nothing gated runs under unsafe skies.
// ---------------------------------------------------------------------------

/// How long a 2026-07-28 client may cache `tools/list`. The catalog
/// never changes while rp runs; the bound only limits how long a stale
/// listing survives an rp restart that added or removed a provider.
pub const CATALOG_TTL: std::time::Duration = std::time::Duration::from_mins(1);

#[rmcp::tool_handler(router = self.tool_router)]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "the tool_handler expansion writes async trait methods whose bodies have no awaits"
)]
impl rmcp::handler::server::ServerHandler for McpHandler {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints
                .then(|| u64::try_from(CATALOG_TTL.as_millis()).unwrap_or(u64::MAX)),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Private),
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        mut context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        // An unknown tool has no class; leave it to the router's own
        // "tool not found" answer rather than registering it.
        let class = self.classes.class_of(&request.name);
        let _guard = class.map(|class| {
            let (guard, cancel) =
                self.in_flight
                    .register(&context.id, &request.name, class, &context.ct);
            context.extensions.insert(cancel);
            guard
        });
        if class == Some(gate::ToolClass::Gated) && !self.safety.is_safe() {
            let monitor = self.safety.unsafe_monitor();
            tracing::debug!(
                tool = %request.name,
                monitor = monitor.as_deref().unwrap_or("<none>"),
                "gated tool refused: conditions are unsafe"
            );
            return Err(gate::safety_unsafe_error(monitor.as_deref()));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }
}
