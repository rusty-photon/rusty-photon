//! Configuration types and JSON loader.
//!
//! Top-level [`Config`] is built by [`load_config`] from a JSON file.
//! Each domain-specific block lives in a sibling module:
//! [`session`], [`site`], [`equipment`] (plus the per-device-type
//! configs [`camera`], [`focuser`], [`mount`], [`filter_wheel`],
//! [`cover_calibrator`], the [`optical_train`] light-path lists, and
//! the mount-scoped [`guiding`] service block), [`imaging`],
//! [`plate_solver`], [`server`].
//! The submodules' public types are re-exported here so existing
//! `crate::config::CameraConfig` callsites keep working unchanged.

pub mod camera;
pub mod centering;
pub mod cooling;
pub mod cover_calibrator;
pub mod dome;
pub mod equipment;
pub mod filter_wheel;
pub mod focuser;
pub mod guiding;
pub mod imaging;
pub mod mount;
pub mod naming_template;
pub mod observing_conditions;
pub mod optical_train;
pub mod plate_solver;
pub mod rotator;
pub mod safety;
pub mod safety_monitor;
pub mod server;
pub mod session;
pub mod site;
pub mod switch;
pub mod target_store;

pub use camera::CameraConfig;
pub use centering::CenteringConfig;
pub use cooling::CoolingConfig;
pub use cover_calibrator::CoverCalibratorConfig;
pub use dome::DomeConfig;
pub use equipment::EquipmentConfig;
pub use filter_wheel::FilterWheelConfig;
pub use focuser::FocuserConfig;
pub use guiding::{FocusWatchConfig, GuiderDefaults, GuidingConfig};
pub use imaging::ImagingConfig;
pub use mount::MountConfig;
pub use observing_conditions::ObservingConditionsConfig;
pub use optical_train::{
    FocalLengthMm, OpticalTrainConfig, PositionAngleDegrees, TrainAutoFocusConfig, TrainPurpose,
};
pub use plate_solver::PlateSolverConfig;
pub use rotator::RotatorConfig;
pub use safety::SafetyConfig;
pub use safety_monitor::SafetyMonitorConfig;
pub use server::{AdvertisedUrl, ServerConfig};
pub use session::SessionConfig;
pub use site::SiteConfig;
pub use switch::SwitchConfig;
pub use target_store::{TargetStoreConfig, TargetStoreConfigWire};

use std::path::Path;

use rusty_photon_config::actions::FieldError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RpError};

/// `deny_unknown_fields` so typoed or removed top-level keys fail loudly at
/// load instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub session: SessionConfig,
    pub equipment: EquipmentConfig,
    /// Observer site (lat/lon). Required for ephemeris features
    /// (`compute_alt_az`, `get_target_status`, etc.); optional otherwise.
    /// When `Some` and a mount is configured, `rp` validates the
    /// configured lat/lon against the mount's `SiteLatitude` /
    /// `SiteLongitude` on connect — see `docs/services/rp.md`
    /// §"Site Validation Against the ASCOM Mount".
    #[serde(default)]
    pub site: Option<SiteConfig>,
    #[serde(default)]
    pub plugins: Vec<Value>,
    /// Target-store settings (`db_path`, `default_goals`,
    /// `default_scheduling`). Targets themselves live in the redb store
    /// (added via `add_target`), not in config — the legacy `targets[]`
    /// planner array was retired. A stray `targets` key or a leftover
    /// array shape here fails loudly at load (`deny_unknown_fields` +
    /// the typed field).
    #[serde(default)]
    pub target_store: TargetStoreConfigWire,
    #[serde(default)]
    pub planner: Value,
    /// Safety-enforcement knobs (rp.md § Safety); the monitors
    /// themselves live under `equipment.safety_monitors`.
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub imaging: ImagingConfig,
    /// Per-rig estimates that size the advisory `center_on_target`
    /// deadline carried on `centering_started` (§2.5). Always present;
    /// an omitted block uses [`CenteringConfig`]'s defaults.
    #[serde(default)]
    pub centering: CenteringConfig,
    /// Camera-cooling controller tuning (rp.md § Camera Cooling).
    /// Always present; an omitted block uses [`CoolingConfig`]'s
    /// defaults. The per-camera setpoint ladders live under
    /// `equipment.cameras[].cooler_targets_c`.
    #[serde(default)]
    pub cooling: CoolingConfig,
    /// Optional plate-solver service. When `None`, the `plate_solve`
    /// MCP tool returns `plate solver not configured`. Mirrors the
    /// `Option<MountConfig>` pattern — the service is optional
    /// infrastructure, not part of the equipment surface.
    #[serde(default)]
    pub plate_solver: Option<PlateSolverConfig>,
    #[serde(default = "server::default_server")]
    pub server: ServerConfig,
    /// PEM CA certificate `rp` trusts for every outbound HTTPS connection
    /// it makes as a client: Alpaca devices (`equipment.*[].alpaca_url`),
    /// the plate-solver service, and the guider service. An observatory
    /// runs one CA (`rusty_photon_tls`), so this is a single rp-level
    /// setting rather than per-target — matching the `ca_cert` field
    /// doctor already wires into sentinel, session-runner, and
    /// calibrator-flats (`services/doctor/src/provision/mod.rs`
    /// `CLIENT_WIRING_SERVICES`). `Some` becomes the client's **only**
    /// trusted root (`tls_certs_only`, ADR-002) — it replaces, not adds
    /// to, the platform trust store, so a public-CA `https://` target
    /// becomes unreachable alongside the observatory CA. `None` (the
    /// default) uses the platform trust store, so an https target signed
    /// by the observatory's self-signed CA fails certificate verification.
    #[serde(default)]
    pub ca_cert: Option<String>,
}

impl Config {
    /// [`Config::ca_cert`] as a `Path`, for `rusty_photon_tls::client`.
    pub fn ca_cert_path(&self) -> Option<&Path> {
        self.ca_cert.as_deref().map(Path::new)
    }
}

/// Minimal runnable scaffold `rp` writes on first start when no config
/// exists at the platform default path: no equipment, default server,
/// session data under a platform-dependent directory — the packaged unit's
/// `StateDirectory` (`/var/lib/rusty-photon/rp/`) on Linux,
/// `~/Library/Application Support/rusty-photon/rp/` on macOS,
/// `%PROGRAMDATA%\rusty-photon\rp\` on Windows (ADR-015). Must stay
/// deserializable into [`Config`] — the packaged first-start contract
/// depends on it.
#[must_use]
pub fn default_scaffold() -> serde_json::Value {
    serde_json::json!({
        "session": { "data_directory": default_data_directory() },
        "equipment": {},
        "server": { "port": 11115, "bind_address": "0.0.0.0" }
    })
}

