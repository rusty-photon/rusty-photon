//! Reading the config directory (docs/services/doctor.md §Config parsing).
//!
//! For each catalog service, doctor reads `<svc>.json` the way the service
//! itself would: the file must be valid JSON, and the top-level `server`
//! block must parse under the catalog-declared shared shape, including
//! `deny_unknown_fields`. Everything else in every file stays opaque
//! `serde_json::Value` except the known cross-reference blocks, which are
//! read **leniently** (doctor knows only the fields it joins across, never
//! the whole shape).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusty_photon_server_config::doctor_toml::ServerClass;
use rusty_photon_server_config::{AdvertisingServerConfig, AlpacaServerConfig, ServerConfig};
use serde::Deserialize;
use serde_json::Value;

use crate::catalog::CatalogEntry;

/// Why a config file's contents are unavailable — two different operator
/// problems, diagnosed under two different check names.
#[derive(Debug)]
pub enum ReadError {
    /// The file exists but could not be read (permissions, I/O).
    Unreadable(String),
    /// The file was read but is not valid JSON.
    InvalidJson(String),
}

/// The `server` block, parsed under the catalog-declared shape.
#[derive(Debug)]
pub enum ServerBlock {
    /// No config file — the service will self-create defaults on first run.
    FileAbsent,
    /// No parseable `server` block to judge: the file has no `server` key
    /// (the service applies its defaults), or the file itself is unreadable
    /// or invalid JSON — the read-level checks own that diagnosis, and the
    /// effective port falls back to the catalog default either way.
    BlockAbsent,
    /// Parsed. `discovery_port` is `None` for core services;
    /// `advertised_url` is `None` for everything but the advertising
    /// class (rp), whose `core()` view drops it — the ACME convergence
    /// check `rp.advertised-url` is why it is carried here.
    Parsed {
        server: ServerConfig,
        discovery_port: Option<u16>,
        advertised_url: Option<String>,
    },
    /// The block would make the service refuse to start.
    Invalid(String),
}

/// One catalog service's on-disk state.
#[derive(Debug)]
pub struct ServiceScan {
    pub entry: &'static CatalogEntry,
    pub config_path: PathBuf,
    /// `None` — file absent; `Err` — unreadable or not valid JSON.
    pub raw: Option<Result<Value, ReadError>>,
    pub server: ServerBlock,
}

impl ServiceScan {
    /// Whether this service takes part in diagnosis: its unit is installed
    /// or its config file exists.
    #[must_use]
    pub const fn config_present(&self) -> bool {
        self.raw.is_some()
    }

    /// The parsed config `Value`, when the file exists and is valid JSON.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.raw.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The port this service will actually use: the configured
    /// `server.port`, else the catalog default (also while the server block
    /// is unparseable — the collision picture should not go blind because
    /// one file has a typo).
    #[must_use]
    pub const fn effective_port(&self) -> u16 {
        match &self.server {
            ServerBlock::Parsed { server, .. } => server.port,
            _ => self.entry.default_port,
        }
    }

    /// The enabled discovery port, when the config sets one.
    #[must_use]
    pub const fn discovery_port(&self) -> Option<u16> {
        match &self.server {
            ServerBlock::Parsed { discovery_port, .. } => *discovery_port,
            _ => None,
        }
    }

    /// The parsed TLS/auth view of the server block, when parsed.
    #[must_use]
    pub const fn server(&self) -> Option<&ServerConfig> {
        match &self.server {
            ServerBlock::Parsed { server, .. } => Some(server),
            _ => None,
        }
    }
}

/// Scan one service's config file.
#[must_use]
pub fn scan_service(config_dir: &Path, entry: &'static CatalogEntry) -> ServiceScan {
    let config_path = config_dir.join(entry.config_file());
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(content) => Some(
            serde_json::from_str::<Value>(&content)
                .map_err(|e| ReadError::InvalidJson(e.to_string())),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Err(ReadError::Unreadable(e.to_string()))),
    };
    scan_from_raw(config_dir, entry, raw)
}

/// A [`ServiceScan`] over an in-memory config value instead of the file.
///
/// The flip orchestrator's staged planning rounds re-scan the values its
/// ops mutated without writing them (docs/services/doctor.md §The flip
/// orchestrator).
#[must_use]
pub fn scan_service_from_value(
    config_dir: &Path,
    entry: &'static CatalogEntry,
    value: Value,
) -> ServiceScan {
    scan_from_raw(config_dir, entry, Some(Ok(value)))
}

fn scan_from_raw(
    config_dir: &Path,
    entry: &'static CatalogEntry,
    raw: Option<Result<Value, ReadError>>,
) -> ServiceScan {
    let config_path = config_dir.join(entry.config_file());
    let server = match &raw {
        None => ServerBlock::FileAbsent,
        Some(Err(_)) => ServerBlock::BlockAbsent,
        Some(Ok(value)) => value
            .get("server")
            .map_or(ServerBlock::BlockAbsent, |block| {
                parse_server_block(block, entry.class)
            }),
    };
    ServiceScan {
        entry,
        config_path,
        raw,
        server,
    }
}

