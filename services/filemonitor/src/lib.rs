#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
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

pub mod config_actions;
pub mod doctor;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ascom_alpaca::api::{CargoServerInfo, Device, SafetyMonitor};
use ascom_alpaca::{ASCOMError, ASCOMErrorCode, ASCOMResult, Server};
pub use rusty_photon_server_config::AlpacaServerConfig;
use rusty_photon_service_lifecycle::ReloadSignal;
use rusty_photon_tls::config::TlsConfig;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::config_actions::FileMonitorDriver;
use rusty_photon_driver::ConfigActionCtx;

/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device: DeviceConfig,
    pub file: FileConfig,
    pub parsing: ParsingConfig,
    pub server: AlpacaServerConfig,
}

/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub name: String,
    pub unique_id: String,
    pub description: String,
}

/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub path: PathBuf,
    // `humantime_serde` stores the duration as a string (e.g. "60s"); tell
    // schemars to describe it as a string so the generated schema matches the
    // wire form rather than the `{secs, nanos}` auto-derive.
    #[serde(with = "humantime_serde")]
    #[schemars(with = "String")]
    pub polling_interval: Duration,
}

/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParsingConfig {
    pub rules: Vec<ParsingRule>,
    pub case_sensitive: bool,
}

/// `deny_unknown_fields` so typoed or removed keys fail loudly at load
/// instead of being silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParsingRule {
    #[serde(rename = "type")]
    pub rule_type: RuleType,
    pub pattern: String,
    pub safe: bool,
}

// Unit-variant-only enum deserialized from a bare string (e.g. `"contains"`),
// not a JSON object — `deny_unknown_fields` has no meaningful effect here, so
// it is intentionally omitted.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuleType {
    Contains,
    Regex,
}

/// The packaged first-start default watch path, under the service's
/// platform-dependent state directory: the packaged unit's `StateDirectory`
/// on Unix, `%PROGRAMDATA%\rusty-photon\filemonitor\` on Windows (ADR-015).
/// A placeholder either way — the operator points it at the real roof-status
/// file.
#[cfg(not(windows))]
fn default_watch_path() -> PathBuf {
    PathBuf::from("/var/lib/rusty-photon/filemonitor/RoofStatusFile.txt")
}
#[cfg(windows)]
fn default_watch_path() -> PathBuf {
    program_data_root(std::env::var_os("ProgramData"))
        .join("rusty-photon")
        .join("filemonitor")
        .join("RoofStatusFile.txt")
}

/// Pure resolution of the Windows `ProgramData` root from the value of the
/// `ProgramData` environment variable: the value verbatim when present and
/// non-empty, else the fixed `C:\ProgramData` fallback. A private copy of the
/// same rule `rusty-photon-config` applies to the config path (each crate
/// keeps its own — see the W2 note in `docs/plans/windows-packaging.md`);
/// compiled on Windows and in test builds on every platform, so the logic
/// is unit-testable on non-Windows hosts.
#[cfg(any(windows, test))]
fn program_data_root(program_data: Option<std::ffi::OsString>) -> PathBuf {
    match program_data {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(r"C:\ProgramData"),
    }
}

impl Default for Config {
    /// The packaged first-start default: watch a roof-status file under the
    /// service's state directory and fail safe (no readable status = unsafe).
    fn default() -> Self {
        Self {
            device: DeviceConfig {
                name: "File Safety Monitor".to_string(),
                unique_id: "filemonitor-001".to_string(),
                description: "ASCOM Alpaca SafetyMonitor that monitors file content".to_string(),
            },
            file: FileConfig {
                path: default_watch_path(),
                polling_interval: Duration::from_mins(1),
            },
            parsing: ParsingConfig {
                rules: vec![
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "CLOSED".to_string(),
                        safe: true,
                    },
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "OPEN".to_string(),
                        safe: false,
                    },
                    ParsingRule {
                        rule_type: RuleType::Regex,
                        pattern: r"Status:\s*(SAFE|OK)".to_string(),
                        safe: true,
                    },
                ],
                case_sensitive: false,
            },
            server: AlpacaServerConfig::new(11111),
        }
    }
}