/// The Linux state path, provisioned and owned by the packaged unit's
/// systemd `StateDirectory=`; also the macOS last-resort fallback.
#[cfg(any(not(windows), test))]
const LINUX_STATE_DATA_DIR: &str = "/var/lib/rusty-photon/rp/data";

/// The scaffold's platform-dependent `session.data_directory` default.
///
/// The startup target-store open creates this directory (rp.md § Target
/// Store), so the default has to be writable by whatever account the
/// packaged service runs as. Linux gets that from systemd `StateDirectory=`
/// and Windows from `LocalSystem`'s access to `%PROGRAMDATA%`; macOS has no
/// equivalent — `brew services` runs as the invoking user, who cannot write
/// `/var/lib` — so macOS puts session data beside the config, mirroring the
/// Windows layout under its own platform root.
#[cfg(not(any(windows, target_os = "macos")))]
fn default_data_directory() -> String {
    LINUX_STATE_DATA_DIR.to_string()
}
#[cfg(target_os = "macos")]
fn default_data_directory() -> String {
    macos_data_directory(rusty_photon_config::default_config_dir().ok())
}
#[cfg(windows)]
fn default_data_directory() -> String {
    program_data_root(std::env::var_os("ProgramData"))
        .join("rusty-photon")
        .join("rp")
        .join("data")
        .to_string_lossy()
        .into_owned()
}

/// Pure resolution of the macOS `session.data_directory` default from the
/// resolved platform config directory (`~/Library/Application Support/
/// rusty-photon`, what `rusty-photon-config` puts `rp.json` in): session
/// data lands in `rp/data` beneath it. Falls back to
/// [`LINUX_STATE_DATA_DIR`] when no home directory resolves at all — a
/// machine with no home is no better served by either path, and that one
/// is at least documented. Parameterized over the resolved directory, and
/// compiled on macOS and in test builds on every platform, so the logic is
/// unit-testable on non-macOS hosts.
#[cfg(any(target_os = "macos", test))]
fn macos_data_directory(config_dir: Option<std::path::PathBuf>) -> String {
    match config_dir {
        Some(dir) => dir.join("rp").join("data").to_string_lossy().into_owned(),
        None => LINUX_STATE_DATA_DIR.to_string(),
    }
}

/// Pure resolution of the Windows `ProgramData` root from the value of the
/// `ProgramData` environment variable: the value verbatim when present and
/// non-empty, else the fixed `C:\ProgramData` fallback. A private copy of the
/// same rule `rusty-photon-config` applies to the config path (each crate
/// keeps its own — see the W2 note in `docs/plans/windows-packaging.md`);
/// compiled on Windows and in test builds on every platform, so the logic
/// is unit-testable on non-Windows hosts.
#[cfg(any(windows, test))]
fn program_data_root(program_data: Option<std::ffi::OsString>) -> std::path::PathBuf {
    match program_data {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::path::PathBuf::from(r"C:\ProgramData"),
    }
}

/// Domain validation shared by startup ([`load_config`]) and the REST
/// `PUT /api/config` endpoint (via [`crate::config_actions::RpConfigDriver`]).
/// Empty result means valid. Paths are dotted with array indices
/// (`equipment.cameras.0.focal_length_mm`) so a UI can render each error
/// next to its field; messages name the device id where one exists.
#[must_use]
pub fn validate_config(config: &Config) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(site) = config.site.as_ref() {
        errors.extend(site.field_errors());
    }
    if let Some(pattern) = config.session.file_naming_pattern.as_deref() {
        if let Err(msg) = naming_template::validate_pattern(pattern) {
            errors.push(FieldError {
                path: "session.file_naming_pattern".to_string(),
                msg,
            });
        }
    }
    if let Some(pattern) = config.session.directory_pattern.as_deref() {
        if let Err(msg) = naming_template::validate_directory_pattern(pattern) {
            errors.push(FieldError {
                path: "session.directory_pattern".to_string(),
                msg,
            });
        }
    }
    // Grading thresholds only ever apply to frames the progress scan can
    // find, and the scan needs the naming templates to find any (rp.md §
    // Progress derivation). Configured without them, `default_grading`
    // would silently judge nothing — a misconfiguration worth failing on
    // rather than a working setup.
    if config.target_store.default_grading.is_some() && config.session.file_naming_pattern.is_none()
    {
        errors.push(FieldError {
            path: "target_store.default_grading".to_string(),
            msg: "grading thresholds need session.file_naming_pattern configured — \
                  without it capture writes flat filenames the progress scan cannot \
                  attribute to a target, so no frame would ever be graded"
                .to_string(),
        });
    }
    for (index, cam) in config.equipment.cameras.iter().enumerate() {
        errors.extend(cam.field_errors(index));
    }
    // The optical-train graph rules (roster existence, terminal camera,
    // order consistency, the one-guiding-train rule) live with the
    // derived model so validation and derivation cannot drift apart.
    if let Err(train_errors) =
        crate::equipment::trains::TrainModel::try_from_equipment(&config.equipment)
    {
        errors.extend(train_errors);
    }
    errors.extend(plugin_registration_errors(&config.plugins));
    errors
}

/// The startup warning to emit when progress derivation is inert, or
/// `None` when it is live.
///
/// Progress is derived by scanning the frames `capture` wrote (rp.md §
/// Progress derivation), and only a configured `session.file_naming_pattern`
/// puts a target's identity into the path. Without it every capture still
/// succeeds and still records what it is — it just lands in a flat
/// `<uuid8>.fits` the scan cannot attribute, so goals silently never
/// advance. That is a config-time mistake with a night-time symptom, so it
/// is worth saying out loud at startup even though it is not fatal:
/// unlike `target_store.default_grading` (which cannot mean anything
/// without a pattern and so fails the load), a rig may legitimately image
/// without goals.
#[must_use]
pub fn progress_derivation_warning(session: &session::SessionConfig) -> Option<&'static str> {
    session.file_naming_pattern.is_none().then_some(
        "session.file_naming_pattern is not configured: capture writes flat \
         <uuid8>.fits filenames, so the progress scan cannot attribute frames to a \
         target. Every target's progress stays 0/0 and goal-terminated sessions \
         never end on goals — they run to max_frames or dawn. See \
         docs/services/rp.md § Progress derivation.",
    )
}