fn parse_server_block(block: &Value, class: ServerClass) -> ServerBlock {
    match class {
        ServerClass::Alpaca => match AlpacaServerConfig::deserialize(block) {
            Ok(config) => ServerBlock::Parsed {
                discovery_port: config.discovery_port,
                server: config.core(),
                advertised_url: None,
            },
            Err(e) => ServerBlock::Invalid(e.to_string()),
        },
        ServerClass::Core => match ServerConfig::deserialize(block) {
            Ok(server) => ServerBlock::Parsed {
                server,
                discovery_port: None,
                advertised_url: None,
            },
            Err(e) => ServerBlock::Invalid(e.to_string()),
        },
        ServerClass::Advertising => match AdvertisingServerConfig::deserialize(block) {
            Ok(config) => ServerBlock::Parsed {
                advertised_url: config
                    .advertised_url
                    .as_ref()
                    .map(|url| url.as_str().to_string()),
                server: config.core(),
                discovery_port: None,
            },
            Err(e) => ServerBlock::Invalid(e.to_string()),
        },
    }
}

/// The `.json` files in the config dir that belong to no catalog service —
/// candidates for the unknown-config warning.
///
/// Known non-service files are exempt: `acme.json` (doctor's ACME state)
/// lives beside the configs.
pub fn unknown_config_files(config_dir: &Path, known: &[String]) -> Vec<String> {
    const NON_SERVICE_FILES: &[&str] = &["acme.json"];
    let Ok(entries) = std::fs::read_dir(config_dir) else {
        return Vec::new();
    };
    let mut unknown: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| {
            std::path::Path::new(name).extension() == Some(std::ffi::OsStr::new("json"))
                && !NON_SERVICE_FILES.contains(&name.as_str())
                && !known.contains(name)
        })
        .collect();
    unknown.sort();
    unknown
}

// ---- Lenient views of the known cross-reference blocks ----
//
// No `deny_unknown_fields` anywhere below: these are *doctor's* partial
// views of other services' shapes, and must keep working as those shapes
// grow fields doctor does not join across.

/// ui-htmx: the retired `drivers` override map, read only to diagnose it
/// (`config.retired-keys`), plus the `rp`/`sentinel` client targets.
///
/// Doctor joins the targets against their own server TLS/auth state
/// (docs/services/doctor.md §Client-target joins).
#[derive(Debug, Deserialize, Default)]
pub struct UiHtmxView {
    #[serde(default)]
    pub drivers: Option<Value>,
    #[serde(default)]
    pub rp: Option<ClientTargetView>,
    #[serde(default)]
    pub sentinel: Option<ClientTargetView>,
}

/// A `base_url` client target with an optional credential and CA-trust
/// path — ui-htmx's `rp`/`sentinel` blocks (`services/ui-htmx/src/config.rs`
/// `RpTarget`/`SentinelTarget`).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ClientTargetView {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: Option<ClientAuthView>,
    #[serde(default)]
    pub ca_cert_path: Option<String>,
}

