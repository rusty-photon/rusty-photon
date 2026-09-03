#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod config;
pub mod config_actions;
pub mod cooling;
pub mod doctor;
pub mod equipment;
pub mod error;
pub mod events;
pub mod guiding_watch;
pub mod imaging;
pub mod mcp;
pub mod motion_gate;
pub mod persistence;
pub mod planner;
pub mod routes;
pub mod safety;
pub mod session;

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tracing::{debug, info, warn};

use rusty_photon_tls::config::TlsConfig;
use tokio_util::sync::CancellationToken;

use crate::config::{AdvertisedUrl, Config};
use crate::equipment::EquipmentRegistry;
use crate::error::Result;
use crate::events::EventBus;
use crate::mcp::McpHandler;
use crate::persistence::ImageCache;
use crate::routes::{build_router, AppState};
use crate::safety::{AlpacaSafetyProbe, SafetyEnforcer, SafetyStatus};
use crate::session::{SessionConfig, SessionManager};

/// Builder for the rp server.
///
/// Configures equipment, event bus, session manager, and MCP handler,
/// then binds the server. The returned [`BoundServer`] can be inspected
/// (e.g. `listen_addr()`) before calling `start()`.
pub struct ServerBuilder {
    config: Option<Config>,
    config_path: Option<std::path::PathBuf>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config: None,
            config_path: None,
        }
    }

    #[must_use]
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// The resolved config-file path the server was loaded from.
    /// `PUT /api/config` persists to it.
    #[must_use]
    pub fn with_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Connect the equipment, assemble every subsystem from the config,
    /// and bind the listener — everything short of serving.
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Config`](crate::error::RpError::Config) if the
    /// builder is missing its config or config path, or if any
    /// config-derived piece fails to validate or build (plugin
    /// registrations, site, target store, planner altitude floor, plate
    /// solver, optical trains, naming templates, guider client);
    /// [`RpError::SiteMismatch`](crate::error::RpError::SiteMismatch) if
    /// the mount disagrees with the configured site; and
    /// [`RpError::Io`](crate::error::RpError::Io) if the listener cannot
    /// bind. Equipment that fails to connect is not an error here — the
    /// registry records it as disconnected.
    pub async fn build(self) -> Result<BoundServer> {
        let config = required(
            self.config,
            "config is required \u{2014} call .with_config(...) first",
        )?;
        let config_path = required(
            self.config_path,
            "config path is required \u{2014} call .with_config_path(...) first",
        )?;
        // The effective running config: rp has no config-overriding CLI
        // flags, so this is exactly the loaded file. Served by
        // `GET /api/config` and diffed against by `PUT /api/config`.
        let effective_config = Arc::new(config.clone());
        let bind_addr = config.server.socket_addr();

        let equipment = connect_equipment(&config).await?;

        let SessionStack {
            event_bus,
            cooling,
            session,
        } = build_session_stack(&config, &equipment)?;

        let session_config = SessionConfig {
            data_directory: config.session.data_directory.clone(),
        };

        let naming_templates = build_naming_templates(&config)?;
        let image_cache = build_image_cache(&config, naming_templates.as_deref());

        let site = build_site(&config)?;

        // Target store (rp.md § Target Store): the sole source of planner
        // targets (the legacy `targets[]` config array was retired).
        let (target_store, target_store_config) = open_target_store(&config).await?;
        let default_min_alt = planner_default_min_altitude(&config)?;
        let (plate_solver_client, plate_solver_default_radius) = build_plate_solver(&config)?;
        let trains = validated_trains(&config)?;
        if let Some(warning) = crate::config::progress_derivation_warning(&config.session) {
            tracing::warn!("{}", warning);
        }

        let guiding_config = config
            .equipment
            .mount
            .as_ref()
            .and_then(|m| m.guiding.as_ref());
        let (guider_client, guider_defaults) = build_guider(&config, guiding_config)?;
        spawn_focus_watch_if_configured(
            &trains,
            guider_client.clone(),
            guiding_config.and_then(|g| g.focus_watch),
            &event_bus,
        );

        // The safety state is shared between the enforcer (writer) and
        // the MCP dispatch (the gate reads it), so it is built before
        // either; the class table is the built-in default with the
        // operator's `safety.gate` overrides applied, logged once here
        // so an operator can see what their config did.
        let safety_status = Arc::new(SafetyStatus::default());
        let classes = build_class_table(&config)?;

        let mcp = McpHandler::new(
            equipment.clone(),
            event_bus.clone(),
            session_config,
            image_cache.clone(),
            site,
        )
        .with_class_table(classes)
        .with_safety_status(safety_status.clone())
        .with_planner_default_min_altitude(default_min_alt)
        .with_plate_solver(plate_solver_client, plate_solver_default_radius)
        .with_guider(guider_client.clone(), guider_defaults)
        .with_trains(trains)
        .with_centering_config(config.centering.clone())
        .with_cooling(cooling)
        .with_target_store(Some(target_store), target_store_config)
        .with_naming_templates(naming_templates);

        // Cancellation token for in-flight SSE streams
        // (`/api/events/subscribe`). Cloned into AppState so the handler can
        // end its stream, and stored on BoundServer so `start()` can cancel it
        // when the lifecycle shutdown fires — otherwise a long-lived SSE body
        // would block axum's graceful shutdown from ever completing.
        let sse_shutdown = CancellationToken::new();

        let safety = build_safety(&config, &session, &mcp, guider_client, &safety_status);

        let reconnect = build_reconnect(&config, &equipment, &event_bus);

        let state = AppState {
            equipment,
            mcp,
            session: session.clone(),
            image_cache,
            sse_shutdown: sse_shutdown.clone(),
            config: effective_config,
            config_path: Arc::new(config_path),
        };

        let (router, hostname) = assemble_router(state, &config, bind_addr.ip());

        let tls = config.server.tls.clone();

        let (listener, local_addr) = bind_and_advertise(
            bind_addr,
            &config,
            &session,
            tls.is_some(),
            hostname.as_deref(),
        )
        .await?;

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls,
            sse_shutdown,
            safety,
            reconnect,
            session,
            safety_status,
        })
    }
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The effective tool-class table (rp.md § Safety → In-Flight Tool
/// Calls → Operator overrides), logged once so the operator can see
/// what `safety.gate` did.
///
/// # Errors
///
/// Returns [`RpError::Config`] naming every offending entry when the
/// overrides name an unknown tool or list one on both sides. The
/// config loader already ran the same check, so this only fires for a
/// `Config` built without it.
fn build_class_table(config: &Config) -> Result<Arc<crate::mcp::gate::ClassTable>> {
    let classes =
        crate::mcp::gate::ClassTable::with_overrides(&config.safety.gate).map_err(|errors| {
            let detail: Vec<String> = errors
                .iter()
                .map(|e| format!("{} {}", e.path, e.msg))
                .collect();
            crate::error::RpError::Config(detail.join("; "))
        })?;
    let overrides = &config.safety.gate;
    if overrides.gated.is_empty() && overrides.ungated.is_empty() {
        info!(gated = ?classes.gated(), "safety gate: built-in tool classes");
    } else {
        info!(
            gated = ?classes.gated(),
            gated_by_override = ?overrides.gated,
            ungated_by_override = ?overrides.ungated,
            "safety gate: tool classes with safety.gate overrides applied"
        );
    }
    Ok(Arc::new(classes))
}

/// Safety enforcement (rp.md § Safety): the enforcer writes the safety
/// status the MCP dispatch gates on, and the handler's in-flight
/// tool-call registry is shared with it so an unsafe transition can
/// cancel every gated call in flight. The enforcer is `None` when no
/// safety monitors are configured — sessions then run ungated and no
/// polling task is spawned.
///
/// The enforcer never touches the MCP transport: an unsafe transition
/// cancels in-flight calls through the in-flight registry and closes
/// the gate; the transport itself is session-less (rp.md § MCP Server)
/// and keeps serving every request.
fn build_safety(
    config: &Config,
    session: &Arc<SessionManager>,
    mcp: &McpHandler,
    guider_client: Option<Arc<dyn rp_guider::GuiderClient>>,
    safety_status: &Arc<SafetyStatus>,
) -> Option<SafetyEnforcer<AlpacaSafetyProbe>> {
    SafetyEnforcer::from_registry(
        mcp.equipment.clone(),
        mcp.event_bus.clone(),
        session.clone(),
        mcp.in_flight.clone(),
        safety_status.clone(),
        guider_client,
        config.safety.poll_interval,
    )
}

/// Reconnect supervisor (rp.md § Device Session Recovery): heals device
/// sessions killed by a downstream service restart and picks up devices
/// that were unreachable at startup.
fn build_reconnect(
    config: &Config,
    equipment: &Arc<EquipmentRegistry>,
    event_bus: &Arc<EventBus>,
) -> crate::equipment::ReconnectSupervisor {
    crate::equipment::ReconnectSupervisor::new(
        equipment.clone(),
        event_bus.clone(),
        config.equipment.reconnect_interval,
        config.ca_cert_path().map(std::path::Path::to_path_buf),
    )
}

/// The builder-contract error for a missing required builder field.
fn required<T>(value: Option<T>, hint: &str) -> Result<T> {
    value.ok_or_else(|| crate::error::RpError::Config(format!("ServerBuilder::build: {hint}")))
}

/// Connect the equipment registry, then validate the configured site
/// against the mount's reported SiteLatitude/SiteLongitude. A mismatch
/// beyond 0.01° aborts startup — see docs/services/rp.md §"Site
/// Validation Against the ASCOM Mount". When site config is omitted,
/// the mount lacks the property, or no mount is configured, this is a
/// debug-logged no-op.
async fn connect_equipment(config: &Config) -> Result<Arc<EquipmentRegistry>> {
    debug!("initializing equipment registry");
    let equipment =
        Arc::new(EquipmentRegistry::new(&config.equipment, config.ca_cert_path()).await);
    equipment.validate_site(config.site.as_ref()).await?;
    Ok(equipment)
}

/// The event/session spine of the server: the pieces that share Arcs
/// with each other before the rest of the assembly consumes them.
struct SessionStack {
    event_bus: Arc<EventBus>,
    cooling: Arc<crate::cooling::CoolingController>,
    session: Arc<SessionManager>,
}

/// Build the event bus, the camera-cooling controller (rp.md § Camera
/// Cooling; the `start_cooldown` / `start_warmup` tools drive it — and,
/// until the registry goes, session start and the transitions to idle
/// — while `do_capture` reads the held rung per frame), and the session
/// manager wired to both.
fn build_session_stack(
    config: &Config,
    equipment: &Arc<EquipmentRegistry>,
) -> Result<SessionStack> {
    debug!("initializing event bus");
    let event_bus = Arc::new(
        EventBus::from_config(&config.plugins, config.ca_cert_path())
            .map_err(crate::error::RpError::Config)?,
    );

    debug!("initializing session manager");
    let cooling = Arc::new(crate::cooling::CoolingController::new(
        equipment.clone(),
        event_bus.clone(),
        config.cooling.clone(),
    ));
    let session = Arc::new(
        SessionManager::new(event_bus.clone(), &config.plugins, config.ca_cert_path())
            .map_err(crate::error::RpError::Config)?
            .with_state_path(config.session.session_state_path())
            .with_cooling(cooling.clone()),
    );
    Ok(SessionStack {
        event_bus,
        cooling,
        session,
    })
}

/// Assemble the HTTP router: the MCP `Host` allowlist (the system
/// hostname is read once here and also feeds the advertised URL, so
/// the endpoint accepts every URL rp hands out — rp.md § MCP Server),
/// the route table, and the optional authentication layer. Returns the
/// router plus the hostname for `advertised_base_url`.
fn assemble_router(
    state: AppState,
    config: &Config,
    bind_ip: IpAddr,
) -> (axum::Router, Option<String>) {
    let hostname = hostname::get().ok().and_then(|h| h.into_string().ok());
    let mcp_allowed_hosts = additional_allowed_hosts(
        config.server.advertised_url.as_ref(),
        bind_ip,
        hostname.as_deref(),
        &interface_addrs(bind_ip),
    );
    debug!(
        ?mcp_allowed_hosts,
        "MCP Host allowlist, in addition to the loopback defaults"
    );
    let router = build_router(state, mcp_allowed_hosts);

    // Layer authentication if configured
    let router = match &config.server.auth {
        Some(auth) => {
            if config.server.tls.is_none() {
                tracing::warn!(
                    "Authentication is enabled but TLS is not. \
                     Credentials will be transmitted in cleartext. \
                     Consider enabling TLS (see `doctor --fix`)."
                );
            }
            rp_auth::layer(router, auth)
        }
        None => router,
    };
    (router, hostname)
}

/// Build the observer site (if configured) once, at startup, so the
/// tzf-rs `DefaultFinder` is constructed exactly once per process and
/// the IANA timezone is logged on the same path that populates
/// `McpHandler`.
fn build_site(config: &Config) -> Result<Option<rp_ephemeris::Site>> {
    config
        .site
        .as_ref()
        .map(|site_cfg| {
            let site =
                rp_ephemeris::Site::new(site_cfg.latitude_degrees, site_cfg.longitude_degrees)
                    .map_err(|e| crate::error::RpError::Config(format!("site: {e}")))?;
            tracing::info!("{}", site);
            Ok(site)
        })
        .transpose()
}

/// Open the target store, at `target_store.db_path` or
/// `<data_directory>/targets.redb` — unconditionally; it is the sole
/// source of planner targets.
async fn open_target_store(
    config: &Config,
) -> Result<(
    Arc<dyn rp_targets::TargetStore>,
    crate::config::target_store::TargetStoreConfig,
)> {
    let target_store_config =
        crate::config::target_store::parse_target_store_config(&config.target_store)
            .map_err(|e| crate::error::RpError::Config(format!("target_store: {e}")))?;
    let target_store_db_path = target_store_config.db_path.clone().unwrap_or_else(|| {
        std::path::Path::new(&config.session.data_directory)
            .join("targets.redb")
            .to_string_lossy()
            .into_owned()
    });
    if let Some(parent) = std::path::Path::new(&target_store_db_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::error::RpError::Config(format!(
                "target_store.db_path {target_store_db_path:?}: failed to create parent directory: {e}"
            ))
        })?;
    }
    let target_store: Arc<dyn rp_targets::TargetStore> = Arc::new(
        rp_targets::RedbTargetStore::open(&target_store_db_path)
            .await
            .map_err(|e| {
                crate::error::RpError::Config(format!(
                    "target_store.db_path {target_store_db_path:?}: failed to open target store: {e}"
                ))
            })?,
    );
    Ok((target_store, target_store_config))
}

/// `planner.min_altitude_degrees` — the planner-wide default floor for
/// `get_next_target` (per-target overrides apply). Range-validated at
/// startup so a config typo (e.g. `200`) fails loud rather than
/// silently changing planner behaviour.
fn planner_default_min_altitude(config: &Config) -> Result<f64> {
    match config
        .planner
        .get("min_altitude_degrees")
        .and_then(serde_json::Value::as_f64)
    {
        Some(v) if (-90.0..=90.0).contains(&v) => Ok(v),
        Some(v) => Err(crate::error::RpError::Config(format!(
            "planner.min_altitude_degrees must be in [-90, 90]; got {v}"
        ))),
        None => Ok(20.0),
    }
}

/// The optional plate-solver client plus its configured default search
/// radius.
type PlateSolverParts = (
    Option<Arc<dyn rp_plate_solver::PlateSolveClient>>,
    Option<f64>,
);

/// Build the plate-solver HTTP client when the operator configured
/// one. Failure to build (e.g. invalid TLS bag in reqwest's builder)
/// aborts startup loud rather than silently disabling the tool — same
/// posture as wrapper config validation.
fn build_plate_solver(config: &Config) -> Result<PlateSolverParts> {
    match &config.plate_solver {
        Some(ps_cfg) => {
            let client = rp_plate_solver::PlateSolverClient::new(
                ps_cfg.url.clone(),
                ps_cfg.timeout,
                ps_cfg.auth.as_ref(),
                config.ca_cert_path(),
            )
            .map_err(|e| {
                crate::error::RpError::Config(format!(
                    "plate_solver: failed to build HTTP client: {e}"
                ))
            })?;
            let arc: Arc<dyn rp_plate_solver::PlateSolveClient> = Arc::new(client);
            Ok((Some(arc), ps_cfg.default_search_radius_deg))
        }
        None => Ok((None, None)),
    }
}

/// The derived optical-train model (rp.md § Optical Trains).
/// `load_config` already ran the same graph validation, so a failure
/// here is a code bug — surface it loud regardless.
fn validated_trains(config: &Config) -> Result<crate::equipment::trains::TrainModel> {
    crate::equipment::trains::TrainModel::try_from_equipment(&config.equipment).map_err(|errors| {
        let msg = errors.first().map_or_else(
            || "optical_trains validation failed".to_string(),
            |e| format!("{} {}", e.path, e.msg),
        );
        crate::error::RpError::Config(msg)
    })
}

/// `capture`'s target-linkage naming templates (rp.md § Capture Tool
/// Details, Decision 11). `load_config` already validated both
/// patterns via the same `naming_template::validate_*` calls this
/// compiles with, so a failure here is a code bug — surface it loud
/// regardless, same posture as `validated_trains`.
fn build_naming_templates(
    config: &Config,
) -> Result<Option<Arc<crate::config::naming_template::NamingTemplates>>> {
    Ok(
        crate::config::naming_template::NamingTemplates::from_session_config(&config.session)
            .map_err(|e| {
                crate::error::RpError::Config(format!(
                    "session naming templates: {e} (load_config should have already \
                     rejected this)"
                ))
            })?
            .map(Arc::new),
    )
}

/// The image cache, with its disk-fallback scan bounded to the depth
/// `capture` writes under `data_directory` — `directory_pattern`'s
/// component count, or 0 with no naming template (rp.md § Document
/// Resolution). That is why the templates are built before the cache.
fn build_image_cache(
    config: &Config,
    naming_templates: Option<&crate::config::naming_template::NamingTemplates>,
) -> ImageCache {
    let frame_directory_depth =
        naming_templates.map_or(0, |templates| templates.directory.path_component_count());
    ImageCache::new(
        config.imaging.cache_max_mib,
        config.imaging.cache_max_images,
        std::path::PathBuf::from(&config.session.data_directory),
        frame_directory_depth,
    )
}

/// Build the guider HTTP client when the operator configured one
/// (`equipment.mount.guiding`) — same aborts-loud posture as the plate
/// solver. The client Arc is shared between the MCP tools and the
/// safety enforcer's stop-guiding-on-unsafe step.
fn build_guider(
    config: &Config,
    guiding_config: Option<&crate::config::guiding::GuidingConfig>,
) -> Result<(
    Option<Arc<dyn rp_guider::GuiderClient>>,
    crate::config::GuiderDefaults,
)> {
    match guiding_config {
        Some(g_cfg) => {
            let client = rp_guider::GuiderServiceClient::new(
                g_cfg.url.clone(),
                g_cfg.timeout,
                g_cfg.auth.as_ref(),
                config.ca_cert_path(),
            )
            .map_err(|e| {
                crate::error::RpError::Config(format!("guider: failed to build HTTP client: {e}"))
            })?;
            let arc: Arc<dyn rp_guider::GuiderClient> = Arc::new(client);
            Ok((Some(arc), g_cfg.defaults()))
        }
        None => Ok((None, crate::config::GuiderDefaults::default())),
    }
}

/// Spawn the Guide Focus Watch (rp.md § Guide Focus Watch) when the
/// operator configured `focus_watch` and a guider client exists.
/// Events only — the orchestrator owns the refocus response.
fn spawn_focus_watch_if_configured(
    trains: &crate::equipment::trains::TrainModel,
    guider_client: Option<Arc<dyn rp_guider::GuiderClient>>,
    watch_cfg: Option<crate::config::guiding::FocusWatchConfig>,
    event_bus: &Arc<EventBus>,
) {
    let (Some(client), Some(watch_cfg)) = (guider_client, watch_cfg) else {
        return;
    };
    let guiding_train_id = trains.guiding_train().map(|t| t.id.clone());
    let guiding_focusers: Vec<String> = trains
        .guiding_train()
        .map(|t| {
            t.devices
                .iter()
                .filter(|d| d.kind == crate::equipment::trains::TrainDeviceKind::Focuser)
                .map(|d| d.id.clone())
                .collect()
        })
        .unwrap_or_default();
    crate::guiding_watch::spawn(
        client,
        event_bus.clone(),
        watch_cfg,
        guiding_train_id,
        guiding_focusers,
    );
}

/// Bind the listener and advertise the MCP base URL to orchestrators
/// via the session manager. The stdout line is parsed by BDD tests to
/// discover the bound port, so it must go to stdout (not
/// tracing/stderr) — console mode only: stdout is a dead handle under
/// the Windows SCM, and the only stdout consumer (bdd-infra's port
/// parser) never runs services with `--service`.
async fn bind_and_advertise(
    bind_addr: SocketAddr,
    config: &Config,
    session: &Arc<SessionManager>,
    tls: bool,
    hostname: Option<&str>,
) -> Result<(tokio::net::TcpListener, SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    // Set the MCP base URL on the session manager
    let scheme = if tls { "https" } else { "http" };
    let base_url = advertised_base_url(
        config.server.advertised_url.as_ref(),
        scheme,
        local_addr,
        hostname,
    );
    debug!(%base_url, "advertising MCP base URL to orchestrators");
    session.set_mcp_base_url(base_url).await;

    if !rusty_photon_service_lifecycle::is_scm_service() {
        println!("Bound rp server bound_addr={local_addr}");
    }
    info!("rp service bound on {}", local_addr);
    Ok((listener, local_addr))
}

/// The base URL advertised to orchestrators as `mcp_server_url`
/// (rp.md § MCP Server): the configured `advertised_url` verbatim,
/// else derived from the bound listener. A wildcard bind advertises
/// the system hostname — a SAN on every doctor-provisioned
/// certificate — because the literal wildcard address is dialable by
/// nobody and covered by no certificate; without a hostname it falls
/// back to loopback so co-located orchestrators still connect.
fn advertised_base_url(
    advertised: Option<&AdvertisedUrl>,
    scheme: &str,
    local_addr: SocketAddr,
    hostname: Option<&str>,
) -> String {
    if let Some(url) = advertised {
        return url.as_str().to_string();
    }
    if !local_addr.ip().is_unspecified() {
        return format!("{scheme}://{local_addr}");
    }
    let port = local_addr.port();
    hostname.map_or_else(
        || {
            warn!(
                "wildcard bind and no system hostname; advertising loopback — \
                 remote orchestrators cannot connect"
            );
            format!("{scheme}://127.0.0.1:{port}")
        },
        |host| format!("{scheme}://{host}:{port}"),
    )
}

/// The `Host` authorities rp's MCP transport accepts *in addition to*
/// rmcp's loopback defaults (rp.md § MCP Server). Derived from the same
/// facts as [`advertised_base_url`], so the URL rp hands an orchestrator
/// is never one its own DNS-rebinding protection rejects: the system
/// hostname (what a wildcard bind advertises), the authority of a
/// configured `advertised_url`, the literal bind address of an explicit
/// bind, and — for a wildcard bind — the interface addresses that
/// listener really answers on.
///
/// Entries carry no port (matching the name on any port) except the one
/// from `advertised_url`, which is taken verbatim so a configured port
/// is honoured.
fn additional_allowed_hosts(
    advertised: Option<&AdvertisedUrl>,
    bind_ip: IpAddr,
    hostname: Option<&str>,
    interface_addrs: &[IpAddr],
) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(host) = hostname {
        hosts.push(host.to_string());
    }
    if let Some(url) = advertised {
        hosts.push(advertised_authority(url.as_str()).to_string());
    }
    // A wildcard bind names no reachable address of its own; the
    // interface addresses below stand in for it.
    if !bind_ip.is_unspecified() {
        hosts.push(bind_ip.to_string());
    }
    hosts.extend(interface_addrs.iter().map(IpAddr::to_string));

    let mut seen = std::collections::HashSet::new();
    hosts.retain(|host| !host.is_empty() && seen.insert(host.clone()));
    hosts
}

/// The `host[:port]` authority of an advertised base URL — everything
/// between `://` and the first path separator. [`AdvertisedUrl`] has
/// already validated the scheme and a non-empty remainder at config load.
fn advertised_authority(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
}