/// A plugin registration stays an opaque `Value` — it is a plugin-author
/// surface, so unknown keys are legal there in a way they are nowhere else
/// in this config. The registrations rp itself *dials* are the exception,
/// because on those rp reads two fields: the callback URL it POSTs to —
/// the orchestrator's `invoke_url` (a session start) or an event plugin's
/// `webhook_url` (an emitted event) — and `auth`, the credential it
/// presents there (rp.md § Orchestrator Registration, § Delivery:
/// Webhooks). Both are permanent configuration faults when malformed — a
/// half-written credential would be read as "no credential" and 401 every
/// delivery; a bad URL would fail every attempt — so both fail at load
/// rather than at first use, which means `load_config`,
/// `PUT /api/config`, and `rp doctor` all reject them identically instead
/// of leaving the first session of the night to discover it.
///
/// Scoped to those two types exactly because the surface is otherwise
/// opaque: rp dials no other registration, so a tool-provider plugin
/// carrying its own differently-shaped `auth` key (a bearer token, say)
/// is that author's business and must not fail rp's config load. The same
/// scope decides which registration doctor offers to wire a credential
/// into (`RpView::plugin_targets`).
///
/// How *many* orchestrators are registered is checked here too
/// ([`second_orchestrator_error`]): rp invokes one, and picking it by
/// array position would make a silently ignored registration —
/// or a reordering by any writer that round-trips the config — a
/// legal config.
fn plugin_registration_errors(plugins: &[Value]) -> Vec<FieldError> {
    // Each dialed registration is checked by running the very parse its
    // runtime builds from — `EventSubscription::parse` for the bus's
    // subscribers, `OrchestratorRegistration::parse` for the session
    // manager's invoke target — so a config that loads is a config rp
    // dials. There is no second implementation here to drift from the
    // runtime's.
    let mut errors: Vec<FieldError> = plugins
        .iter()
        .enumerate()
        .filter_map(|(index, plugin)| {
            if is_orchestrator(plugin) {
                OrchestratorRegistration::parse(index, plugin).err()
            } else if is_event_plugin(plugin) {
                EventSubscription::parse(index, plugin).err()
            } else {
                None
            }
        })
        .collect();
    errors.extend(second_orchestrator_error(plugins));
    errors
}

/// One `type: "orchestrator"` registration, parsed into everything an
/// invocation needs: who to POST a session start to, the opaque `config`
/// to pass through, and the credential to present.
///
/// [`Self::parse`] is the single definition of what an orchestrator
/// registration must carry and [`Self::sole`] of how many may be
/// registered. [`plugin_registration_errors`] runs both to reject a
/// faulty config at load, and [`crate::session::SessionManager`] runs
/// them to build the registration it invokes, so the config rp accepts
/// and the config rp acts on are the same by construction.
#[derive(Debug)]
pub struct OrchestratorRegistration {
    /// The registration's position in `plugins[]` — the path an operator
    /// edits, and what rp's startup errors name.
    pub index: usize,
    pub name: String,
    pub invoke_url: String,
    /// The registration's `config` object — opaque to rp, passed through
    /// verbatim in the `/invoke` POST.
    pub config: Option<Value>,
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

impl OrchestratorRegistration {
    /// Parse `plugins[index]`, which the caller has already established
    /// is an orchestrator registration ([`is_orchestrator`]).
    ///
    /// `invoke_url` is required. An entry declaring itself the
    /// orchestrator with nothing to POST to is malformed, and read as
    /// inert it degrades into "no orchestrator is configured" — which
    /// reads as intentional, so `POST /api/session/start` would find
    /// nothing to invoke and report no fault, night after night.
    ///
    /// `name` is optional, because rp only labels errors and logs with
    /// it and the index is what identifies the entry; an unnamed
    /// registration is reported as `orchestrator`. Every message carries
    /// it beside the index anyway — nothing stops two registrations
    /// sharing a `name`, but an operator reads the name first.
    pub fn parse(index: usize, entry: &Value) -> std::result::Result<Self, FieldError> {
        let name = registration_name(entry);
        let at = |field: &str, msg: &str| FieldError {
            path: format!("plugins.{index}.{field}"),
            msg: format!("({name}) {msg}"),
        };

        let url_value = entry
            .get(ORCHESTRATOR_URL_FIELD)
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                at(
                    ORCHESTRATOR_URL_FIELD,
                    "is required: an orchestrator registration carries the URL rp POSTs a \
                     session start to",
                )
            })?;
        let invoke_url =
            validate_callback_url(url_value).map_err(|msg| at(ORCHESTRATOR_URL_FIELD, &msg))?;

        let auth = match entry.get("auth") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<rp_auth::config::ClientAuthConfig>(value.clone())
                    .map_err(|e| at("auth", &e.to_string()))?,
            ),
        };

        Ok(Self {
            index,
            name: name.to_string(),
            invoke_url: invoke_url.to_string(),
            config: entry.get("config").cloned(),
            auth,
        })
    }

    /// The one orchestrator registered in `plugins`, or `None` when none
    /// is — the lookup [`crate::session::SessionManager`] builds from.
    ///
    /// More than one is a configuration fault, not a tie to break by
    /// position: `plugins[]` order is not identity, every writer that
    /// round-trips the config (`PUT /api/config`, ui-htmx) may reorder
    /// it, and the entry that loses is invoked never — with no error, no
    /// warning, and an operator watching a session that simply does not
    /// start.
    /// Every orchestrator entry is parsed before the count is judged, and
    /// in `plugins[]` order — the order [`plugin_registration_errors`]
    /// reports in — so the runtime and the load path name the same field
    /// on the same file. It is also the more useful of the two errors: a
    /// stub with no `invoke_url` sitting beside a working registration
    /// has to be named as the malformed entry, or "remove one" points an
    /// operator at whichever of the two the message happens to name.
    pub fn sole(plugins: &[Value]) -> std::result::Result<Option<Self>, FieldError> {
        let registered = plugins
            .iter()
            .enumerate()
            .filter(|(_, plugin)| is_orchestrator(plugin))
            .map(|(index, entry)| Self::parse(index, entry))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(error) = second_orchestrator_error(plugins) {
            return Err(error);
        }
        Ok(registered.into_iter().next())
    }
}

/// The error for a `plugins[]` that registers a second orchestrator, or
/// `None` when at most one is registered. Names the later entry, since
/// removing it restores the behaviour the config already had — rp
/// invokes the first — and names the earlier one in the message, because
/// which of the two is live is the whole question.
fn second_orchestrator_error(plugins: &[Value]) -> Option<FieldError> {
    let mut registered = plugins
        .iter()
        .enumerate()
        .filter(|(_, plugin)| is_orchestrator(plugin));
    let (first_index, first) = registered.next()?;
    let (second_index, second) = registered.next()?;
    Some(FieldError {
        path: format!("plugins.{second_index}.type"),
        msg: format!(
            "({}) is a second orchestrator registration, and rp invokes exactly one: \
             plugins.{first_index} ({}) comes first, so this one would never be invoked — \
             no error, no warning, no work. Remove one.",
            registration_name(second),
            registration_name(first),
        ),
    })
}

/// The name a registration is reported under. Optional on an
/// orchestrator entry (unlike an event subscriber's, which delivery logs
/// by name), so an unnamed one reads as `orchestrator`.
fn registration_name(entry: &Value) -> &str {
    entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("orchestrator")
}