/// sentinel: one watchdog operation family.
#[derive(Debug, Deserialize, Default)]
pub struct WatchdogOperationView {
    #[serde(default)]
    pub service: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct WatchdogView {
    /// The rp instance the watchdog subscribes to — a client target doctor
    /// joins against rp's own server TLS state (docs/services/doctor.md
    /// §Client-target joins). Its auth is the shared `service_auth`
    /// pair below, already covered by `auth.mismatch`.
    #[serde(default)]
    pub rp_url: Option<String>,
    #[serde(default)]
    pub operations: BTreeMap<String, WatchdogOperationView>,
}

/// A plaintext HTTP Basic credential.
///
/// sentinel's doctor-written `service_auth`/per-monitor `auth`, and
/// ui-htmx's client target `auth` (docs/services/sentinel.md,
/// docs/services/doctor.md §Client-target joins).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ClientAuthView {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// sentinel: one Alpaca safety monitor's connection facts — a client
/// target doctor joins against the named service's own TLS/auth state.
///
/// Defaults mirror `services/sentinel/src/config.rs`'s `MonitorConfig`;
/// fields doctor does not join across (`name`, `device_number`,
/// `polling_interval`) are read leniently and ignored.
#[derive(Debug, Deserialize)]
pub struct MonitorView {
    #[serde(default = "default_monitor_host")]
    pub host: String,
    #[serde(default = "default_monitor_port")]
    pub port: u16,
    #[serde(default = "default_monitor_scheme")]
    pub scheme: String,
    #[serde(default)]
    pub auth: Option<ClientAuthView>,
}

fn default_monitor_host() -> String {
    "localhost".to_string()
}

const fn default_monitor_port() -> u16 {
    11111
}

fn default_monitor_scheme() -> String {
    "http".to_string()
}

/// sentinel: the blocks doctor joins across.
///
/// The retired `services` map is read only to diagnose it
/// (`config.retired-keys`) — since D3s sentinel discovers its services
/// from the platform service manager. `ca_cert` is the single top-level
/// CA every sentinel client trusts — the watchdog's `rp_url` and each
/// monitor alike, the same shape as `RpView::ca_cert`.
#[derive(Debug, Deserialize, Default)]
pub struct SentinelView {
    #[serde(default)]
    pub services: Option<Value>,
    #[serde(default)]
    pub operation_watchdog: Option<WatchdogView>,
    #[serde(default)]
    pub service_auth: Option<ClientAuthView>,
    #[serde(default)]
    pub monitors: Vec<MonitorView>,
    #[serde(default)]
    pub ca_cert: Option<String>,
    /// The probe-host override sentinel dials `<service>.<probe_domain>`
    /// names with — what the ACME convergence check
    /// `sentinel.probe-domain` writes from `acme.json`'s `domain`.
    #[serde(default)]
    pub probe_domain: Option<String>,
}

/// rp: the session block fields doctor checks — the data directory
/// (`rp.data-directory`) and the retired state-file key
/// (`rp.orchestrator-registration-removed`).
#[derive(Debug, Deserialize, Default)]
pub struct RpSessionView {
    #[serde(default)]
    pub data_directory: Option<String>,
    #[serde(default)]
    pub session_state_file: Option<String>,
}

/// planetarium-bridge: the one field the fake-mount check reads
/// (`joins.fake-mount`, planetarium-bridge.md § Doctor integration).
///
/// That is the bridge's minted Alpaca `UniqueID`, which the probe leg
/// compares against what rp's configured mount actually reports.
#[derive(Debug, Deserialize, Default)]
pub struct BridgeView {
    #[serde(default)]
    pub device: Option<BridgeDeviceView>,
}

/// See [`BridgeView`].
#[derive(Debug, Deserialize, Default)]
pub struct BridgeDeviceView {
    #[serde(default)]
    pub unique_id: Option<String>,
}

/// rp: the client target block for `plate_solver` — a URL plus an
/// optional per-target credential (issue #620).
///
/// CA trust is a separate, top-level `RpView::ca_cert` shared by every rp
/// client (issue #609 / PR #612), not per-target. The guider's equivalent
/// block (nested inside `equipment.mount.guiding`) is read via
/// `RpView::mount_guiding_url` / `RpView::mount_guiding_auth` instead,
/// since `equipment` stays an opaque `Value`.
#[derive(Debug, Deserialize, Default)]
pub struct RpUrlTargetView {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub auth: Option<ClientAuthView>,
}

/// rp: the blocks doctor reads.
///
/// `equipment` stays a `Value` — device usage is opaque; only each entry's
/// `alpaca_url` and the mount's nested `guiding.url` are extracted.
/// `plugins` is opaque for the same reason: registrations are a
/// plugin-author surface, and only an event plugin's `webhook_url` +
/// `auth` are extracted (issue #800) — plus each entry's `type`, for the
/// retired-orchestrator check.
#[derive(Debug, Deserialize, Default)]
pub struct RpView {
    #[serde(default)]
    pub equipment: Option<Value>,
    #[serde(default)]
    pub session: Option<RpSessionView>,
    #[serde(default)]
    pub plate_solver: Option<RpUrlTargetView>,
    #[serde(default)]
    pub plugins: Vec<Value>,
    #[serde(default)]
    pub ca_cert: Option<String>,
}

impl RpView {
    /// Every `alpaca_url` in the equipment block, wherever the entry lives
    /// (`equipment.<kind>[].alpaca_url`).
    pub fn alpaca_urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        if let Some(Value::Object(kinds)) = &self.equipment {
            for entries in kinds.values() {
                if let Value::Array(entries) = entries {
                    for entry in entries {
                        if let Some(url) = entry.get("alpaca_url").and_then(Value::as_str) {
                            urls.push(url.to_string());
                        }
                    }
                }
            }
        }
        urls
    }

    /// The guider service's URL at `equipment.mount.guiding.url` — `mount`
    /// is a singular object (at most one mount per rp deployment), unlike
    /// the plural array kinds `alpaca_urls` walks.
    pub fn mount_guiding_url(&self) -> Option<String> {
        self.equipment
            .as_ref()?
            .get("mount")?
            .get("guiding")?
            .get("url")?
            .as_str()
            .map(str::to_string)
    }