#[derive(derive_more::Debug)]
pub struct FileMonitorDevice {
    config: Config,
    connected: Arc<RwLock<bool>>,
    last_content: Arc<Mutex<Option<String>>>,
    polling_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// `Some` when the driver was built with a config source (the normal path
    /// through `ServerBuilder`); `None` for focused unit-test devices that
    /// don't exercise config actions.
    #[debug(skip)]
    config_ctx: Option<ConfigActionCtx<FileMonitorDriver>>,
}

impl FileMonitorDevice {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            connected: Arc::new(RwLock::new(false)),
            last_content: Arc::new(Mutex::new(None)),
            polling_handle: Arc::new(Mutex::new(None)),
            config_ctx: None,
        }
    }

    /// Attach the config-action context, enabling `config.get` / `config.apply`.
    #[must_use]
    pub fn with_config_actions(mut self, ctx: ConfigActionCtx<FileMonitorDriver>) -> Self {
        self.config_ctx = Some(ctx);
        self
    }

    #[must_use]
    pub fn evaluate_safety(&self, content: &str) -> bool {
        for rule in &self.config.parsing.rules {
            let matches = match rule.rule_type {
                RuleType::Contains => {
                    if self.config.parsing.case_sensitive {
                        content.contains(&rule.pattern)
                    } else {
                        content
                            .to_lowercase()
                            .contains(&rule.pattern.to_lowercase())
                    }
                }
                RuleType::Regex => {
                    let pattern = if self.config.parsing.case_sensitive {
                        rule.pattern.clone()
                    } else {
                        format!("(?i){}", rule.pattern)
                    };

                    // Invalid regex patterns don't match.
                    regex::Regex::new(&pattern).is_ok_and(|re| re.is_match(content))
                }
            };

            if matches {
                return rule.safe;
            }
        }

        false
    }

    fn read_file(&self) -> Result<String, std::io::Error> {
        std::fs::read_to_string(&self.config.file.path)
    }

    async fn start_polling(&self) {
        let config = self.config.clone();
        let last_content = Arc::clone(&self.last_content);
        let connected = Arc::clone(&self.connected);

        let handle = tokio::spawn(async move {
            let mut interval = interval(config.file.polling_interval);

            loop {
                interval.tick().await;

                // Check if still connected
                if !*connected.read().await {
                    break;
                }

                // Read and store file content
                if let Ok(content) = std::fs::read_to_string(&config.file.path) {
                    let mut last = last_content.lock().await;
                    *last = Some(content);
                }
            }
        });

        let mut polling_handle = self.polling_handle.lock().await;
        *polling_handle = Some(handle);
    }

    async fn stop_polling(&self) {
        let mut handle = self.polling_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
    }
}

#[async_trait::async_trait]
impl Device for FileMonitorDevice {
    fn static_name(&self) -> &str {
        &self.config.device.name
    }

    fn unique_id(&self) -> &str {
        &self.config.device.unique_id
    }

    async fn description(&self) -> ASCOMResult<String> {
        Ok(self.config.device.description.clone())
    }

    async fn connected(&self) -> ASCOMResult<bool> {
        Ok(*self.connected.read().await)
    }

    async fn set_connected(&self, connected: bool) -> Result<(), ASCOMError> {
        if connected {
            // Reload the file
            match self.read_file() {
                Ok(content) => {
                    let mut last_content = self.last_content.lock().await;
                    *last_content = Some(content);
                }
                Err(e) => {
                    return Err(ASCOMError::new(
                        ASCOMErrorCode::NOT_CONNECTED,
                        format!("Failed to read file: {e}"),
                    ));
                }
            }

            // Set connected state
            let mut conn_state = self.connected.write().await;
            *conn_state = true;
            drop(conn_state);

            // Start polling
            self.start_polling().await;
        } else {
            // Set disconnected state
            let mut conn_state = self.connected.write().await;
            *conn_state = false;
            drop(conn_state);

            // Stop polling
            self.stop_polling().await;
        }

        Ok(())
    }