/// One `type: "event"` registration, parsed into everything a delivery
/// needs: who to POST to, which events, and the credential to present.
///
/// [`Self::parse`] is the single implementation of what an event
/// registration must carry. [`plugin_registration_errors`] runs it to
/// reject a faulty registration at config load, and
/// [`crate::events::EventBus`] runs it to build the subscribers it
/// delivers to, so the config rp accepts and the config rp acts on are
/// the same set by construction.
#[derive(Debug)]
pub struct EventSubscription {
    pub name: String,
    pub webhook_url: String,
    pub subscribes_to: Vec<String>,
    pub auth: Option<rp_auth::config::ClientAuthConfig>,
}

impl EventSubscription {
    /// Parse `plugins[index]`, which the caller has already established is
    /// an event registration ([`is_event_plugin`]).
    ///
    /// Every field is required, and the error names the offending one.
    /// An event registration exists to receive deliveries, so one that can
    /// receive none — no name, no `webhook_url`, a `subscribes_to` that is
    /// absent, empty, or not a list of event-name strings — is a
    /// configuration fault rather than an inert entry. Read as inert, a
    /// `subscribe_to` typo would leave a plugin that looks registered,
    /// logs nothing, and is fed nothing until dawn: delivery is
    /// fire-and-forget, so a subscriber rp never even attempts to reach
    /// produces no failure to notice (rp.md § Delivery: Webhooks).
    ///
    /// `index` is the registration's position in `plugins[]`, because that
    /// is the path an operator can act on — nothing stops two
    /// registrations sharing a `name`.
    pub fn parse(index: usize, entry: &Value) -> std::result::Result<Self, FieldError> {
        let at = |name: &str, msg: &str| FieldError {
            path: format!("plugins.{index}.{name}"),
            msg: msg.to_string(),
        };

        let name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
            at(
                "name",
                "is required and must be a string naming the subscriber",
            )
        })?;

        let url_value = entry
            .get(EVENT_URL_FIELD)
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                at(
                    EVENT_URL_FIELD,
                    "is required: an event registration carries the URL rp POSTs each subscribed event to",
                )
            })?;
        let webhook_url =
            validate_callback_url(url_value).map_err(|msg| at(EVENT_URL_FIELD, &msg))?;

        let subscribed = entry
            .get("subscribes_to")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                at(
                    "subscribes_to",
                    "is required and must be an array of the event names rp should deliver",
                )
            })?;
        let subscribes_to = subscribed
            .iter()
            .enumerate()
            .map(|(position, v)| {
                // Names the position rather than echoing the entry: an
                // error message is rendered by the UI and by `rp doctor`,
                // and the position is what an operator edits anyway.
                v.as_str().map(String::from).ok_or_else(|| {
                    at(
                        "subscribes_to",
                        &format!("entry {position} must be an event-name string"),
                    )
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if subscribes_to.is_empty() {
            return Err(at(
                "subscribes_to",
                "must name at least one event; a registration subscribed to nothing is never \
                 delivered to, so remove it instead",
            ));
        }

        let auth = match entry.get("auth") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<rp_auth::config::ClientAuthConfig>(value.clone())
                    .map_err(|e| at("auth", &e.to_string()))?,
            ),
        };

        Ok(Self {
            name: name.to_string(),
            webhook_url: webhook_url.to_string(),
            subscribes_to,
            auth,
        })
    }
}

/// The registration field naming the endpoint rp POSTs a session start
/// to (rp.md § Orchestrator Registration). Read by
/// [`OrchestratorRegistration::parse`], which both
/// [`crate::session::SessionManager`]'s registration lookup and
/// [`plugin_registration_errors`] run, so the two cannot drift.
pub const ORCHESTRATOR_URL_FIELD: &str = "invoke_url";

/// The registration field naming the endpoint rp POSTs each subscribed
/// event to (rp.md § Delivery: Webhooks). Read by
/// [`crate::events::EventBus`]'s registration lookup and validated by
/// [`plugin_registration_errors`] through this one name.
pub const EVENT_URL_FIELD: &str = "webhook_url";

/// A callback URL must be an `http://` or `https://` URL rp can actually
/// POST to. Rejected at load for the same reason `server.advertised_url`
/// is: a bad scheme or a non-URL is a permanent configuration fault, and
/// left to first use it would only surface as a failed delivery in the
/// middle of the night. Mirrors [`server::AdvertisedUrl`]'s rule rather
/// than inventing a second one, plus one rule of its own — see the
/// userinfo branch.
///
/// Returns the accepted URL so a caller that needs the string — the
/// [`EventSubscription`] parse — takes it from the check rather than
/// re-reading the `Value` and having to handle a "not a string" case
/// this function has already ruled out.
fn validate_callback_url(value: &Value) -> std::result::Result<&str, String> {
    let Some(url) = value.as_str() else {
        return Err(format!("must be a string, got {value}"));
    };
    // Every branch that echoes the URL back redacts it first: a
    // `FieldError` is rendered by the UI and by `rp doctor`, and these
    // two run *before* the userinfo check below — on inputs that may not
    // even parse — so a malformed credential-bearing URL would otherwise
    // leak its password on the way to being rejected.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!(
            "must be an http:// or https:// URL, got {:?}",
            redact_userinfo(url)
        ));
    }
    // The scheme is already known to be http(s), for which the URL
    // grammar requires a host — so a successful parse here is a URL rp
    // can post to, and the host-less case arrives as a parse error
    // ("empty host") rather than needing a branch of its own.
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| format!("is not a valid URL ({e}): {:?}", redact_userinfo(url)))?;
    // Embedded credentials are rejected rather than honored: rp logs this
    // URL on every delivery attempt, so a password in it is a password in
    // the night's logs, and the sibling `auth` block — which rp applies
    // per-request, marked sensitive — would silently win over it anyway.
    // This message omits the URL entirely rather than redacting it: there
    // is nothing left to point at that the field path does not say.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "must not embed credentials in the URL; put them in the sibling `auth` block"
                .to_string(),
        );
    }
    Ok(url)
}

/// `url` with any embedded userinfo replaced by `***`, for the messages
/// that echo a rejected URL back to an operator.
///
/// Deliberately textual rather than going through [`reqwest::Url`]: the
/// callers run on input that has already failed a scheme check or failed
/// to parse at all, so there is no parsed URL to read a username off.
/// Scans only the authority — up to the first `/`, `?`, or `#` — so a
/// later `@` in a path or query is left alone.
fn redact_userinfo(url: &str) -> std::borrow::Cow<'_, str> {
    let authority_start = url.find("://").map_or(0, |i| i.saturating_add(3));
    let Some(rest) = url.get(authority_start..) else {
        return std::borrow::Cow::Borrowed(url);
    };
    let authority = rest
        .find(['/', '?', '#'])
        .and_then(|i| rest.get(..i))
        .unwrap_or(rest);
    // The last `@` in the authority is the delimiter; an earlier one
    // would be inside the userinfo itself.
    let Some(at) = authority.rfind('@') else {
        return std::borrow::Cow::Borrowed(url);
    };
    // Everything after the `@`, including the path and query — only the
    // userinfo is dropped.
    match (
        url.get(..authority_start),
        url.get(authority_start.saturating_add(at).saturating_add(1)..),
    ) {
        (Some(prefix), Some(after)) => std::borrow::Cow::Owned(format!("{prefix}***@{after}")),
        _ => std::borrow::Cow::Borrowed(url),
    }
}