    /// The mount driver's own URL at `equipment.mount.alpaca_url` — the
    /// subject of the fake-mount hazard check (`joins.fake-mount`,
    /// planetarium-bridge.md § Doctor integration). Also walked by
    /// [`Self::equipment_targets`] for the generic scheme/auth joins.
    pub fn mount_alpaca_url(&self) -> Option<String> {
        self.equipment
            .as_ref()?
            .get("mount")?
            .get("alpaca_url")?
            .as_str()
            .map(str::to_string)
    }

    /// rp's client credential for the mount at `equipment.mount.auth` —
    /// what the fake-mount probe presents to the mount's management API,
    /// exactly as rp itself would connect.
    #[must_use]
    pub fn mount_auth(&self) -> Option<ClientAuthView> {
        let auth = self.equipment.as_ref()?.get("mount")?.get("auth")?;
        serde_json::from_value(auth.clone()).ok()
    }

    /// The guider service's credential at `equipment.mount.guiding.auth`
    /// (issue #620) — `None` when absent or when the block does not parse
    /// as a `ClientAuthView`.
    #[must_use]
    pub fn mount_guiding_auth(&self) -> Option<ClientAuthView> {
        let auth = self
            .equipment
            .as_ref()?
            .get("mount")?
            .get("guiding")?
            .get("auth")?;
        serde_json::from_value(auth.clone()).ok()
    }

    /// Every generic equipment client target — each `equipment.<kind>[].alpaca_url`
    /// entry plus the singular `equipment.mount.alpaca_url` (issue #663) —
    /// alongside its own `auth` field. Kind-agnostic by construction (any
    /// object under `equipment` carrying an `alpaca_url`, whether nested in
    /// an array or standalone like `mount`), so a future equipment kind
    /// picks up scheme/auth join checking without new doctor code, mirroring
    /// `alpaca_urls()`'s existing walk. The guider's URL
    /// (`equipment.mount.guiding.url`) lives under a `url` key, not
    /// `alpaca_url`, so it never collides with this walk.
    pub fn equipment_targets(&self) -> Vec<RpClientTarget> {
        let mut targets = Vec::new();
        let Some(Value::Object(kinds)) = &self.equipment else {
            return targets;
        };
        for (kind, value) in kinds {
            // `kind` is a JSON object key from the opaque `equipment` value
            // (never validated against `EquipmentConfig`'s known field
            // names before this point), so it must be RFC-6901-escaped
            // before it becomes a pointer segment — same reason ui-htmx's
            // free-form `drivers` map keys go through `escape_token`
            // (fix.rs) rather than straight into a pointer string.
            let escaped_kind = crate::fix::escape_token(kind);
            match value {
                Value::Array(entries) => {
                    for (idx, entry) in entries.iter().enumerate() {
                        if let Some(url) = entry.get("alpaca_url").and_then(Value::as_str) {
                            targets.push(RpClientTarget {
                                field: format!("equipment.{kind}.{idx}.alpaca_url"),
                                url: url.to_string(),
                                url_pointer: format!("/equipment/{escaped_kind}/{idx}/alpaca_url"),
                                auth_pointer: format!("/equipment/{escaped_kind}/{idx}/auth"),
                                auth: entry
                                    .get("auth")
                                    .and_then(|a| serde_json::from_value(a.clone()).ok()),
                            });
                        }
                    }
                }
                Value::Object(_) => {
                    if let Some(url) = value.get("alpaca_url").and_then(Value::as_str) {
                        targets.push(RpClientTarget {
                            field: format!("equipment.{kind}.alpaca_url"),
                            url: url.to_string(),
                            url_pointer: format!("/equipment/{escaped_kind}/alpaca_url"),
                            auth_pointer: format!("/equipment/{escaped_kind}/auth"),
                            auth: value
                                .get("auth")
                                .and_then(|a| serde_json::from_value(a.clone()).ok()),
                        });
                    }
                }
                _ => {}
            }
        }
        targets
    }

    /// The callback URL of every registration rp dials — an event
    /// plugin's `webhook_url`, what it POSTs each subscribed event to
    /// (rp.md § Delivery: Webhooks) — alongside that registration's own
    /// `auth` field (issue #800).
    ///
    /// Deliberately narrower than [`Self::equipment_targets`]'s
    /// walk-anything-with-an-`alpaca_url` rule: a registration is an
    /// opaque plugin-author surface, and rp interprets a callback URL and
    /// `auth` on that entry type alone. Walking by field alone would
    /// let `--fix` write a credential into a registration rp never reads,
    /// and file a transport verdict on a URL rp never calls — a tool
    /// provider, for one, is reached over MCP and authenticates however
    /// its author chose. The scope mirrors rp's own
    /// `config::EVENT_URL_FIELD` mapping; the two must agree, or doctor
    /// advises a fix rp ignores. (The orchestrator's `invoke_url` left
    /// the set with mcp-sessionless D6: rp dials no orchestrator, and
    /// such an entry is reported by `rp.orchestrator-registration-removed`
    /// instead.)
    #[must_use]
    pub fn plugin_targets(&self) -> Vec<RpClientTarget> {
        self.plugins
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let url_field = dialed_url_field(entry)?;
                let url = entry.get(url_field)?.as_str()?;
                Some(RpClientTarget {
                    field: format!("plugins.{idx}.{url_field}"),
                    url: url.to_string(),
                    url_pointer: format!("/plugins/{idx}/{url_field}"),
                    auth_pointer: format!("/plugins/{idx}/auth"),
                    auth: entry
                        .get("auth")
                        .and_then(|a| serde_json::from_value(a.clone()).ok()),
                })
            })
            .collect()
    }
}

