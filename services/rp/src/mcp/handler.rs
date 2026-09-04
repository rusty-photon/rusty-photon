//! `McpHandler` — the type that owns rp's MCP state and on which all
//! `#[tool]`-annotated methods live.
//!
//! Per-category tools live in
//! sibling submodules under [`super::built_in`]; each declares its own
//! `#[tool_router(router = tool_router_<category>, vis = "pub")]`
//! impl block on this type. [`McpHandler::new`] merges those
//! per-category routers via the `+` operator on
//! [`rmcp::handler::server::router::tool::ToolRouter`].

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;

use crate::equipment::EquipmentRegistry;
use crate::events::EventBus;
use crate::persistence::ImageCache;

use super::gate::ClassTable;
use super::inflight::InFlight;
use crate::safety::SafetyStatus;

/// The slice of the `session.*` config block the tools read at runtime.
///
/// Where captures and the target store land. The block keeps its
/// historical name — it never described a session registry, and
/// `data_directory` / naming patterns are all it carries now.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub data_directory: String,
}

#[derive(Clone)]
pub struct McpHandler {
    pub equipment: Arc<EquipmentRegistry>,
    pub event_bus: Arc<EventBus>,
    pub session_config: SessionConfig,
    pub image_cache: ImageCache,
    /// Configured observer site, if any. `None` when the deployment
    /// has no `site` block (camera-only / flats rigs); ephemeris
    /// tools that require a site (`compute_alt_az`, `get_twilight`,
    /// etc.) error cleanly in that case.
    pub site: Option<rp_ephemeris::Site>,
    /// Planner-wide minimum altitude default (degrees). Read from
    /// `Config.planner.min_altitude_degrees`, falling back to 20°
    /// when omitted.
    pub default_min_altitude_degrees: f64,
    /// Optional plate-solver HTTP client. `None` ⇒ `plate_solve`
    /// MCP tool returns "plate solver not configured". Wired by
    /// `with_plate_solver` from the `plate_solver` block in rp
    /// config.
    pub plate_solver: Option<Arc<dyn rp_plate_solver::PlateSolveClient>>,
    /// Operator-set default applied when the per-call
    /// `search_radius_deg` parameter is omitted. Mirrors
    /// `PlateSolverConfig::default_search_radius_deg`.
    pub plate_solver_default_search_radius_deg: Option<f64>,
    /// Optional guider-service HTTP client. `None` ⇒ every guiding
    /// MCP tool returns "guider not configured". Wired by
    /// `with_guider` from the `guider` block in rp config; the same
    /// client `Arc` is shared with the safety enforcer's
    /// stop-guiding-on-unsafe path.
    pub guider: Option<Arc<dyn rp_guider::GuiderClient>>,
    /// Operator-set guiding defaults (settle threshold/time/timeout,
    /// dither amount) applied when the per-call MCP parameters are
    /// omitted. Mirrors the non-connection fields of
    /// `GuidingConfig`.
    pub guider_defaults: crate::config::GuiderDefaults,
    /// The derived optical-train model (rp.md § Optical Trains).
    /// `do_capture` resolves each camera's `focal_length_mm` through
    /// it for the exposure document's `optics` block. The default
    /// (no trains) is the pre-train behavior — no optics block.
    pub trains: crate::equipment::trains::TrainModel,
    /// The mount motion gate (rp.md § Mount Motion Gate). Behind an
    /// `Arc` so every clone of the handler — rmcp clones it per MCP
    /// connection — contends on the same gate.
    pub motion_gate: Arc<crate::motion_gate::MotionGate>,
    /// Per-rig estimates sizing the advisory `center_on_target` deadline
    /// (§2.5) carried on `centering_started`. Wired by
    /// `with_centering_config` from the `centering` block in rp config;
    /// tests use `CenteringConfig::default()`.
    pub centering: crate::config::CenteringConfig,
    /// Camera-cooling controller (rp.md § Camera Cooling): the body of
    /// the `start_cooldown` / `start_warmup` tools, and read by
    /// `do_capture` to stamp the currently held rung on each exposure
    /// document. `None` in tests that only exercise other tools — the
    /// cooling tools then report "not configured" and frames record no
    /// `cooler_setpoint_c`.
    pub cooling: Option<Arc<crate::cooling::CoolingController>>,
    /// The target store (rp.md § Target Store). `None` in tests that
    /// only exercise other tool categories and configs where opening
    /// it failed to matter — the target CRUD tools then report "target
    /// store not configured". Wired by `with_target_store` from
    /// lib.rs, which always opens one (`targets.db_path`, default
    /// `<data_directory>/targets.redb`).
    pub target_store: Option<Arc<dyn rp_targets::TargetStore>>,
    /// `targets.default_goals` from config — applied by `add_target`
    /// when the caller supplies no `goals[]` (Decision 10).
    pub target_store_defaults: crate::config::TargetStoreConfig,
    /// `session.file_naming_pattern`/`directory_pattern`, compiled once
    /// at startup (Decision 11). `None` when `file_naming_pattern` is
    /// unset — `do_capture` then keeps writing a flat `<doc_uuid_8>.fits`
    /// regardless of `capture`'s `target`/`frame_type` parameters. Wired
    /// by `with_naming_templates` from lib.rs.
    pub naming_templates: Option<Arc<crate::config::naming_template::NamingTemplates>>,
    /// Merged tool catalog. Built by summing per-category routers
    /// in [`McpHandler::new`]; consumed by the
    /// `#[tool_handler(router = self.tool_router)]` `ServerHandler`
    /// impl in [`super`].
    pub tool_router: ToolRouter<Self>,
    /// The in-flight tool-call registry (rp.md § Safety → In-Flight
    /// Tool Calls). Every `tools/call` is entered here by the
    /// `ServerHandler::call_tool` wrapper in [`super`]; the safety
    /// enforcer holds the same `Arc` and cancels the gated entries on
    /// the unsafe transition. Behind an `Arc` so every clone of the
    /// handler — rmcp clones it per MCP connection — shares one
    /// registry.
    pub in_flight: Arc<InFlight>,
    /// The effective tool-class table (rp.md § Safety → In-Flight Tool
    /// Calls): the built-in default with the operator's `safety.gate`
    /// overrides applied. Read at dispatch to refuse gated tools while
    /// unsafe and to register each call with its class; reported by
    /// `get_safety_status`. Behind an `Arc` so every clone of the
    /// handler shares one table.
    pub classes: Arc<ClassTable>,
    /// The safety state the gate reads (rp.md § Safety): written by the
    /// safety enforcer, which holds the same `Arc`; `get_safety_status`
    /// reports it. The default is safe with no monitors — what a
    /// deployment without safety monitors, or a unit test, sees.
    pub safety: Arc<SafetyStatus>,
}