/// Whether a plugin registration is the orchestrator kind — the single
/// place that rule is written, shared by [`plugin_registration_errors`]
/// and [`OrchestratorRegistration::sole`], the lookup
/// [`crate::session::SessionManager`] builds from.
pub fn is_orchestrator(plugin: &Value) -> bool {
    plugin.get("type").and_then(Value::as_str) == Some("orchestrator")
}

/// Whether a plugin registration is the event-webhook kind — the single
/// place that rule is written, shared by [`plugin_registration_errors`]
/// and [`crate::events::EventBus`]'s registration lookup.
pub fn is_event_plugin(plugin: &Value) -> bool {
    plugin.get("type").and_then(Value::as_str) == Some("event")
}

pub fn load_config(path: &Path) -> Result<Config> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        RpError::Config(format!(
            "failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    let config: Config = serde_json::from_str(&contents).map_err(|e| {
        RpError::Config(format!(
            "failed to parse config file '{}': {}",
            path.display(),
            e
        ))
    })?;
    // Same field validation as `PUT /api/config`; startup keeps its
    // pre-REST behaviour of aborting on the first offending field.
    if let Some(err) = validate_config(&config).into_iter().next() {
        return Err(RpError::Config(format!("{} {}", err.path, err.msg)));
    }
    Ok(config)
}