/// Every non-loopback address a local interface answers on, for a
/// wildcard bind — an orchestrator holding only a LAN address dials one
/// of these. Empty for an explicit bind (that single address is added on
/// its own) and on enumeration failure, which is logged and non-fatal:
/// the hostname and loopback entries still stand.
fn interface_addrs(bind_ip: IpAddr) -> Vec<IpAddr> {
    if !bind_ip.is_unspecified() {
        return Vec::new();
    }
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces
            .iter()
            .filter(|interface| !interface.is_loopback())
            .map(if_addrs::Interface::ip)
            .collect(),
        Err(e) => {
            warn!(
                error = %e,
                "could not enumerate local interfaces; MCP requests addressed to a \
                 LAN address of this host will be rejected"
            );
            Vec::new()
        }
    }
}

/// A fully bound rp server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    /// Cancelled by `start()` when the lifecycle shutdown future resolves, so
    /// in-flight `/api/events/subscribe` SSE streams end and axum's graceful
    /// shutdown can drain. A clone lives in `AppState` for the handler.
    sse_shutdown: CancellationToken,
    /// Safety polling loop, spawned by `start()` and cancelled on shutdown.
    /// `None` when no safety monitors are configured.
    safety: Option<SafetyEnforcer<AlpacaSafetyProbe>>,
    /// Reconnect supervisor (rp.md § Device Session Recovery), spawned
    /// by `start()` and cancelled on shutdown.
    reconnect: crate::equipment::ReconnectSupervisor,
    /// Kept so `start()` can run startup recovery (rp.md § Recovery
    /// Behavior) once the server is about to serve.
    session: Arc<SessionManager>,
    /// The safety state, read by `start()` after the inline first
    /// safety poll to decide whether startup recovery may re-invoke.
    safety_status: Arc<SafetyStatus>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the server until `shutdown` resolves: one inline safety poll,
    /// startup recovery, then the HTTP(S) serve loop with graceful
    /// drain.
    ///
    /// # Errors
    ///
    /// Returns [`RpError::Server`](crate::error::RpError::Server) if the
    /// TLS serve loop fails, or [`RpError::Io`](crate::error::RpError::Io)
    /// if the plain one does; a clean shutdown is `Ok`.
    pub async fn start(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        // Safety before recovery (rp.md § Recovery Behavior): with
        // monitors configured, complete one poll inline so the safety
        // gate reflects reality — and unsafe conditions already secured
        // the equipment — before any orchestrator is re-invoked. The
        // loop then continues from that state. The lifecycle shutdown
        // is chained below.
        let safety_cancel = CancellationToken::new();
        let safety_task = match self.safety {
            Some(enforcer) => {
                let mut per_monitor = std::collections::HashMap::new();
                let overall = enforcer.poll_once(&mut per_monitor, true).await;
                let cancel = safety_cancel.clone();
                Some(tokio::spawn(enforcer.run_from(
                    cancel,
                    per_monitor,
                    overall,
                )))
            }
            None => None,
        };

        // The reconnect supervisor rides the same cancellation as the
        // safety loop; the startup connect just ran, so its first pass
        // is one full interval out.
        let reconnect_task = tokio::spawn(self.reconnect.run(safety_cancel.clone()));

        // Startup recovery (rp.md § Recovery Behavior): restore a
        // persisted session — re-invoking the orchestrator only under
        // safe conditions; under unsafe ones the session is restored
        // interrupted and the safe transition resumes it. The listener
        // is already bound, so the re-invoked orchestrator's immediate
        // connect-back queues in the accept backlog until `axum::serve`
        // below starts draining.
        self.session
            .recover_startup(self.safety_status.is_safe())
            .await;

        // Chain the lifecycle shutdown to the SSE cancellation token: when the
        // signal fires, cancel in-flight `/api/events/subscribe` streams first
        // so their long-lived response bodies end, then let axum's graceful
        // shutdown drain the remaining connections. Without this, a connected
        // SSE consumer would keep the server from ever shutting down. The
        // safety polling loop rides the same signal.
        let sse_shutdown = self.sse_shutdown;
        let graceful = async move {
            shutdown.await;
            sse_shutdown.cancel();
            safety_cancel.cancel();
        };

        if let Some(ref tls_config) = self.tls {
            info!("rp service started on {} (TLS)", self.local_addr);
            rusty_photon_tls::server::serve_tls(self.listener, self.router, tls_config, graceful)
                .await
                .map_err(|e| crate::error::RpError::Server(e.to_string()))?;
        } else {
            info!("rp service started on {}", self.local_addr);
            axum::serve(self.listener, self.router)
                .with_graceful_shutdown(graceful)
                .await?;
        }

        // The safety loop and reconnect supervisor were cancelled by
        // `graceful`; join them so the process doesn't exit
        // mid-transition.
        if let Some(task) = safety_task {
            let _ = task.await;
        }
        let _ = reconnect_task.await;

        debug!("rp service shut down");
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn advertised(url: &str) -> AdvertisedUrl {
        serde_json::from_value(serde_json::json!(url)).unwrap()
    }

    #[test]
    fn explicit_bind_advertises_the_literal_address() {
        let url = advertised_base_url(None, "http", addr("127.0.0.1:11115"), Some("rig"));
        assert_eq!(url, "http://127.0.0.1:11115");
    }

    #[test]
    fn wildcard_bind_advertises_the_hostname() {
        let url = advertised_base_url(None, "https", addr("0.0.0.0:11115"), Some("rig"));
        assert_eq!(url, "https://rig:11115");
    }

    #[test]
    fn ipv6_wildcard_bind_advertises_the_hostname() {
        let url = advertised_base_url(None, "https", addr("[::]:11115"), Some("rig"));
        assert_eq!(url, "https://rig:11115");
    }

    #[test]
    fn wildcard_bind_without_hostname_advertises_loopback() {
        let url = advertised_base_url(None, "https", addr("0.0.0.0:11115"), None);
        assert_eq!(url, "https://127.0.0.1:11115");
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn explicit_bind_allows_its_own_address_and_the_hostname() {
        let hosts = additional_allowed_hosts(None, ip("192.168.1.10"), Some("rig"), &[]);
        assert_eq!(hosts, vec!["rig", "192.168.1.10"]);
    }

    #[test]
    fn wildcard_bind_allows_the_hostname_and_every_interface_address() {
        // The wildcard address itself is dialable by nobody, so it is not
        // an entry; the addresses that listener answers on are.
        let hosts = additional_allowed_hosts(
            None,
            ip("0.0.0.0"),
            Some("rig"),
            &[ip("192.168.1.10"), ip("2001:db8::1")],
        );
        assert_eq!(hosts, vec!["rig", "192.168.1.10", "2001:db8::1"]);
    }

    #[test]
    fn ipv6_wildcard_bind_allows_the_hostname_and_every_interface_address() {
        let hosts = additional_allowed_hosts(None, ip("::"), Some("rig"), &[ip("2001:db8::1")]);
        assert_eq!(hosts, vec!["rig", "2001:db8::1"]);
    }

    #[test]
    fn the_advertised_host_is_allowed_with_its_port() {
        // The URL rp hands out must be one rp accepts — the bug in #792.
        let hosts = additional_allowed_hosts(
            Some(&advertised("https://observatory.example:11115")),
            ip("0.0.0.0"),
            Some("rig"),
            &[],
        );
        assert_eq!(hosts, vec!["rig", "observatory.example:11115"]);
    }

    #[test]
    fn allowed_hosts_are_deduplicated() {
        // A hostname-shaped interface address, or an advertised URL naming
        // the bind address, must not produce a duplicate entry.
        let hosts = additional_allowed_hosts(
            Some(&advertised("http://192.168.1.10")),
            ip("192.168.1.10"),
            Some("rig"),
            &[],
        );
        assert_eq!(hosts, vec!["rig", "192.168.1.10"]);
    }

    #[test]
    fn without_a_hostname_only_the_derivable_addresses_are_allowed() {
        // Loopback still answers: rmcp's defaults hold it, and this list
        // only ever extends them.
        let hosts = additional_allowed_hosts(None, ip("0.0.0.0"), None, &[]);
        assert!(hosts.is_empty(), "{hosts:?}");
    }

    #[test]
    fn advertised_authority_keeps_host_and_port_and_drops_scheme_and_path() {
        assert_eq!(
            advertised_authority("https://rig.example:11115"),
            "rig.example:11115"
        );
        assert_eq!(advertised_authority("http://rig.example"), "rig.example");
        assert_eq!(
            advertised_authority("https://rig.example:11115/rp"),
            "rig.example:11115"
        );
        // A bracketed IPv6 literal survives intact — rmcp trims the
        // brackets on both the allowlist entry and the Host header.
        assert_eq!(
            advertised_authority("https://[2001:db8::1]:11115"),
            "[2001:db8::1]:11115"
        );
    }

    #[test]
    fn an_explicit_bind_enumerates_no_interfaces() {
        assert_eq!(
            interface_addrs(ip("127.0.0.1")),
            Vec::<std::net::IpAddr>::new()
        );
        assert_eq!(
            interface_addrs(ip("192.168.1.10")),
            Vec::<std::net::IpAddr>::new()
        );
    }

    #[test]
    fn a_wildcard_bind_enumerates_only_non_loopback_interfaces() {
        // Whatever this host has, loopback is never in the list: rmcp's
        // defaults already cover it, and a duplicate would be noise.
        for addr in interface_addrs(ip("0.0.0.0")) {
            assert!(!addr.is_loopback(), "loopback leaked into the list: {addr}");
        }
    }

    #[test]
    fn configured_advertised_url_wins_over_derivation() {
        let url = advertised_base_url(
            Some(&advertised("https://observatory.example:443")),
            "http",
            addr("0.0.0.0:11115"),
            Some("rig"),
        );
        assert_eq!(url, "https://observatory.example:443");
    }

    /// `ServerBuilder::build` aborts loud when the plate-solver client
    /// can't be built — here, an invalid `ca_cert` path makes
    /// `client_builder` fail. Pins the "`plate_solver`: failed to build
    /// HTTP client" error path (issue #609's `ca_cert_path` plumbing).
    #[tokio::test]
    async fn build_fails_loud_when_plate_solver_client_cannot_be_built() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "session": {"data_directory": dir.path().join("data").to_string_lossy()},
            "equipment": {},
            "plate_solver": {"url": "http://127.0.0.1:11131"},
            "server": {"port": 0}
        }))
        .unwrap();
        config.ca_cert = Some("/nonexistent/ca.pem".to_string());

        // `BoundServer` isn't `Debug`, so `unwrap_err()` doesn't apply.
        let err = ServerBuilder::new()
            .with_config(config)
            .with_config_path(dir.path().join("config.json"))
            .build()
            .await
            .err()
            .expect("build must fail when the client can't be constructed");
        assert!(
            err.to_string()
                .contains("plate_solver: failed to build HTTP client"),
            "{err}"
        );
    }

    /// Same as above for the guider client, configured at
    /// `equipment.mount.guiding`.
    #[tokio::test]
    async fn build_fails_loud_when_guider_client_cannot_be_built() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: Config = serde_json::from_value(serde_json::json!({
            "session": {"data_directory": dir.path().join("data").to_string_lossy()},
            "equipment": {
                "mount": {
                    "alpaca_url": "http://127.0.0.1:11122",
                    "guiding": {"url": "http://127.0.0.1:11130"}
                }
            },
            "server": {"port": 0}
        }))
        .unwrap();
        config.ca_cert = Some("/nonexistent/ca.pem".to_string());

        // `BoundServer` isn't `Debug`, so `unwrap_err()` doesn't apply.
        let err = ServerBuilder::new()
            .with_config(config)
            .with_config_path(dir.path().join("config.json"))
            .build()
            .await
            .err()
            .expect("build must fail when the client can't be constructed");
        assert!(
            err.to_string()
                .contains("guider: failed to build HTTP client"),
            "{err}"
        );
    }

    /// Same as above for the orchestrator plugin's `/invoke` client
    /// (issue #800): a registration whose credential is half-written
    /// aborts startup instead of surfacing as a 401 on the first session
    /// start of the night.
    #[tokio::test]
    async fn build_fails_loud_when_the_orchestrator_registration_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "session": {"data_directory": dir.path().join("data").to_string_lossy()},
            "equipment": {},
            "plugins": [{
                "name": "calibrator-flats",
                "type": "orchestrator",
                "invoke_url": "http://127.0.0.1:11170/invoke",
                "auth": {"username": "observatory"}
            }],
            "server": {"port": 0}
        }))
        .unwrap();

        // `BoundServer` isn't `Debug`, so `unwrap_err()` doesn't apply.
        let err = ServerBuilder::new()
            .with_config(config)
            .with_config_path(dir.path().join("config.json"))
            .build()
            .await
            .err()
            .expect("build must fail when the registration can't be read");
        assert!(
            err.to_string()
                .contains("plugins.0.auth (calibrator-flats)"),
            "{err}"
        );
    }
}