impl McpHandler {
    pub fn new(
        equipment: Arc<EquipmentRegistry>,
        event_bus: Arc<EventBus>,
        session_config: SessionConfig,
        image_cache: ImageCache,
        site: Option<rp_ephemeris::Site>,
    ) -> Self {
        let motion_gate = Arc::new(crate::motion_gate::MotionGate::new(event_bus.clone()));
        Self {
            equipment,
            event_bus,
            session_config,
            image_cache,
            site,
            default_min_altitude_degrees: 20.0,
            plate_solver: None,
            plate_solver_default_search_radius_deg: None,
            guider: None,
            guider_defaults: crate::config::GuiderDefaults::default(),
            trains: crate::equipment::trains::TrainModel::default(),
            motion_gate,
            centering: crate::config::CenteringConfig::default(),
            cooling: None,
            target_store: None,
            target_store_defaults: crate::config::TargetStoreConfig::default(),
            naming_templates: None,
            // Pattern (c) merge: each `built_in/<category>.rs`
            // declares a `#[tool_router(router = tool_router_<name>,
            // vis = "pub")]` block whose generated associated function
            // returns the per-category `ToolRouter<Self>`. The
            // `ToolRouter` type implements `Add` so we sum them into
            // one merged catalog. Adding a new tool category =
            // append one `+ Self::tool_router_<name>()` here.
            tool_router: Self::merged_tool_router(),
            in_flight: Arc::new(InFlight::default()),
            classes: Arc::new(ClassTable::default()),
            safety: Arc::new(SafetyStatus::default()),
        }
    }