/// The callback field rp POSTs to for this registration, or `None` when
/// rp would not dial it at all.
///
/// Mirrors the field rp reads per dialed type (`config::EVENT_URL_FIELD`).
/// doctor cannot share rp's code here (it reads the config as opaque JSON
/// from another crate), so the two are held together by their tests.
///
/// Every registration of the dialed type is joined, with no second guess
/// at whether rp would actually dial it: rp refuses to start on an event
/// registration it cannot deliver to (rp.md § Delivery: Webhooks), so an
/// entry that reaches a running rig is one rp dials. A `type:
/// "orchestrator"` entry is not dialed — rp rejects it at load
/// (mcp-sessionless D11) — so it is never joined.
fn dialed_url_field(entry: &Value) -> Option<&'static str> {
    match entry.get("type").and_then(Value::as_str)? {
        "event" => Some("webhook_url"),
        _ => None,
    }
}

/// One client target rp walks a config-controlled collection for —
/// [`RpView::equipment_targets`]'s generic equipment roster and
/// [`RpView::plugin_targets`]'s plugin registrations.
///
/// `field` is a dotted path (the same convention rp's own `field_errors`
/// uses, e.g. `equipment.cameras.0.alpaca_url`) for **display only** —
/// unlike `rp_client_joins`'s other two call sites, this is not run back
/// through `field.replace('.', "/")` to derive a pointer, because a `kind`
/// key is config-controlled and may itself need RFC-6901 escaping.
/// `url_pointer`/`auth_pointer` are pre-built, already-escaped JSON
/// pointers.
#[derive(Debug, Clone)]
pub struct RpClientTarget {
    pub field: String,
    pub url: String,
    pub url_pointer: String,
    pub auth_pointer: String,
    pub auth: Option<ClientAuthView>,
}

/// Parse a lenient view out of a scanned config, distinguishing "view not
/// applicable" (file absent / invalid JSON, `None`) from "the known block
/// itself does not parse" (`Some(Err)`).
#[must_use]
pub fn view<T: for<'de> Deserialize<'de>>(scan: &ServiceScan) -> Option<Result<T, String>> {
    let value = scan.value()?;
    Some(T::deserialize(value.clone()).map_err(|e| e.to_string()))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::unreachable)]
mod tests {
    use super::*;
    use crate::catalog;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn test_scan_distinguishes_absent_invalid_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let entry = catalog::entry("qhy-focuser").unwrap();

        let scan = scan_service(dir.path(), entry);
        assert!(matches!(scan.server, ServerBlock::FileAbsent));
        assert!(!scan.config_present());
        assert_eq!(scan.effective_port(), 11113);

        write(dir.path(), "qhy-focuser.json", "{ not json");
        let scan = scan_service(dir.path(), entry);
        assert!(scan.raw.as_ref().unwrap().is_err());
        assert_eq!(scan.effective_port(), 11113);