    async fn driver_info(&self) -> ASCOMResult<String> {
        Ok(self.config.device.description.clone())
    }

    async fn driver_version(&self) -> ASCOMResult<String> {
        Ok("0.1.0".to_string())
    }

    async fn supported_actions(&self) -> ASCOMResult<Vec<String>> {
        Ok(rusty_photon_driver::supported_actions(&self.config_ctx))
    }

    async fn action(&self, action: String, parameters: String) -> ASCOMResult<String> {
        rusty_photon_driver::dispatch::<FileMonitorDriver>(&self.config_ctx, action, parameters)
            .await
    }
}

#[async_trait::async_trait]
impl SafetyMonitor for FileMonitorDevice {
    async fn is_safe(&self) -> ASCOMResult<bool> {
        if !*self.connected.read().await {
            return Err(ASCOMError::NOT_CONNECTED);
        }

        let last_content = self.last_content.lock().await;
        Ok(last_content
            .as_ref()
            .is_some_and(|content| self.evaluate_safety(content)))
    }
}

/// Load a [`Config`] from the JSON file at `path`.
///
/// # Errors
///
/// Returns an error if the file cannot be read or does not parse as a
/// [`Config`].
pub fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = serde_json::from_str(&content)?;
    Ok(config)
}

/// Builder for the ASCOM Alpaca filemonitor server.
///
/// The returned [`BoundServer`] can be inspected (e.g. `listen_addr()`)
/// before calling `start()`.
pub struct ServerBuilder {
    config: Config,
    /// Where `config.apply` persists and reload re-reads. `Some` enables the
    /// config actions (together with `reload`).
    config_path: Option<PathBuf>,
    /// Reload trigger handed to the device for fire-after-response reload.
    reload: Option<ReloadSignal>,
}