    /// The one merged catalog out of the per-category routers.
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "rmcp `ToolRouter`'s `Add` is a catalog merge; there is no arithmetic to overflow"
    )]
    pub(crate) fn merged_tool_router() -> ToolRouter<Self> {
        Self::tool_router_camera()
            + Self::tool_router_imaging()
            + Self::tool_router_filter_wheel()
            + Self::tool_router_cover_calibrator()
            + Self::tool_router_focuser()
            + Self::tool_router_mount()
            + Self::tool_router_auto_focus()
            + Self::tool_router_rotator()
            + Self::tool_router_plate_solve()
            + Self::tool_router_guider()
            + Self::tool_router_center_on_target()
            + Self::tool_router_planner()
            + Self::tool_router_targets()
            + Self::tool_router_plan_schema()
            + Self::tool_router_safety()
            + Self::tool_router_cooling()
    }

    /// Wire the effective tool-class table (the built-in default with
    /// the `safety.gate` overrides applied). The lib.rs build path
    /// calls this with the table it built from config; tests keep the
    /// built-in default.
    #[must_use]
    pub fn with_class_table(mut self, classes: Arc<ClassTable>) -> Self {
        self.classes = classes;
        self
    }

    /// Merge the tool providers' proxy routes into the catalog (rp.md
    /// § Plugin-Provided Tools). The merge was already checked for
    /// collisions by `Providers::connect`, so every route lands under
    /// a name no built-in uses; the class table must already carry the
    /// provider tools (`ClassTable::with_catalog`) or the dispatch
    /// treats them as unknown.
    #[must_use]
    pub fn with_providers(mut self, providers: &super::providers::Providers) -> Self {
        for route in providers.routes() {
            self.tool_router.add_route(route);
        }
        self
    }

    /// Share the safety status with the safety enforcer, so the gate
    /// at dispatch reads what the enforcer writes (rp.md § Safety).
    /// Tests that never flip conditions keep the safe default.
    #[must_use]
    pub fn with_safety_status(mut self, safety: Arc<SafetyStatus>) -> Self {
        self.safety = safety;
        self
    }

    /// Wire the planner-wide minimum-altitude default after
    /// construction. The lib.rs build path calls this with
    /// `planner.min_altitude_degrees` (defaulting to 20°); it is the
    /// altitude floor `get_next_target` applies to a store-backed
    /// target that carries no per-target or `default_scheduling`
    /// override. Tests can leave the default as-is.
    #[must_use]
    pub const fn with_planner_default_min_altitude(
        mut self,
        default_min_altitude_degrees: f64,
    ) -> Self {
        self.default_min_altitude_degrees = default_min_altitude_degrees;
        self
    }

    /// Wire the plate-solver HTTP client + operator-set search-radius
    /// default. `None` for `client` keeps the MCP tool reporting
    /// "not configured"; `None` for the radius means the wrapper
    /// falls through to ASTAP's own default when the per-call
    /// parameter is also omitted.
    #[must_use]
    pub fn with_plate_solver(
        mut self,
        client: Option<Arc<dyn rp_plate_solver::PlateSolveClient>>,
        default_search_radius_deg: Option<f64>,
    ) -> Self {
        self.plate_solver = client;
        self.plate_solver_default_search_radius_deg = default_search_radius_deg;
        self
    }

    /// Wire the guider-service HTTP client + operator-set guiding
    /// defaults. `None` for `client` keeps the guiding MCP tools
    /// reporting "not configured"; unset fields in `defaults` mean
    /// the per-call parameters (or the guider service's own
    /// `settling` config) decide.
    #[must_use]
    pub fn with_guider(
        mut self,
        client: Option<Arc<dyn rp_guider::GuiderClient>>,
        defaults: crate::config::GuiderDefaults,
    ) -> Self {
        self.guider = client;
        self.guider_defaults = defaults;
        self
    }

    /// Wire the derived optical-train model. The lib.rs build path
    /// calls this with the model built from `equipment.optical_trains`;
    /// tests without trains keep the empty default (no optics block).
    #[must_use]
    pub fn with_trains(mut self, trains: crate::equipment::trains::TrainModel) -> Self {
        self.trains = trains;
        self
    }

    /// Wire the per-rig centering estimates (§2.5) from the `centering`
    /// config block. The lib.rs build path calls this with
    /// `config.centering`; tests leave the default.
    #[must_use]
    pub const fn with_centering_config(
        mut self,
        centering: crate::config::CenteringConfig,
    ) -> Self {
        self.centering = centering;
        self
    }

    /// Wire the camera-cooling controller behind the `start_cooldown` /
    /// `start_warmup` tools and `do_capture`'s `cooler_setpoint_c` stamp
    /// (rp.md § Camera Cooling). Tests leave `None`.
    #[must_use]
    pub fn with_cooling(mut self, cooling: Arc<crate::cooling::CoolingController>) -> Self {
        self.cooling = Some(cooling);
        self
    }

    /// Wire the target store (rp.md § Target Store) plus its config
    /// defaults. The lib.rs build path always calls this with `Some`
    /// (it opens the store unconditionally); tests that don't need
    /// target tools leave the `None` default.
    #[must_use]
    pub fn with_target_store(
        mut self,
        store: Option<Arc<dyn rp_targets::TargetStore>>,
        defaults: crate::config::TargetStoreConfig,
    ) -> Self {
        self.target_store = store;
        self.target_store_defaults = defaults;
        self
    }

    /// Wire the compiled `session.file_naming_pattern`/`directory_pattern`
    /// (Decision 11). `None` when `file_naming_pattern` is unset.
    #[must_use]
    pub fn with_naming_templates(
        mut self,
        naming_templates: Option<Arc<crate::config::naming_template::NamingTemplates>>,
    ) -> Self {
        self.naming_templates = naming_templates;
        self
    }
}