        write(
            dir.path(),
            "qhy-focuser.json",
            r#"{ "server": { "port": 4711, "discovery_port": 32227 } }"#,
        );
        let scan = scan_service(dir.path(), entry);
        assert_eq!(scan.effective_port(), 4711);
        assert_eq!(scan.discovery_port(), Some(32227));
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_config_is_distinguished_from_invalid_json() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "qhy-focuser.json", "{}");
        let path = dir.path().join("qhy-focuser.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let scan = scan_service(dir.path(), catalog::entry("qhy-focuser").unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        match scan.raw {
            Some(Err(ReadError::Unreadable(_))) => {}
            Some(Ok(_)) => {} // running privileged — mode 000 still reads
            other => unreachable!("expected Unreadable, got {other:?}"),
        }
    }

    #[test]
    fn test_server_block_rejects_the_wrong_class_shape() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ui-htmx.json",
            r#"{ "server": { "port": 11120, "discovery_port": 32227 } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("ui-htmx").unwrap());
        let ServerBlock::Invalid(err) = &scan.server else {
            unreachable!("core shape must reject discovery_port: {:?}", scan.server);
        };
        assert!(err.contains("discovery_port"), "{err}");
    }

    #[test]
    fn test_unknown_config_files_exempt_known_non_service_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "qhy-focuser.json", "{}");
        write(dir.path(), "qhy-focusser.json", "{}");
        write(dir.path(), "acme.json", "{}");
        write(dir.path(), "notes.txt", "not json");
        let unknown = unknown_config_files(dir.path(), &["qhy-focuser.json".to_string()]);
        assert_eq!(unknown, vec!["qhy-focusser.json".to_string()]);
    }

    #[test]
    fn test_rp_view_collects_alpaca_urls_across_kinds() {
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": {
                   "cameras": [ { "alpaca_url": "http://localhost:11121", "role": "guide" } ],
                   "mounts": [ { "alpaca_url": "http://localhost:11117" }, {} ] },
                 "session": { "data_directory": "/var/lib/x" },
                 "unknown_future_block": 1 }"#,
        )
        .unwrap();
        assert_eq!(
            view.alpaca_urls(),
            vec!["http://localhost:11121", "http://localhost:11117"]
        );
        assert_eq!(view.session.unwrap().data_directory.unwrap(), "/var/lib/x");
    }

    #[test]
    fn test_rp_view_reads_plate_solver_and_mount_guiding_urls() {
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                            "guiding": { "url": "http://localhost:11130" } } },
                 "plate_solver": { "url": "http://localhost:11131", "timeout": "1m" } }"#,
        )
        .unwrap();
        assert_eq!(
            view.mount_guiding_url().as_deref(),
            Some("http://localhost:11130")
        );
        assert_eq!(
            view.plate_solver.unwrap().url.as_deref(),
            Some("http://localhost:11131")
        );
    }

    #[test]
    fn test_rp_view_mount_guiding_url_absent_without_a_mount_or_guider() {
        let view: RpView = serde_json::from_str(r#"{ "equipment": {} }"#).unwrap();
        assert!(view.mount_guiding_url().is_none());
        let view: RpView =
            serde_json::from_str(r#"{ "equipment": { "mount": { "alpaca_url": "x" } } }"#).unwrap();
        assert!(view.mount_guiding_url().is_none());
    }

    #[test]
    fn test_rp_view_reads_plate_solver_and_mount_guiding_auth() {
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                            "guiding": { "url": "http://localhost:11130",
                                                         "auth": { "username": "observatory", "password": "gpw" } } } },
                 "plate_solver": { "url": "http://localhost:11131",
                                    "auth": { "username": "observatory", "password": "ppw" } } }"#,
        )
        .unwrap();
        let guiding_auth = view.mount_guiding_auth().unwrap();
        assert_eq!(guiding_auth.username.as_deref(), Some("observatory"));
        assert_eq!(guiding_auth.password.as_deref(), Some("gpw"));
        let ps_auth = view.plate_solver.unwrap().auth.unwrap();
        assert_eq!(ps_auth.username.as_deref(), Some("observatory"));
        assert_eq!(ps_auth.password.as_deref(), Some("ppw"));
    }

    #[test]
    fn test_rp_view_mount_guiding_auth_absent_without_a_credential() {
        let view: RpView = serde_json::from_str(r#"{ "equipment": {} }"#).unwrap();
        assert!(view.mount_guiding_auth().is_none());
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                            "guiding": { "url": "http://localhost:11130" } } } }"#,
        )
        .unwrap();
        assert!(view.mount_guiding_auth().is_none());
    }

    #[test]
    fn test_rp_view_equipment_targets_covers_arrays_and_the_singular_mount() {
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": {
                   "mount": { "alpaca_url": "http://localhost:11117",
                              "guiding": { "url": "http://localhost:11130" },
                              "auth": { "username": "observatory", "password": "mpw" } },
                   "cameras": [ { "alpaca_url": "http://localhost:11122",
                                  "auth": { "username": "observatory", "password": "cpw" } },
                                { "alpaca_url": "http://localhost:11121" } ],
                   "optical_trains": [ { "cameras": ["main"] } ] } }"#,
        )
        .unwrap();
        let targets = view.equipment_targets();
        assert_eq!(targets.len(), 3, "{targets:?}");

        let mount = targets
            .iter()
            .find(|t| t.field == "equipment.mount.alpaca_url")
            .expect("the singular mount target");
        assert_eq!(mount.url, "http://localhost:11117");
        assert_eq!(mount.url_pointer, "/equipment/mount/alpaca_url");
        assert_eq!(mount.auth_pointer, "/equipment/mount/auth");
        assert_eq!(
            mount.auth.as_ref().unwrap().password.as_deref(),
            Some("mpw")
        );

        let camera0 = targets
            .iter()
            .find(|t| t.field == "equipment.cameras.0.alpaca_url")
            .expect("the first camera target");
        assert_eq!(camera0.url, "http://localhost:11122");
        assert_eq!(camera0.url_pointer, "/equipment/cameras/0/alpaca_url");
        assert_eq!(camera0.auth_pointer, "/equipment/cameras/0/auth");
        assert_eq!(
            camera0.auth.as_ref().unwrap().password.as_deref(),
            Some("cpw")
        );

        let camera1 = targets
            .iter()
            .find(|t| t.field == "equipment.cameras.1.alpaca_url")
            .expect("the second camera target");
        assert_eq!(camera1.url, "http://localhost:11121");
        assert!(camera1.auth.is_none());

        // optical_trains carries no alpaca_url — never a target.
        assert!(!targets.iter().any(|t| t.field.contains("optical_trains")));
    }

    #[test]
    fn test_rp_view_equipment_targets_empty_without_equipment() {
        let view: RpView = serde_json::from_str(r#"{ "equipment": {} }"#).unwrap();
        assert!(view.equipment_targets().is_empty());

        let view: RpView = serde_json::from_str(r"{}").unwrap();
        assert!(view.equipment_targets().is_empty());
    }

    #[test]
    fn test_rp_view_equipment_targets_escapes_a_slash_in_the_kind_key() {
        // `equipment` is opaque (never checked against EquipmentConfig's
        // known field names before this runs), so a stray '/' or '~' in a
        // kind key must not mis-segment the fix pointer — RFC 6901, same
        // reason ui-htmx's free-form `drivers` map keys go through
        // `fix::escape_token`.
        let view: RpView = serde_json::from_str(
            r#"{ "equipment": { "weird/kind": [ { "alpaca_url": "http://localhost:11122" } ],
                                 "weird~kind": { "alpaca_url": "http://localhost:11117" } } }"#,
        )
        .unwrap();
        let targets = view.equipment_targets();
        assert_eq!(targets.len(), 2, "{targets:?}");

        let array_entry = targets
            .iter()
            .find(|t| t.url == "http://localhost:11122")
            .expect("the array-kind target");
        assert_eq!(
            array_entry.url_pointer,
            "/equipment/weird~1kind/0/alpaca_url"
        );
        assert_eq!(array_entry.auth_pointer, "/equipment/weird~1kind/0/auth");

        let singular_entry = targets
            .iter()
            .find(|t| t.url == "http://localhost:11117")
            .expect("the singular-kind target");
        assert_eq!(
            singular_entry.url_pointer,
            "/equipment/weird~0kind/alpaca_url"
        );
        assert_eq!(singular_entry.auth_pointer, "/equipment/weird~0kind/auth");
    }

    #[test]
    fn test_rp_view_reads_the_top_level_ca_cert() {
        let view: RpView =
            serde_json::from_str(r#"{ "equipment": {}, "ca_cert": "/pki/ca.pem" }"#).unwrap();
        assert_eq!(view.ca_cert.as_deref(), Some("/pki/ca.pem"));

        let view: RpView = serde_json::from_str(r#"{ "equipment": {} }"#).unwrap();
        assert!(view.ca_cert.is_none());
    }

    #[test]
    fn test_sentinel_view_reads_the_top_level_ca_cert() {
        let view: SentinelView = serde_json::from_str(r#"{ "ca_cert": "/pki/ca.pem" }"#).unwrap();
        assert_eq!(view.ca_cert.as_deref(), Some("/pki/ca.pem"));

        let view: SentinelView = serde_json::from_str(r#"{ "monitors": [] }"#).unwrap();
        assert!(view.ca_cert.is_none());
    }

    #[test]
    fn test_ui_htmx_view_reads_rp_and_sentinel_targets() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ui-htmx.json",
            r#"{ "server": { "port": 11120 },
                 "rp": { "base_url": "http://127.0.0.1:11115" },
                 "sentinel": { "base_url": "https://127.0.0.1:11114",
                               "auth": { "username": "observatory", "password": "s3cret" },
                               "ca_cert_path": "/pki/ca.pem" } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("ui-htmx").unwrap());
        let ui: UiHtmxView = view(&scan).unwrap().unwrap();
        let rp = ui.rp.unwrap();
        assert_eq!(rp.base_url.as_deref(), Some("http://127.0.0.1:11115"));
        assert!(rp.auth.is_none());
        assert!(rp.ca_cert_path.is_none());
        let sentinel = ui.sentinel.unwrap();
        assert_eq!(
            sentinel.base_url.as_deref(),
            Some("https://127.0.0.1:11114")
        );
        assert_eq!(
            sentinel.auth.unwrap().username.as_deref(),
            Some("observatory")
        );
        assert_eq!(sentinel.ca_cert_path.as_deref(), Some("/pki/ca.pem"));
    }

    #[test]
    fn test_sentinel_view_reads_monitors_and_watchdog_rp_url_leniently() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sentinel.json",
            r#"{ "server": { "port": 11114 },
                 "monitors": [ { "type": "alpaca_safety_monitor", "name": "Roof",
                                  "host": "localhost", "port": 11119, "scheme": "http",
                                  "device_number": 0, "polling_interval": "30s" } ],
                 "operation_watchdog": { "rp_url": "http://localhost:11115" } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("sentinel").unwrap());
        let sentinel: SentinelView = view(&scan).unwrap().unwrap();
        assert_eq!(sentinel.monitors.len(), 1);
        assert_eq!(sentinel.monitors[0].port, 11119);
        assert_eq!(sentinel.monitors[0].scheme, "http");
        assert!(sentinel.monitors[0].auth.is_none());
        assert_eq!(
            sentinel.operation_watchdog.unwrap().rp_url.as_deref(),
            Some("http://localhost:11115")
        );
    }

    #[test]
    fn test_sentinel_view_monitor_defaults_mirror_sentinels_own_config() {
        let view: SentinelView = serde_json::from_str(
            r#"{ "monitors": [ { "type": "alpaca_safety_monitor", "name": "Roof" } ] }"#,
        )
        .unwrap();
        let monitor = &view.monitors[0];
        assert_eq!(monitor.host, "localhost");
        assert_eq!(monitor.port, 11111);
        assert_eq!(monitor.scheme, "http");
    }

    #[test]
    fn test_views_parse_leniently_but_report_shape_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sentinel.json",
            r#"{ "server": { "port": 11114 },
                 "operation_watchdog": { "operations": "not a map" } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("sentinel").unwrap());
        let result: Result<SentinelView, String> = view(&scan).unwrap();
        result.unwrap_err();

        write(
            dir.path(),
            "sentinel.json",
            r#"{ "server": { "port": 11114 },
                 "operation_watchdog": {
                   "rp_url": "http://localhost:11115",
                   "operations": { "slew": { "service": "qhy-focuser", "future": 1 } } },
                 "dashboard_extras": true }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("sentinel").unwrap());
        let sentinel: SentinelView = view(&scan).unwrap().unwrap();
        let watchdog = sentinel.operation_watchdog.unwrap();
        assert_eq!(
            watchdog.operations["slew"].service.as_deref(),
            Some("qhy-focuser")
        );
        assert!(sentinel.services.is_none(), "no retired key present");
    }

    #[test]
    fn test_sentinel_view_reads_probe_domain() {
        let view: SentinelView =
            serde_json::from_str(r#"{ "probe_domain": "pier1.example.com" }"#).unwrap();
        assert_eq!(view.probe_domain.as_deref(), Some("pier1.example.com"));

        let view: SentinelView = serde_json::from_str(r#"{ "monitors": [] }"#).unwrap();
        assert!(view.probe_domain.is_none());
    }

    #[test]
    fn test_an_advertising_scan_surfaces_advertised_url() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "rp.json",
            r#"{ "server": { "port": 11115,
                             "advertised_url": "https://rp.pier1.example.com:11115" } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("rp").unwrap());
        let ServerBlock::Parsed { advertised_url, .. } = &scan.server else {
            unreachable!("the block parses: {:?}", scan.server);
        };
        assert_eq!(
            advertised_url.as_deref(),
            Some("https://rp.pier1.example.com:11115")
        );

        write(dir.path(), "rp.json", r#"{ "server": { "port": 11115 } }"#);
        let scan = scan_service(dir.path(), catalog::entry("rp").unwrap());
        let ServerBlock::Parsed { advertised_url, .. } = &scan.server else {
            unreachable!("the block parses: {:?}", scan.server);
        };
        assert!(advertised_url.is_none());
    }

    #[test]
    fn test_non_advertising_scans_never_carry_an_advertised_url() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "ui-htmx.json",
            r#"{ "server": { "port": 11120 } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("ui-htmx").unwrap());
        let ServerBlock::Parsed { advertised_url, .. } = &scan.server else {
            unreachable!("the block parses: {:?}", scan.server);
        };
        assert!(advertised_url.is_none());

        write(
            dir.path(),
            "qhy-focuser.json",
            r#"{ "server": { "port": 11113 } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("qhy-focuser").unwrap());
        let ServerBlock::Parsed { advertised_url, .. } = &scan.server else {
            unreachable!("the block parses: {:?}", scan.server);
        };
        assert!(advertised_url.is_none());
    }

    #[test]
    fn test_views_surface_retired_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "sentinel.json",
            r#"{ "server": { "port": 11114 },
                 "services": { "cam": { "restart_command": "systemctl restart x" } } }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("sentinel").unwrap());
        let sentinel: SentinelView = view(&scan).unwrap().unwrap();
        assert!(sentinel.services.is_some(), "the retired map must be seen");

        write(
            dir.path(),
            "ui-htmx.json",
            r#"{ "server": { "port": 11120 },
                 "drivers": {} }"#,
        );
        let scan = scan_service(dir.path(), catalog::entry("ui-htmx").unwrap());
        let ui: UiHtmxView = view(&scan).unwrap().unwrap();
        assert!(ui.drivers.is_some(), "the retired map must be seen");
    }
}