impl ServerBuilder {
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            config_path: None,
            reload: None,
        }
    }

    /// Set the config source (persist path) for the `config.get` /
    /// `config.apply` actions. Together with [`Self::with_reload_signal`],
    /// this enables config editing.
    #[must_use]
    pub fn with_config_source(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Provide the reload trigger `config.apply` fires after its response
    /// flushes. Together with [`Self::with_config_source`], this enables
    /// config editing.
    #[must_use]
    pub fn with_reload_signal(mut self, reload: ReloadSignal) -> Self {
        self.reload = Some(reload);
        self
    }

    /// Consume the builder and bind the configured listen address.
    ///
    /// # Errors
    ///
    /// Returns an error if the listener cannot be bound or its address
    /// read, or if the config opts into Alpaca discovery and the
    /// responder's UDP port cannot be bound.
    pub async fn build(self) -> Result<BoundServer, Box<dyn std::error::Error + Send + Sync>> {
        let mut device = FileMonitorDevice::new(self.config.clone());
        let config_ctx: Option<ConfigActionCtx<FileMonitorDriver>> =
            match (self.config_path.clone(), self.reload.clone()) {
                (Some(path), Some(reload)) => Some(ConfigActionCtx {
                    effective: self.config.clone(),
                    path,
                    overrides: (),
                    reload,
                }),
                _ => None,
            };
        if let Some(ctx) = config_ctx {
            device = device.with_config_actions(ctx);
        }

        let mut server = Server::new(CargoServerInfo!());
        server.listen_addr = self.config.server.socket_addr();
        server.devices.register(device);

        info!(
            "Starting ASCOM Alpaca server on port {}",
            self.config.server.port
        );
        info!(
            "Device: {} ({})",
            self.config.device.name, self.config.device.unique_id
        );
        info!("Monitoring file: {:?}", self.config.file.path);

        let tls = self.config.server.tls.clone();
        let router = axum::Router::new().fallback_service(server.into_service());

        // Layer authentication if configured
        let router = match &self.config.server.auth {
            Some(auth) => {
                if self.config.server.tls.is_none() {
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

        let listener =
            rusty_photon_tls::server::bind_dual_stack_tokio(self.config.server.socket_addr())
                .await?;
        let local_addr = listener.local_addr()?;

        // Opt-in Alpaca UDP discovery responder (config `discovery_port`);
        // bound here so a taken port fails startup, run in start().
        let discovery =
            rusty_photon_driver::discovery::bind(local_addr, self.config.server.discovery_port)
                .await?;

        // Console mode only: stdout is a dead handle under the Windows SCM,
        // and the only stdout consumer (bdd-infra's port parser) never runs
        // services with --service.
        if !rusty_photon_service_lifecycle::is_scm_service() {
            println!("Bound Alpaca server bound_addr={local_addr}");
        }

        Ok(BoundServer {
            listener,
            router,
            local_addr,
            tls,
            discovery,
        })
    }
}

/// A fully bound filemonitor server ready to accept connections.
pub struct BoundServer {
    listener: tokio::net::TcpListener,
    router: axum::Router,
    local_addr: SocketAddr,
    tls: Option<TlsConfig>,
    /// Alpaca UDP discovery responder, when the config opts in. Runs inside
    /// `start()`'s select so its socket closes when serving ends (reload).
    discovery: Option<ascom_alpaca::discovery::BoundDiscoveryServer>,
}

impl BoundServer {
    pub const fn listen_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve the bound listener (and the discovery responder, when
    /// bound) until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS material cannot be loaded or the
    /// serve loop fails.
    pub async fn start(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Self {
            listener,
            router,
            local_addr,
            tls,
            discovery,
        } = self;
        let serve = async {
            if let Some(ref tls_config) = tls {
                info!("filemonitor started on {} (TLS)", local_addr);
                rusty_photon_tls::server::serve_tls(listener, router, tls_config, shutdown).await
            } else {
                info!("filemonitor started on {}", local_addr);
                rusty_photon_tls::server::serve_plain(listener, router, shutdown).await
            }
        };
        rusty_photon_driver::discovery::serve_with(discovery, serve).await?;
        debug!("filemonitor shut down");
        Ok(())
    }
}

/// Build a fresh `BoundServer` from a `Config` and run it until the
/// `shutdown` future resolves.
///
/// The drain semantics are whatever
/// `rusty_photon_tls::server::serve_plain` / `serve_tls` provide for
/// graceful shutdown.
///
/// # Errors
///
/// Returns [`ServerBuilder::build`]'s bind and discovery failures, or
/// [`BoundServer::start`]'s TLS and serve failures.
pub async fn start_server(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ServerBuilder::new(config)
        .build()
        .await?
        .start(shutdown)
        .await
}

/// Run filemonitor's server in a config-reload loop until `shutdown`
/// fires.
///
/// Both `shutdown` and a `config.apply`-fired `reload` are fed into the
/// *same* stop future passed to `bound.start()`, so either one drives
/// the inner `serve_plain`/`serve_tls` call's graceful shutdown —
/// draining in-flight requests and closing keep-alive connections —
/// before `run_server_loop` rebuilds from the freshly-persisted config.
/// Reload needs the same graceful-drain treatment as shutdown, or a
/// client's pooled keep-alive connection would keep talking to the
/// torn-down server's in-memory state instead of picking up the rebound
/// one.
///
/// # Errors
///
/// Returns an error if a config load, a rebuild's bind, or the serve
/// loop fails; a failure on any reload iteration ends the loop.
pub async fn run_server_loop(
    config_path: &Path,
    shutdown: CancellationToken,
    reload: ReloadSignal,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let config = load_config(config_path)?;
        info!("Starting filemonitor server on port {}", config.server.port);
        let bound = ServerBuilder::new(config)
            .with_config_source(config_path.to_path_buf())
            .with_reload_signal(reload.clone())
            .build()
            .await?;

        let reloaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = {
            let reloaded = Arc::clone(&reloaded);
            let shutdown = shutdown.clone().cancelled_owned();
            let reload = reload.clone();
            async move {
                tokio::select! {
                    () = shutdown => {}
                    () = reload.recv() => reloaded.store(true, std::sync::atomic::Ordering::SeqCst),
                }
            }
        };
        bound.start(stop).await?;

        if reloaded.load(std::sync::atomic::Ordering::SeqCst) {
            info!("Reloading configuration");
            continue;
        }
        return Ok(());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod device_config_action_tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            device: DeviceConfig {
                name: "Test Monitor".to_string(),
                unique_id: "filemonitor-test-id".to_string(),
                description: "Test".to_string(),
            },
            file: FileConfig {
                path: PathBuf::from("/tmp/RoofStatusFile.txt"),
                polling_interval: Duration::from_mins(1),
            },
            parsing: ParsingConfig {
                rules: vec![ParsingRule {
                    rule_type: RuleType::Contains,
                    pattern: "OPEN".to_string(),
                    safe: false,
                }],
                case_sensitive: false,
            },
            server: AlpacaServerConfig::new(11111),
        }
    }

    /// Build a device wired with a config-action context backed by a temp file
    /// pre-seeded with `effective`. Returns the reload handle (clone) so tests
    /// can assert the fire-after-response reload, and the `TempDir`/path.
    fn device_with_config_actions(
        effective: Config,
    ) -> (FileMonitorDevice, ReloadSignal, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filemonitor.json");
        std::fs::write(&path, serde_json::to_string(&effective).unwrap()).unwrap();
        let reload = ReloadSignal::new();
        let device =
            FileMonitorDevice::new(effective.clone()).with_config_actions(ConfigActionCtx {
                effective,
                path: path.clone(),
                overrides: (),
                reload: reload.clone(),
            });
        (device, reload, dir, path)
    }

    #[tokio::test]
    async fn supported_actions_lists_config_actions_when_configured() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let actions = device.supported_actions().await.unwrap();
        assert!(actions.contains(&"config.get".to_string()));
        assert!(actions.contains(&"config.apply".to_string()));
        assert!(actions.contains(&"config.schema".to_string()));
    }

    #[tokio::test]
    async fn supported_actions_empty_without_config_ctx() {
        let device = FileMonitorDevice::new(test_config());
        assert!(device.supported_actions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn config_get_returns_effective_config_and_overrides() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        // Config actions must work while disconnected.
        assert!(!device.connected().await.unwrap());
        let body = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value
                .pointer("/config/device/unique_id")
                .and_then(|v| v.as_str()),
            Some("filemonitor-test-id")
        );
        assert!(value
            .get("overrides")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn config_get_redacts_password_hash() {
        let mut effective = test_config();
        effective.server.auth = Some(rp_auth::config::AuthConfig {
            username: "obs".to_string(),
            password_hash: "$argon2id$v=19$real".to_string(),
        });
        let (device, _reload, _dir, _path) = device_with_config_actions(effective);
        let body = device
            .action("config.get".to_string(), String::new())
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value
                .pointer("/config/server/auth/password_hash")
                .and_then(|v| v.as_str()),
            Some(config_actions::REDACTED)
        );
    }

    #[tokio::test]
    async fn config_apply_persists_and_fires_reload() {
        let (device, reload, _dir, path) = device_with_config_actions(test_config());
        let mut changed = test_config();
        changed.file.polling_interval = Duration::from_mins(2);
        let params = serde_json::to_string(&changed).unwrap();

        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value.get("status").and_then(|v| v.as_str()),
            Some("applying")
        );
        let reload_paths = value.get("reload").and_then(|v| v.as_array()).unwrap();
        assert!(reload_paths
            .iter()
            .any(|p| p.as_str() == Some("file.polling_interval")));

        // Persisted to disk with the new value (humantime_serde normalizes
        // "120s" to "2m").
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.pointer("/file/polling_interval").unwrap(), "2m");

        // The reload is fired after the response — it must arrive.
        tokio::time::timeout(std::time::Duration::from_secs(2), reload.recv())
            .await
            .expect("config.apply should fire the reload");
    }

    #[tokio::test]
    async fn config_apply_persists_port_and_interval_together() {
        let (device, _reload, _dir, path) = device_with_config_actions(test_config());
        let mut changed = test_config();
        changed.server.port = 12345;
        changed.file.polling_interval = Duration::from_secs(45);
        let params = serde_json::to_string(&changed).unwrap();

        device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(persisted.pointer("/server/port").unwrap(), 12345);
        assert_eq!(persisted.pointer("/file/polling_interval").unwrap(), "45s");
    }

    #[tokio::test]
    async fn config_apply_without_change_returns_ok() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let params = serde_json::to_string(&test_config()).unwrap();
        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value.get("status").and_then(|v| v.as_str()), Some("ok"));
    }

    #[tokio::test]
    async fn config_apply_invalid_leaves_file_unchanged() {
        let (device, _reload, _dir, path) = device_with_config_actions(test_config());
        let before = std::fs::read_to_string(&path).unwrap();
        let mut bad = test_config();
        bad.device.unique_id = String::new(); // fails validation
        let params = serde_json::to_string(&bad).unwrap();

        let body = device
            .action("config.apply".to_string(), params)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value.get("status").and_then(|v| v.as_str()),
            Some("invalid")
        );
        assert!(!value
            .get("errors")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn config_apply_rejects_non_json() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let err = device
            .action("config.apply".to_string(), "not json".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::INVALID_VALUE);
    }

    #[tokio::test]
    async fn unknown_action_returns_action_not_implemented() {
        let (device, _reload, _dir, _path) = device_with_config_actions(test_config());
        let err = device
            .action("config.frobnicate".to_string(), String::new())
            .await
            .unwrap_err();
        assert_eq!(err.code, ASCOMErrorCode::ACTION_NOT_IMPLEMENTED);
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn create_case_insensitive_config() -> Config {
        Config {
            device: DeviceConfig {
                name: "Test Device".to_string(),
                unique_id: "test-123".to_string(),
                description: "Test Description".to_string(),
            },
            file: FileConfig {
                path: PathBuf::from("/tmp/test.txt"),
                polling_interval: Duration::from_secs(1),
            },
            parsing: ParsingConfig {
                rules: vec![
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "SAFE".to_string(),
                        safe: true,
                    },
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "DANGER".to_string(),
                        safe: false,
                    },
                ],
                case_sensitive: false,
            },
            server: AlpacaServerConfig::new(8080),
        }
    }

    fn create_test_config() -> Config {
        Config {
            device: DeviceConfig {
                name: "Test Device".to_string(),
                unique_id: "test-123".to_string(),
                description: "Test Description".to_string(),
            },
            file: FileConfig {
                path: PathBuf::from("/tmp/test.txt"),
                polling_interval: Duration::from_secs(1),
            },
            parsing: ParsingConfig {
                rules: vec![
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "SAFE".to_string(),
                        safe: true,
                    },
                    ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "DANGER".to_string(),
                        safe: false,
                    },
                ],
                case_sensitive: true,
            },
            server: AlpacaServerConfig::new(8080),
        }
    }

    proptest! {
        #[test]
        fn test_safety_evaluation_consistency(content in ".*") {
            let config = create_test_config();
            let device = FileMonitorDevice::new(config);

            // Safety evaluation should be deterministic
            let result1 = device.evaluate_safety(&content);
            let result2 = device.evaluate_safety(&content);
            prop_assert_eq!(result1, result2);
        }

        #[test]
        fn test_safe_content_always_safe(safe_suffix in ".*") {
            let content = format!("SAFE {safe_suffix}");
            let config = create_test_config();
            let device = FileMonitorDevice::new(config);

            prop_assert!(device.evaluate_safety(&content));
        }

        #[test]
        fn test_danger_content_always_unsafe(danger_suffix in ".*") {
            let content = format!("DANGER {danger_suffix}");
            let config = create_test_config();
            let device = FileMonitorDevice::new(config);

            prop_assert!(!device.evaluate_safety(&content));
        }

        #[test]
        fn test_regex_pattern_consistency(
            pattern in "[a-zA-Z0-9]+",
            content in ".*"
        ) {
            let config = Config {
                device: DeviceConfig {
                    name: "Test".to_string(),
                    unique_id: "test".to_string(),
                    description: "Test".to_string(),
                },
                file: FileConfig {
                    path: PathBuf::from("/tmp/test.txt"),
                    polling_interval: Duration::from_secs(1),
                },
                parsing: ParsingConfig {
                    rules: vec![ParsingRule {
                        rule_type: RuleType::Regex,
                        pattern,
                        safe: true,
                    }],
                    case_sensitive: true,
                },
                server: AlpacaServerConfig::new(8080),
            };

            let device = FileMonitorDevice::new(config);

            // Should not panic on any input
            let _result = device.evaluate_safety(&content);
        }

        #[test]
        fn test_case_insensitive_contains_matches_any_case(
            prefix in "[a-zA-Z ]{0,10}",
            suffix in "[a-zA-Z ]{0,10}"
        ) {
            let config = create_case_insensitive_config();
            let device = FileMonitorDevice::new(config);

            // "safe" in any case should match
            let content = format!("{prefix}safe{suffix}");
            prop_assert!(device.evaluate_safety(&content));

            let content_upper = format!("{prefix}SAFE{suffix}");
            prop_assert!(device.evaluate_safety(&content_upper));

            let content_mixed = format!("{prefix}SaFe{suffix}");
            prop_assert!(device.evaluate_safety(&content_mixed));
        }

        #[test]
        fn test_case_insensitive_regex_consistency(
            pattern in "[a-zA-Z0-9]+",
            content in ".*"
        ) {
            let config = Config {
                device: DeviceConfig {
                    name: "Test".to_string(),
                    unique_id: "test".to_string(),
                    description: "Test".to_string(),
                },
                file: FileConfig {
                    path: PathBuf::from("/tmp/test.txt"),
                    polling_interval: Duration::from_secs(1),
                },
                parsing: ParsingConfig {
                    rules: vec![ParsingRule {
                        rule_type: RuleType::Regex,
                        pattern,
                        safe: true,
                    }],
                    case_sensitive: false,
                },
                server: AlpacaServerConfig::new(8080),
            };

            let device = FileMonitorDevice::new(config);

            // Should not panic and should be deterministic
            let result1 = device.evaluate_safety(&content);
            let result2 = device.evaluate_safety(&content);
            prop_assert_eq!(result1, result2);
        }

        #[test]
        fn test_config_round_trip_serialization(
            name in "[a-zA-Z ]{1,20}",
            unique_id in "[a-z0-9-]{1,20}",
            port in 1024u16..65535u16,
            polling in 1u64..3600u64,
        ) {
            let config = Config {
                device: DeviceConfig {
                    name,
                    unique_id,
                    description: "Test Description".to_string(),
                },
                file: FileConfig {
                    path: PathBuf::from("/tmp/test.txt"),
                    polling_interval: Duration::from_secs(polling),
                },
                parsing: ParsingConfig {
                    rules: vec![ParsingRule {
                        rule_type: RuleType::Contains,
                        pattern: "TEST".to_string(),
                        safe: true,
                    }],
                    case_sensitive: false,
                },
                server: AlpacaServerConfig::new(port),
            };

            let json = serde_json::to_string(&config).unwrap();
            let deserialized: Config = serde_json::from_str(&json).unwrap();

            prop_assert_eq!(config.device.name, deserialized.device.name);
            prop_assert_eq!(config.device.unique_id, deserialized.device.unique_id);
            prop_assert_eq!(config.server.port, deserialized.server.port);
            prop_assert_eq!(config.file.polling_interval, deserialized.file.polling_interval);
            prop_assert_eq!(config.parsing.rules.len(), deserialized.parsing.rules.len());
        }

        #[test]
        fn test_invalid_regex_never_panics(
            pattern in ".*",
            content in ".*"
        ) {
            let config = Config {
                device: DeviceConfig {
                    name: "Test".to_string(),
                    unique_id: "test".to_string(),
                    description: "Test".to_string(),
                },
                file: FileConfig {
                    path: PathBuf::from("/tmp/test.txt"),
                    polling_interval: Duration::from_secs(1),
                },
                parsing: ParsingConfig {
                    rules: vec![ParsingRule {
                        rule_type: RuleType::Regex,
                        pattern,
                        safe: true,
                    }],
                    case_sensitive: false,
                },
                server: AlpacaServerConfig::new(8080),
            };

            let device = FileMonitorDevice::new(config);
            // Should never panic, even with arbitrary regex patterns
            let _result = device.evaluate_safety(&content);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod default_config_tests {
    use super::*;

    #[test]
    fn program_data_root_uses_env_value_verbatim() {
        let root = program_data_root(Some(std::ffi::OsString::from(r"D:\CustomData")));
        assert_eq!(root, PathBuf::from(r"D:\CustomData"));
    }

    #[test]
    fn program_data_root_falls_back_when_env_absent() {
        assert_eq!(program_data_root(None), PathBuf::from(r"C:\ProgramData"));
    }

    #[test]
    fn program_data_root_falls_back_when_env_empty() {
        assert_eq!(
            program_data_root(Some(std::ffi::OsString::new())),
            PathBuf::from(r"C:\ProgramData")
        );
    }

    #[test]
    fn default_watch_path_is_platform_dependent() {
        let config = Config::default();
        #[cfg(not(windows))]
        assert_eq!(
            config.file.path,
            PathBuf::from("/var/lib/rusty-photon/filemonitor/RoofStatusFile.txt")
        );
        #[cfg(windows)]
        assert!(
            config
                .file
                .path
                .ends_with(r"rusty-photon\filemonitor\RoofStatusFile.txt"),
            "{:?}",
            config.file.path
        );
    }

    #[test]
    fn a_typoed_top_level_field_is_rejected_loudly() {
        let err = serde_json::from_str::<Config>(r#"{"typoed_key": 1}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("typoed_key"), "{err}");
    }

    #[test]
    fn a_typoed_device_field_is_rejected_loudly() {
        let err = serde_json::from_str::<DeviceConfig>(r#"{"nmae": "oops"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nmae"), "{err}");
    }

    #[test]
    fn a_typoed_file_field_is_rejected_loudly() {
        let err = serde_json::from_str::<FileConfig>(r#"{"paht": "/tmp/x.txt"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("paht"), "{err}");
    }

    #[test]
    fn a_typoed_parsing_field_is_rejected_loudly() {
        let err = serde_json::from_str::<ParsingConfig>(r#"{"rulez": []}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("rulez"), "{err}");
    }

    #[test]
    fn a_typoed_parsing_rule_field_is_rejected_loudly() {
        let err = serde_json::from_str::<ParsingRule>(
            r#"{"type": "contains", "paterns": "OPEN", "safe": true}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("paterns"), "{err}");
    }

    #[test]
    fn default_server_config_is_plain_http_on_all_interfaces() {
        let c = Config::default();
        assert_eq!(c.server.port, 11111);
        assert_eq!(c.server.bind_address.to_string(), "0.0.0.0");
        assert!(c.server.discovery_port.is_none());
        assert!(c.server.tls.is_none());
        assert!(c.server.auth.is_none());
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod doctor_toml_parity {
    use rusty_photon_server_config::doctor_toml::{parse, ServerClass};

    use super::Config;

    /// `pkg/doctor.toml` is this service's catalog entry for
    /// `rusty-photon-doctor` and must match the config defaults
    /// (docs/services/doctor.md §The derived catalog).
    #[test]
    fn pkg_doctor_toml_matches_config_defaults() {
        let meta = parse(include_str!("../pkg/doctor.toml")).unwrap();
        assert_eq!(meta.port, Config::default().server.port);
        assert_eq!(meta.class, ServerClass::Alpaca);
    }
}