#[cfg(test)]
pub(crate) mod test_support {
    pub const MINIMAL_CONFIG_JSON: &str = r#"{
        "session": {"data_directory": "/tmp/rp-test"},
        "equipment": {}
    }"#;
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::test_support::MINIMAL_CONFIG_JSON;
    use super::*;

    #[test]
    fn default_scaffold_deserializes_into_config() {
        let config: Config = serde_json::from_value(default_scaffold()).unwrap();
        #[cfg(not(any(windows, target_os = "macos")))]
        assert_eq!(config.session.data_directory, LINUX_STATE_DATA_DIR);
        // Under the config root when a home resolves, else the fallback —
        // both end the same way, so this holds without a home on the runner.
        #[cfg(target_os = "macos")]
        assert!(
            config
                .session
                .data_directory
                .ends_with("/rusty-photon/rp/data"),
            "{}",
            config.session.data_directory
        );
        #[cfg(windows)]
        assert!(
            config
                .session
                .data_directory
                .ends_with(r"\rusty-photon\rp\data"),
            "{}",
            config.session.data_directory
        );
        assert!(config.equipment.cameras.is_empty());
        assert!(config.site.is_none());
        assert_eq!(config.server.port, 11115);
    }

    #[test]
    fn macos_data_directory_sits_under_the_config_root() {
        let root = "/Users/astro/Library/Application Support/rusty-photon";
        let dir = macos_data_directory(Some(std::path::PathBuf::from(root)));
        // Compared as `Path`s, not strings: `PathBuf::join` emits the *host's*
        // separator, and this macOS-only path is also built on the Windows and
        // Linux hosts that run the test. `Path` equality is component-wise, so
        // this still pins the tail exactly.
        let relative = std::path::Path::new(&dir)
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("{dir} is not under {root}"));
        assert_eq!(relative, std::path::Path::new("rp/data"));
    }

    #[test]
    fn macos_data_directory_falls_back_without_a_home() {
        assert_eq!(macos_data_directory(None), LINUX_STATE_DATA_DIR);
    }

    #[test]
    fn program_data_root_uses_env_value_verbatim() {
        let root = program_data_root(Some(std::ffi::OsString::from(r"D:\CustomData")));
        assert_eq!(root, std::path::PathBuf::from(r"D:\CustomData"));
    }

    #[test]
    fn program_data_root_falls_back_when_env_absent() {
        assert_eq!(
            program_data_root(None),
            std::path::PathBuf::from(r"C:\ProgramData")
        );
    }

    #[test]
    fn program_data_root_falls_back_when_env_empty() {
        assert_eq!(
            program_data_root(Some(std::ffi::OsString::new())),
            std::path::PathBuf::from(r"C:\ProgramData")
        );
    }

    #[test]
    fn load_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.session.data_directory, "/tmp/rp-test");
        assert_eq!(config.server.port, 11115);
        assert_eq!(config.server.bind_address.to_string(), "0.0.0.0");
        assert_eq!(config.imaging.cache_max_mib, 1024);
        assert_eq!(config.imaging.cache_max_images, 8);
    }

    #[test]
    fn ca_cert_omitted_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, MINIMAL_CONFIG_JSON).unwrap();

        let config = load_config(&path).unwrap();
        assert!(config.ca_cert.is_none());
        assert!(config.ca_cert_path().is_none());
    }

    #[test]
    fn ca_cert_path_reflects_the_configured_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "ca_cert": "/etc/rusty-photon/pki/ca.pem"
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.ca_cert_path(),
            Some(Path::new("/etc/rusty-photon/pki/ca.pem"))
        );
    }

    #[test]
    fn load_config_missing_file() {
        let err = load_config(Path::new("/nonexistent/rp/config.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read config file"));
    }

    #[test]
    fn load_config_rejects_unknown_top_level_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "server": { "port": 0 },
                "workflows": []
            }"#,
        )
        .unwrap();

        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("workflows"), "{err}");
    }

    #[test]
    fn load_config_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "not valid json").unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config file"));
    }

    #[test]
    fn validate_config_flags_site_camera_and_train_fields_with_dotted_paths() {
        let mut config: Config = serde_json::from_value(default_scaffold()).unwrap();
        config.site = Some(crate::config::SiteConfig {
            latitude_degrees: 91.0,
            longitude_degrees: 181.0,
        });
        config.equipment.cameras = vec![
            serde_json::from_value(serde_json::json!({
                "id": "bad-cam",
                "alpaca_url": "http://localhost:11120",
                "cooler_targets_c": [-12]
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": "good-cam",
                "alpaca_url": "http://localhost:11121"
            }))
            .unwrap(),
        ];
        config.equipment.optical_trains = vec![serde_json::from_value(serde_json::json!({
            "id": "main",
            "devices": ["ghost-focuser", "good-cam"]
        }))
        .unwrap()];

        let errors = validate_config(&config);
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "site.latitude_degrees",
                "site.longitude_degrees",
                "equipment.cameras.0.cooler_targets_c",
                "equipment.optical_trains.0.devices.0",
            ]
        );
        assert!(
            errors[2].msg.contains("bad-cam"),
            "camera errors name the device id for humans: {:?}",
            errors[2]
        );
        assert!(
            errors[3].msg.contains("ghost-focuser"),
            "train errors name the offending device id: {:?}",
            errors[3]
        );
    }

    #[test]
    fn validate_config_accepts_scaffold() {
        let config: Config = serde_json::from_value(default_scaffold()).unwrap();
        assert_eq!(validate_config(&config), vec![]);
    }

    #[test]
    fn grading_thresholds_without_a_naming_pattern_are_rejected() {
        // Without `file_naming_pattern` the progress scan has nothing to
        // attribute frames with, so the thresholds would judge nothing
        // (rp.md § Progress derivation).
        let mut scaffold = default_scaffold();
        scaffold["target_store"] = serde_json::json!({
            "default_grading": { "max_hfr_pixels": 3.0 }
        });
        let config: Config = serde_json::from_value(scaffold).unwrap();
        let errors = validate_config(&config);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "target_store.default_grading");
        assert!(errors[0].msg.contains("file_naming_pattern"), "{errors:?}");
    }

    #[test]
    fn grading_thresholds_are_accepted_alongside_a_naming_pattern() {
        let mut scaffold = default_scaffold();
        scaffold["session"]["file_naming_pattern"] = serde_json::json!(
            "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_{uuid8}"
        );
        scaffold["target_store"] = serde_json::json!({
            "default_grading": { "max_hfr_pixels": 3.0 }
        });
        let config: Config = serde_json::from_value(scaffold).unwrap();
        assert_eq!(validate_config(&config), vec![]);
    }

    /// A rig may legitimately image without goals, so a missing naming
    /// pattern warns rather than failing the load — but it must warn,
    /// because the symptom (goals that never advance) otherwise only
    /// shows up mid-night.
    #[test]
    fn a_missing_naming_pattern_warns_that_progress_derivation_is_inert() {
        let config: Config = serde_json::from_value(default_scaffold()).unwrap();
        assert!(config.session.file_naming_pattern.is_none());

        let warning = progress_derivation_warning(&config.session)
            .expect("no naming pattern must produce a startup warning");
        assert!(warning.contains("file_naming_pattern"), "{warning}");
        assert!(warning.contains("progress"), "{warning}");
    }

    #[test]
    fn a_configured_naming_pattern_warns_about_nothing() {
        let mut scaffold = default_scaffold();
        scaffold["session"]["file_naming_pattern"] = serde_json::json!(
            "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_{uuid8}"
        );
        let config: Config = serde_json::from_value(scaffold).unwrap();
        assert_eq!(progress_derivation_warning(&config.session), None);
    }

    /// Serialize → deserialize → serialize must be a fixed point: the REST
    /// `PUT /api/config` path re-parses the serialized value it persists
    /// (`rusty_photon_config::actions::config_apply`), so any asymmetric
    /// field would corrupt the persisted config.
    fn assert_value_round_trips(config: &Config) -> serde_json::Value {
        let value = serde_json::to_value(config).unwrap();
        let back: Config = serde_json::from_value(value.clone()).unwrap();
        let again = serde_json::to_value(&back).unwrap();
        assert_eq!(again, value, "Config JSON round-trip must be stable");
        value
    }

    #[test]
    fn config_json_round_trips_default_scaffold() {
        let config: Config = serde_json::from_value(default_scaffold()).unwrap();
        assert_value_round_trips(&config);
    }

    #[test]
    fn config_json_round_trips_fully_populated_sample() {
        // Every block populated, including all humantime-serde Duration
        // fields (which serialize as humantime strings) and both secret
        // shapes (server auth hash + per-device client auth password).
        let sample = serde_json::json!({
            "session": {
                "data_directory": "/data/lights",
                "session_state_file": "/data/session_state.json",
                "file_naming_pattern": "{target}_{filter}"
            },
            "site": { "latitude_degrees": 47.6062, "longitude_degrees": -122.3321 },
            "equipment": {
                "cameras": [{
                    "id": "main-cam",
                    "name": "Main",
                    "alpaca_url": "https://localhost:11120",
                    "device_type": "camera",
                    "device_number": 0,
                    "cooler_targets_c": [-10, 5],
                    "gain": 100,
                    "offset": 50,
                    "readout_time_estimate": "8s",
                    "auth": { "username": "observatory", "password": "secret" }
                }],
                "optical_trains": [{
                    "id": "main",
                    "purpose": "imaging",
                    "focal_length_mm": 1000.0,
                    "devices": ["main-focuser", "main-fw", "main-cam"]
                }],
                "mount": {
                    "alpaca_url": "http://localhost:11122",
                    "device_number": 0,
                    "settle_after_slew": "3s",
                    "slew_rate_arcsec_per_sec": 7200.0,
                    "guiding": {
                        "url": "http://localhost:11130",
                        "timeout": "90s",
                        "settle_pixels": 0.8,
                        "settle_time": "10s",
                        "settle_timeout": "1m",
                        "dither_pixels": 5.0,
                        "recalibrate_above_deg": 5.0,
                        "auth": { "username": "observatory", "password": "secret" }
                    },
                    "auth": { "username": "observatory", "password": "secret" }
                },
                "focusers": [{
                    "id": "main-focuser",
                    "alpaca_url": "http://localhost:11113",
                    "device_number": 0,
                    "min_position": 0,
                    "max_position": 100_000,
                    "steps_per_sec": 1200.0,
                    "auth": { "username": "observatory", "password": "secret" }
                }],
                "filter_wheels": [{
                    "id": "main-fw",
                    "alpaca_url": "http://localhost:11123",
                    "device_number": 0,
                    "filters": ["L", "R", "G", "B"],
                    "auth": { "username": "observatory", "password": "secret" }
                }],
                "cover_calibrators": [{
                    "id": "flat-panel",
                    "alpaca_url": "http://localhost:11125",
                    "device_number": 0,
                    "poll_interval": "3s",
                    "auth": { "username": "observatory", "password": "secret" }
                }],
                "safety_monitors": [{
                    "id": "weather-watcher",
                    "alpaca_url": "http://localhost:11111",
                    "device_number": 0,
                    "auth": { "username": "observatory", "password": "secret" }
                }]
            },
            "plugins": [{
                "name": "image-analyzer",
                "type": "event",
                "webhook_url": "http://127.0.0.1:11140/webhook",
                "subscribes_to": ["exposure_complete"]
            }],
            "target_store": {
                "db_path": "/data/targets.redb",
                "default_goals": [{ "filter": "L", "binning": "1x1", "exposure_duration": "5m", "desired_count": 20 }],
                "default_scheduling": { "min_altitude_degrees": 20.0 }
            },
            "planner": { "min_altitude_degrees": 20 },
            "safety": { "poll_interval": "10s" },
            "imaging": { "cache_max_mib": 1024, "cache_max_images": 8 },
            "centering": { "solve_time_estimate": "30s", "slew_overhead_estimate": "10s" },
            "cooling": {
                "poll_interval": "10s",
                "plateau_window": "2m",
                "plateau_threshold_c": 0.5,
                "tolerance_c": 1.0,
                "max_cooler_power_pct": 90.0,
                "regulation_margin_c": 3.0,
                "max_cooldown": "20m",
                "warmup_step_interval": "2m",
                "warm_target_c": 10.0
            },
            "plate_solver": { "url": "http://localhost:11131", "timeout": "1m", "default_search_radius_deg": 3.0,
                               "auth": { "username": "observatory", "password": "secret" } },
            "server": {
                "port": 11115,
                "bind_address": "127.0.0.1",
                "tls": { "cert": "/etc/pki/rp.pem", "key": "/etc/pki/rp-key.pem" },
                "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$abc" }
            }
        });

        let config: Config = serde_json::from_value(sample).unwrap();
        let value = assert_value_round_trips(&config);

        // humantime-serde fields serialize as humantime strings, not
        // `{secs, nanos}` objects — the wire shape the schema declares.
        for (pointer, expected) in [
            ("/equipment/cameras/0/readout_time_estimate", "8s"),
            ("/equipment/mount/settle_after_slew", "3s"),
            ("/equipment/cover_calibrators/0/poll_interval", "3s"),
            ("/safety/poll_interval", "10s"),
            ("/centering/solve_time_estimate", "30s"),
            ("/centering/slew_overhead_estimate", "10s"),
            ("/cooling/poll_interval", "10s"),
            ("/cooling/plateau_window", "2m"),
            ("/cooling/max_cooldown", "20m"),
            ("/cooling/warmup_step_interval", "2m"),
            ("/plate_solver/timeout", "1m"),
            ("/equipment/mount/guiding/timeout", "1m 30s"),
            ("/equipment/mount/guiding/settle_time", "10s"),
            ("/equipment/mount/guiding/settle_timeout", "1m"),
        ] {
            assert_eq!(
                value.pointer(pointer).and_then(Value::as_str),
                Some(expected),
                "expected humantime string at {pointer}, got {:?}",
                value.pointer(pointer)
            );
        }
    }

    /// A plugin registration is otherwise opaque, but on a registration rp
    /// dials the `auth` block is read as rp's client credential — a
    /// half-written one must fail at load (and so at `PUT /api/config` and
    /// `rp doctor`) rather than be silently read as "no credential" and
    /// 401 every delivery. Both dialed types are pinned: the two paths
    /// parse the same block through different call sites.
    #[test]
    fn a_malformed_plugin_auth_block_fails_to_load() {
        for (plugin_type, url_field, url) in [
            (
                "orchestrator",
                "invoke_url",
                "http://127.0.0.1:11170/invoke",
            ),
            ("event", "webhook_url", "http://127.0.0.1:11140/webhook"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(
                &path,
                format!(
                    r#"{{
                        "session": {{"data_directory": "/tmp/rp-test"}},
                        "equipment": {{}},
                        "plugins": [{{
                            "name": "a-plugin",
                            "type": "{plugin_type}",
                            "{url_field}": "{url}",
                            "subscribes_to": ["exposure_complete"],
                            "auth": {{"username": "observatory"}}
                        }}],
                        "server": {{ "port": 0 }}
                    }}"#
                ),
            )
            .unwrap();

            let error = load_config(&path).unwrap_err().to_string();
            assert!(
                error.contains("plugins.0.auth") && error.contains("password"),
                "unexpected error for {plugin_type}: {error}"
            );
        }
    }

    /// Validation runs the same [`EventSubscription::parse`] the bus
    /// builds subscribers with, so an undeliverable registration is
    /// refused by `PUT /api/config` and `rp doctor` exactly as it is by
    /// startup. What the parse rejects is pinned case-by-case where it
    /// runs (`events::tests::an_undeliverable_registration_fails_startup`);
    /// what this pins is that the load path runs it at all — a
    /// `subscribe_to` typo must not reach a running rp, where it would
    /// leave a plugin that looks registered and is fed nothing until dawn.
    #[test]
    fn an_undeliverable_event_registration_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plugins": [{
                    "name": "image-analyzer",
                    "type": "event",
                    "webhook_url": "http://127.0.0.1:11140/webhook",
                    "subscribe_to": ["exposure_complete"],
                }],
                "server": {"port": 0},
            })
            .to_string(),
        )
        .unwrap();

        let error = load_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("plugins.0.subscribes_to"),
            "must name the field to fix: {error}"
        );
    }

    /// An orchestrator entry with nothing to POST to is malformed, not a
    /// rig that runs without an orchestrator: read as inert it degrades
    /// into "no orchestrator is configured", which reads as intentional,
    /// and `POST /api/session/start` then finds nothing to invoke and
    /// reports no fault.
    #[test]
    fn an_orchestrator_registration_without_an_invoke_url_fails_to_load() {
        for entry in [
            serde_json::json!({"name": "session-runner", "type": "orchestrator"}),
            serde_json::json!({
                "name": "session-runner", "type": "orchestrator", "invoke_url": null,
            }),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(
                &path,
                serde_json::json!({
                    "session": {"data_directory": "/tmp/rp-test"},
                    "equipment": {},
                    "plugins": [entry],
                    "server": {"port": 0},
                })
                .to_string(),
            )
            .unwrap();

            let error = load_config(&path).unwrap_err().to_string();
            assert!(
                error.contains("plugins.0.invoke_url") && error.contains("session-runner"),
                "must name the entry to fix: {error}"
            );
        }
    }

    /// rp invokes one orchestrator, and `plugins[]` order is not
    /// identity — so two registrations is a config mistake, refused at
    /// load naming both. Resolving it by position would leave the later
    /// entry fully validated, reported clean, and never invoked.
    #[test]
    fn a_second_orchestrator_registration_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plugins": [
                    {
                        "name": "calibrator-flats",
                        "type": "orchestrator",
                        "invoke_url": "http://127.0.0.1:11170/invoke",
                    },
                    {
                        "name": "session-runner",
                        "type": "orchestrator",
                        "invoke_url": "http://127.0.0.1:11171/invoke",
                    },
                ],
                "server": {"port": 0},
            })
            .to_string(),
        )
        .unwrap();

        let error = load_config(&path).unwrap_err().to_string();
        assert!(
            error.contains("plugins.1.type")
                && error.contains("session-runner")
                && error.contains("plugins.0")
                && error.contains("calibrator-flats"),
            "must name both registrations: {error}"
        );
    }

    /// The two rules together are what take `plugins[]` position out of
    /// the question: a stub without an `invoke_url` sitting ahead of a
    /// complete registration used to make rp report no orchestrator at
    /// all, and moving it behind — a semantically meaningless edit —
    /// used to make sessions work again. Now neither order loads, and
    /// the message names the stub either way.
    #[test]
    fn an_incomplete_orchestrator_does_not_hide_a_complete_one() {
        let stub = serde_json::json!({"name": "stub", "type": "orchestrator"});
        let complete = serde_json::json!({
            "name": "session-runner",
            "type": "orchestrator",
            "invoke_url": "http://127.0.0.1:11171/invoke",
        });
        for plugins in [vec![stub.clone(), complete.clone()], vec![complete, stub]] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(
                &path,
                serde_json::json!({
                    "session": {"data_directory": "/tmp/rp-test"},
                    "equipment": {},
                    "plugins": plugins,
                    "server": {"port": 0},
                })
                .to_string(),
            )
            .unwrap();

            let error = load_config(&path).unwrap_err().to_string();
            assert!(
                error.contains("invoke_url") && error.contains("stub"),
                "the incomplete registration must be named whichever order it sits in: {error}"
            );
        }
    }

    /// The callback URL is the registration's primary field, so it fails
    /// at load on the same terms as `auth` beside it — a bad scheme is a
    /// permanent fault, and left to first use it would only surface as a
    /// failed delivery in the middle of the night.
    #[test]
    fn a_non_http_callback_url_fails_to_load() {
        for (plugin_type, url_field) in [("orchestrator", "invoke_url"), ("event", "webhook_url")] {
            for (url, expected) in [
                ("ftp://127.0.0.1:11170/invoke", "http://"),
                ("127.0.0.1:11170/invoke", "http://"),
                ("http://", "empty host"),
                // Embedded credentials: rp logs the callback URL on every
                // attempt, and the sibling `auth` block would win anyway.
                (
                    "https://observatory:s3cret@127.0.0.1:11170/invoke",
                    "must not embed credentials",
                ),
                ("https://observatory@127.0.0.1:11170/invoke", "credentials"),
                // A credential-bearing URL that is *also* malformed is
                // rejected by an earlier branch — one that echoes the URL
                // back. The shared secret-free assertion below is the
                // point of these two: rejection must not leak what it
                // rejected.
                ("ftp://observatory:s3cret@127.0.0.1:11170/invoke", "http://"),
                ("https://observatory:s3cret@", "empty host"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("config.json");
                std::fs::write(
                    &path,
                    format!(
                        r#"{{
                            "session": {{"data_directory": "/tmp/rp-test"}},
                            "equipment": {{}},
                            "plugins": [{{
                                "name": "a-plugin",
                                "type": "{plugin_type}",
                                "subscribes_to": ["exposure_complete"],
                                "{url_field}": "{url}"
                            }}],
                            "server": {{ "port": 0 }}
                        }}"#
                    ),
                )
                .unwrap();

                let error = load_config(&path).unwrap_err().to_string();
                assert!(
                    error.contains(&format!("plugins.0.{url_field}")) && error.contains(expected),
                    "unexpected error for {plugin_type} {url:?}: {error}"
                );
                // The whole point of rejecting embedded credentials is to
                // keep them out of logs, so the rejection itself must not
                // echo one back: a `FieldError` is rendered by the UI and
                // by `rp doctor`.
                assert!(
                    !error.contains("s3cret"),
                    "the rejection leaked the embedded password: {error}"
                );
            }
        }
    }

    /// The redaction the rejection messages depend on, pinned directly:
    /// it runs on input that may not parse, so it cannot lean on
    /// `reqwest::Url` and has to get the authority boundaries right
    /// itself.
    #[test]
    fn redact_userinfo_strips_only_the_authoritys_credentials() {
        for (url, expected) in [
            // Nothing to redact — returned untouched.
            (
                "https://127.0.0.1:11170/invoke",
                "https://127.0.0.1:11170/invoke",
            ),
            ("127.0.0.1:11170/invoke", "127.0.0.1:11170/invoke"),
            // User and user:password forms.
            (
                "https://observatory:s3cret@127.0.0.1:11170/invoke",
                "https://***@127.0.0.1:11170/invoke",
            ),
            ("https://observatory@127.0.0.1/x", "https://***@127.0.0.1/x"),
            // Malformed but still credential-bearing: the case the parse
            // branch echoes.
            ("https://observatory:s3cret@", "https://***@"),
            // Scheme-less, so the authority starts at 0.
            ("observatory:s3cret@127.0.0.1/x", "***@127.0.0.1/x"),
            // An `@` past the authority is part of the path or query, not
            // a credential, and must survive.
            (
                "https://127.0.0.1/hook@v2?to=a@b",
                "https://127.0.0.1/hook@v2?to=a@b",
            ),
        ] {
            assert_eq!(redact_userinfo(url), expected, "for {url:?}");
        }
    }

    /// A registration rp never dials keeps its own `invoke_url`-shaped
    /// key, same as its `auth`.
    #[test]
    fn an_undialed_plugins_invoke_url_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plugins": [{
                    "name": "some-tool-provider",
                    "type": "tool_provider",
                    "invoke_url": "ipc:///run/plugin.sock"
                }],
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.plugins[0]["invoke_url"], "ipc:///run/plugin.sock");
    }

    /// The opaque half of the same rule: rp interprets `auth` on the
    /// registrations it dials alone, so a tool provider's
    /// differently-shaped `auth` key — it authenticates rp's MCP client
    /// its own way — is its author's business and must not fail rp's load.
    #[test]
    fn an_undialed_plugins_own_auth_shape_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plugins": [{
                    "name": "ml-quality-classifier",
                    "type": "tool_provider",
                    "mcp_server_url": "http://127.0.0.1:11150/mcp",
                    "auth": {"bearer_token": "the-plugin-authors-own-shape"}
                }],
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(
            config.plugins[0]["auth"]["bearer_token"],
            "the-plugin-authors-own-shape"
        );
    }

    /// The opaque half of the same rule: a registration's other keys stay
    /// the plugin author's business, and an absent `auth` is the ordinary
    /// no-credential case.
    #[test]
    fn a_plugin_registration_without_auth_loads_with_its_own_keys_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "session": {"data_directory": "/tmp/rp-test"},
                "equipment": {},
                "plugins": [{
                    "name": "calibrator-flats",
                    "type": "orchestrator",
                    "invoke_url": "http://127.0.0.1:11170/invoke",
                    "config": {"camera_id": "main-cam"},
                    "some_plugin_specific_key": 42
                }],
                "server": { "port": 0 }
            }"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        assert_eq!(config.plugins.len(), 1);
        assert_eq!(config.plugins[0]["some_plugin_specific_key"], 42);
    }
}
